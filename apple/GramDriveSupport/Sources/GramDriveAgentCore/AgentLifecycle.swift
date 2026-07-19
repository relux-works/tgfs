import Foundation
import GramDriveCore
import GramDriveSupport

/// Configuration of one agent run.
public struct AgentConfiguration {
    /// The data root the agent coordinates (the same root every GramDrive
    /// process derives from the App Group container; substitute roots for
    /// tests and the smoke).
    public var dataRoot: URL
    /// How long a drain waits for in-flight transfers to finish before
    /// cancelling them.
    public var drainGracePeriod: Duration
    /// How long a drain waits for cancellations to land before reporting
    /// the stragglers abandoned.
    public var drainCancelWait: Duration
    /// Power-event source; `nil` disables power observation (tests that
    /// need none, tools).
    public var powerEvents: (any PowerEventSource)?

    public init(
        dataRoot: URL,
        drainGracePeriod: Duration = .seconds(10),
        drainCancelWait: Duration = .seconds(5),
        powerEvents: (any PowerEventSource)? = nil
    ) {
        self.dataRoot = dataRoot
        self.drainGracePeriod = drainGracePeriod
        self.drainCancelWait = drainCancelWait
        self.powerEvents = powerEvents
    }
}

/// Why the agent is shutting down; recorded in the event log so the next
/// run's diagnostics show how the previous one ended.
public enum ShutdownReason: String, Sendable {
    /// SIGTERM or equivalent: launchd unload, logout, app-requested stop.
    case terminate
    /// The user is logging out of the account; the engine-side secure
    /// wipe (SEC-004) is owned by the auth/logout work — the agent's part
    /// is draining transfers and stopping cleanly first.
    case logout
    /// The app is replacing the agent with a newer version.
    case update
}

/// Why the agent could not start.
public enum AgentStartError: Error {
    /// Another agent holds the single-instance lock over this container.
    case alreadyRunning(path: String)
    /// Shared state could not be opened, even after the coordinator's
    /// corruption-recovery path.
    case storage(detail: String)
    /// The agent runtime directory could not be prepared.
    case runtimeDirectory(underlying: Error)
    /// The health endpoint could not be established.
    case healthEndpoint(underlying: Error)
}

/// The companion agent's lifecycle: the one coordinator process per shared
/// container that hosts the engine (PLAT-MAC-002).
///
/// `launching → recovering → running → draining → stopped`, with the
/// contractual properties on the transitions:
///
/// - **single instance** — an exclusive `flock` taken before anything
///   else; a second agent fails fast with ``AgentStartError/alreadyRunning(path:)``
///   and touches nothing;
/// - **recovery without duplicate work** — startup opens the *durable*
///   state (quarantining a corrupt database first, coordinator-only);
///   in-flight bookkeeping is process-local and dies with a crash, so a
///   restart resumes from durable state instead of replaying anything.
///   The kernel releases a dead agent's flock, so crash recovery starts
///   immediately;
/// - **clean shutdown** — ``shutdown(reason:)`` stops admitting work,
///   drains in-flight transfers (grace, then cancellation through their
///   FFI tokens), and only then tears the endpoint and lock down;
/// - **health** — ``healthSnapshot()`` at any time, served over the
///   bounded IPC channel while the agent runs — including during a drain,
///   so the app can watch a shutdown make progress.
///
/// Accounts: the lifecycle is account-agnostic by design. Accounts are
/// rows inside the shared database (`AccountScope` in the identity model);
/// one agent hosts every account of the container, so "multiple accounts"
/// never means multiple agents racing over one container (PRD-001's design
/// path).
public final class AgentLifecycle: @unchecked Sendable {
    private let lock = NSLock()
    private let configuration: AgentConfiguration
    private let layout: AgentRuntimeLayout
    private let startedAt: Date

    private var state: AgentRunState = .launching
    private var instanceLock: SingleInstanceLock?
    private var healthServer: AgentHealthServer?
    private var powerObservation: PowerEventObservation?
    private var settings: AgentSettings?
    private var events: [String] = []
    private var lastSleepMs: Int64?
    private var lastWakeMs: Int64?
    private var lastKnownDataVersion: Int64?

    /// The in-flight transfer ledger; operations hosted by this agent
    /// register here so shutdown can drain them.
    public let transfers = TransferRegistry()

    /// The shared durable state, open as coordinator while the agent runs.
    public private(set) var store: SharedStateStore?

    /// The drive core handle hosted by this agent.
    public private(set) var core: DriveCore?

    /// How many recent events a snapshot carries.
    private static let eventWindow = 16

    public init(configuration: AgentConfiguration, now: Date = Date()) {
        self.configuration = configuration
        self.layout = AgentRuntimeLayout(dataRoot: configuration.dataRoot)
        self.startedAt = now
    }

    /// The lifecycle state, for hosts and tests.
    public var currentState: AgentRunState {
        lock.lock()
        defer { lock.unlock() }
        return state
    }

    /// The agent runtime layout in use.
    public var runtimeLayout: AgentRuntimeLayout { layout }

    /// Brings the agent up: lock, durable state (with corruption
    /// recovery), core, health endpoint, power observation.
    ///
    /// Synchronous by design — launchd started this process to do exactly
    /// this, and nothing may proceed on a half-started agent.
    public func start() throws {
        do {
            try layout.ensureDirectories()
        } catch {
            throw AgentStartError.runtimeDirectory(underlying: error)
        }

        // Settings are advisory; unreadable settings must not keep the
        // engine host down. The failure is recorded, not swallowed.
        let settingsStore = AgentSettingsStore(fileURL: layout.settingsFile)
        do {
            let loaded = try settingsStore.load()
            setLocked { self.settings = loaded }
        } catch {
            setLocked { self.settings = nil }
            record("settings-unreadable")
        }

        // Single instance before any shared resource is touched.
        do {
            let acquired = try SingleInstanceLock.acquire(at: layout.lockFile)
            setLocked { self.instanceLock = acquired }
        } catch SingleInstanceLockError.alreadyHeld(let path) {
            throw AgentStartError.alreadyRunning(path: path)
        } catch let error as SingleInstanceLockError {
            throw AgentStartError.storage(detail: "instance lock: \(error)")
        }

        // Startup reconciliation: open durable state; a corrupt database
        // is quarantined (coordinator-only right) and the open retried
        // once against the cleared path.
        setLocked { self.state = .recovering }
        do {
            let opened = try openStoreWithRecovery()
            let version = try? opened.dataVersion()
            setLocked {
                self.store = opened
                self.lastKnownDataVersion = version
            }
        } catch {
            teardownAfterFailedStart()
            throw AgentStartError.storage(detail: redactedDetail(of: error))
        }

        do {
            let core = try DriveCore(
                config: CoreConfig(dataDir: configuration.dataRoot.path))
            setLocked { self.core = core }
        } catch {
            teardownAfterFailedStart()
            throw AgentStartError.storage(detail: redactedDetail(of: error))
        }

        do {
            let server = try AgentHealthServer.start(
                socketURL: layout.healthSocket
            ) { [weak self] in
                self?.healthSnapshot() ?? Self.placeholderSnapshot()
            }
            setLocked { self.healthServer = server }
        } catch {
            teardownAfterFailedStart()
            throw AgentStartError.healthEndpoint(underlying: error)
        }

        if let source = configuration.powerEvents {
            let observation = source.observe { [weak self] event in
                self?.handle(power: event)
            }
            setLocked { self.powerObservation = observation }
        }

        setLocked { self.state = .running }
        record("started")
    }

    /// Drains and stops the agent. Safe to call once per run; the health
    /// endpoint stays up *through* the drain so its progress is
    /// observable, then everything tears down in reverse start order.
    @discardableResult
    public func shutdown(reason: ShutdownReason) async -> DrainOutcome {
        setLocked { self.state = .draining }
        record("draining:\(reason.rawValue)")

        let outcome = await transfers.drain(
            gracePeriod: configuration.drainGracePeriod,
            cancelWait: configuration.drainCancelWait)
        if outcome.abandoned > 0 {
            record("drain-abandoned:\(outcome.abandoned)")
        }

        let (observation, server, instance) = releaseResourcesAndStop()
        observation?.cancel()
        server?.stop()
        instance?.release()
        record("stopped:\(reason.rawValue)")
        return outcome
    }

    /// The current health/status report (NFR-032 shape; see
    /// ``AgentHealthSnapshot`` for which fields are wired today).
    public func healthSnapshot() -> AgentHealthSnapshot {
        lock.lock()
        let state = self.state
        let settings = self.settings
        let store = self.store
        let events = Array(self.events.suffix(Self.eventWindow))
        let lastSleepMs = self.lastSleepMs
        let lastWakeMs = self.lastWakeMs
        lock.unlock()

        let contract = contractVersion()
        return AgentHealthSnapshot(
            payloadVersion: 1,
            agentVersion: AgentVersion.current,
            contractVersion: "\(contract.major).\(contract.minor).\(contract.patch)",
            pid: ProcessInfo.processInfo.processIdentifier,
            state: state,
            startedAtMs: Int64((startedAt.timeIntervalSince1970 * 1000).rounded()),
            launchAtLogin: settings?.launchAtLogin,
            stateSchemaVersion: store.flatMap { try? $0.schemaVersion() },
            dataVersion: store.flatMap { try? $0.dataVersion() },
            pendingTransferCount: transfers.pendingCount,
            lastSourceUpdateMs: nil,
            changeCursor: nil,
            cachePressure: nil,
            providerRegistrationState: nil,
            lastSleepMs: lastSleepMs,
            lastWakeMs: lastWakeMs,
            recentEvents: events)
    }

    // MARK: - Internals

    /// Synchronous teardown bookkeeping for ``shutdown(reason:)`` —
    /// `NSLock` may not be taken from an async context, so the locked
    /// extraction lives in this sync helper.
    private func releaseResourcesAndStop() -> (
        PowerEventObservation?, AgentHealthServer?, SingleInstanceLock?
    ) {
        lock.lock()
        defer { lock.unlock() }
        let resources = (powerObservation, healthServer, instanceLock)
        powerObservation = nil
        healthServer = nil
        instanceLock = nil
        store = nil
        core = nil
        state = .stopped
        return resources
    }

    private func openStoreWithRecovery() throws -> SharedStateStore {
        do {
            return try SharedState.open(dataRoot: configuration.dataRoot, role: .coordinator)
        } catch let DriveError.Storage(detail) {
            // The coordinator's recovery right: quarantine only what
            // SQLite itself reports corrupt (the core re-probes before
            // touching anything), then retry the cleared path once.
            _ = detail
            let quarantined = try quarantineCorruptState(
                dataRoot: configuration.dataRoot.path, role: .coordinator)
            record(quarantined == nil ? "state-open-retry" : "state-quarantined")
            return try SharedState.open(dataRoot: configuration.dataRoot, role: .coordinator)
        }
    }

    private func handle(power event: PowerEvent) {
        let nowMs = Int64((Date().timeIntervalSince1970 * 1000).rounded())
        switch event {
        case .willSleep:
            setLocked { self.lastSleepMs = nowMs }
            record("sleep")
        case .didWake:
            // A doorbell rung while this process slept is gone (Darwin
            // notifications do not queue); wake is therefore a mandatory
            // re-probe point.
            let store = setLockedReturning { self.store }
            let version = store.flatMap { try? $0.dataVersion() }
            setLocked {
                self.lastWakeMs = nowMs
                if version != nil, version != self.lastKnownDataVersion {
                    self.lastKnownDataVersion = version
                }
            }
            record("wake")
        }
    }

    /// Appends one event to the redacted ring. Vocabulary is fixed by the
    /// call sites — codes only, never user data, paths, or account
    /// material (NFR-032).
    private func record(_ code: String) {
        lock.lock()
        defer { lock.unlock() }
        events.append(code)
        if events.count > Self.eventWindow * 4 {
            events.removeFirst(events.count - Self.eventWindow * 4)
        }
    }

    /// Reduces a start-path error to its stable category, dropping the
    /// diagnostic detail (which may carry paths) from anything that could
    /// reach a health consumer.
    private func redactedDetail(of error: Error) -> String {
        switch error {
        case let driveError as DriveError:
            switch driveError {
            case .InvalidArgument: return "invalid-argument"
            case .NotFound: return "not-found"
            case .AuthRequired: return "auth-required"
            case .RateLimited: return "rate-limited"
            case .SourceUnavailable: return "source-unavailable"
            case .Storage: return "storage"
            case .Integrity: return "integrity"
            case .Cancelled: return "cancelled"
            case .Internal: return "internal"
            }
        default:
            return "unknown"
        }
    }

    private func teardownAfterFailedStart() {
        lock.lock()
        let server = healthServer
        let instance = instanceLock
        healthServer = nil
        instanceLock = nil
        store = nil
        core = nil
        state = .stopped
        lock.unlock()
        server?.stop()
        instance?.release()
    }

    private func setLocked(_ mutate: () -> Void) {
        lock.lock()
        defer { lock.unlock() }
        mutate()
    }

    private func setLockedReturning<T>(_ read: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return read()
    }

    private static func placeholderSnapshot() -> AgentHealthSnapshot {
        AgentHealthSnapshot(
            payloadVersion: 1,
            agentVersion: AgentVersion.current,
            contractVersion: "",
            pid: ProcessInfo.processInfo.processIdentifier,
            state: .stopped,
            startedAtMs: 0,
            launchAtLogin: nil,
            stateSchemaVersion: nil,
            dataVersion: nil,
            pendingTransferCount: 0,
            lastSourceUpdateMs: nil,
            changeCursor: nil,
            cachePressure: nil,
            providerRegistrationState: nil,
            lastSleepMs: nil,
            lastWakeMs: nil,
            recentEvents: [])
    }
}
