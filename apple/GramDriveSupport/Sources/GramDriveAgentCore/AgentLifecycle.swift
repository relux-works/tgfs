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
    /// Optional transfer-engine bridge override for tests. The production
    /// lifecycle derives the state-backed bridge from its `DriveCore`.
    public var hydrator: (any ContentHydrating)?
    /// The engine-backed seams behind the control endpoint (sign-in,
    /// removal, repair, content policy). The endpoint itself always runs — status and
    /// settings are lifecycle-owned — and a missing seam answers its
    /// command with a truthful `sourceUnavailable`; the shipped agent
    /// wires all three (`AgentMain`).
    public var controlSeams: AgentControlSeams
    /// Long-lived Telegram namespace and normalized-content owner. The shipped
    /// agent supplies the core-backed implementation; tests may inject a fake.
    public var namespaceBootstrapper: (any AgentNamespaceBootstrapping)?
    /// Delay before recreating a post-ready namespace whose source owner
    /// stopped with a retryable source failure. Only one recovery is scheduled
    /// per account at a time.
    public var namespaceRecoveryDelay: Duration
    /// Called after the control server has acknowledged a termination request.
    /// The executable host owns process exit; the lifecycle owns the drain.
    public var onTerminationAccepted: (@Sendable (ControlTerminationRequest) -> Void)?
    /// Atomically claims a request-correlated ready drain before the control
    /// server reports an irreversible commit acceptance.
    public var onTerminationCommitAccepted: (@Sendable (ControlTerminationRequest) -> Bool)?
    /// Invoked after the commit acceptance was written; the executable host
    /// then finishes teardown and exits.
    public var onTerminationCommitAcknowledged: (@Sendable (ControlTerminationRequest) -> Void)?
    /// A prepared update drain may never become irreversible without a
    /// companion commit. This lease bounds how long the agent can remain
    /// unavailable if the companion or its control reply disappears.
    public var terminationCommitLease: Duration

    public init(
        dataRoot: URL,
        drainGracePeriod: Duration = .seconds(10),
        drainCancelWait: Duration = .seconds(5),
        powerEvents: (any PowerEventSource)? = nil,
        hydrator: (any ContentHydrating)? = nil,
        controlSeams: AgentControlSeams = AgentControlSeams(),
        namespaceBootstrapper: (any AgentNamespaceBootstrapping)? = nil,
        namespaceRecoveryDelay: Duration = .seconds(1),
        onTerminationAccepted: (@Sendable (ControlTerminationRequest) -> Void)? = nil,
        onTerminationCommitAccepted: (@Sendable (ControlTerminationRequest) -> Bool)? = nil,
        onTerminationCommitAcknowledged: (@Sendable (ControlTerminationRequest) -> Void)? = nil,
        terminationCommitLease: Duration = .seconds(5)
    ) {
        self.dataRoot = dataRoot
        self.drainGracePeriod = drainGracePeriod
        self.drainCancelWait = drainCancelWait
        self.powerEvents = powerEvents
        self.hydrator = hydrator
        self.controlSeams = controlSeams
        self.namespaceBootstrapper = namespaceBootstrapper
        self.namespaceRecoveryDelay = namespaceRecoveryDelay
        self.onTerminationAccepted = onTerminationAccepted
        self.onTerminationCommitAccepted = onTerminationCommitAccepted
        self.onTerminationCommitAcknowledged = onTerminationCommitAcknowledged
        self.terminationCommitLease = terminationCommitLease
    }
}

/// The engine-backed command seams the agent host composes into its
/// control endpoint.
public struct AgentControlSeams: Sendable {
    public var authorizer: (any AgentAuthorizing)?
    public var remover: (any AgentAccountRemoving)?
    public var repairer: (any AgentRepairing)?
    public var contentPolicy: (any AgentContentPolicyControlling)?

    public init(
        authorizer: (any AgentAuthorizing)? = nil,
        remover: (any AgentAccountRemoving)? = nil,
        repairer: (any AgentRepairing)? = nil,
        contentPolicy: (any AgentContentPolicyControlling)? = nil
    ) {
        self.authorizer = authorizer
        self.remover = remover
        self.repairer = repairer
        self.contentPolicy = contentPolicy
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
    /// The hydration endpoint could not be established.
    case hydrationEndpoint(underlying: Error)
    /// The control endpoint could not be established.
    case controlEndpoint(underlying: Error)
}

private struct OwnedNamespaceSession: @unchecked Sendable {
    let token: UUID
    let session: any AgentNamespaceSessionHosting
}

enum NamespaceReadinessDisposition: Equatable {
    case preparing
    case usable(degradation: AgentActionableFailure?)
    case failed(AgentActionableFailure)
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
    private let processIdentity: AgentProcessIdentity
    private let authDiagnostics: AuthDiagnosticTrail

    private var state: AgentRunState = .launching
    private var instanceLock: SingleInstanceLock?
    private var healthServer: AgentHealthServer?
    private var hydrationServer: HydrationServer?
    private var controlServer: ControlServer?
    private var powerObservation: PowerEventObservation?
    private var settings: AgentSettings?
    private var events: [String] = []
    private var lastSleepMs: Int64?
    private var lastWakeMs: Int64?
    private var lastKnownDataVersion: Int64?
    private var finderContentFailure: AgentActionableFailure?
    private var namespaceSessions: [Int64: OwnedNamespaceSession] = [:]
    private var namespaceProgress: [Int64: AgentNamespaceProgress] = [:]
    private var namespaceTokens: [Int64: UUID] = [:]
    private var namespaceReadyAccounts: Set<Int64> = []
    private var namespaceDegradations: [Int64: AgentActionableFailure] = [:]
    private var namespaceRecoveryScheduled: Set<Int64> = []
    /// Consecutive recovery attempts per account since the last `ready`. Drives
    /// the recovery backoff so a deterministically failing namespace cannot
    /// become a restart loop.
    private var namespaceRecoveryAttempts: [Int64: Int] = [:]
    /// Foreground history hints this agent has been handed, by kind. Counts
    /// only — the chat a hint names never reaches health (BUG-260728-2qfzbd).
    private var historyPriorityHints = HistoryPriorityHintCounts()
    private var shutdownTask: Task<DrainOutcome, Never>?
    private var terminationRequestID: UUID?
    private var terminationRequest: ControlTerminationRequest?
    private var terminationCancellationRequested = false
    private var suspendedTerminationNamespaces: Set<Int64> = []
    private var terminationLeaseTask: Task<Void, Never>?
    /// Once a commit wins, cancellation and the ready lease are no longer
    /// permitted to restore the process. Resources deliberately remain owned
    /// until the host has acknowledged the claim and starts its short teardown;
    /// publishing a live `.stopped` endpoint would otherwise let the companion
    /// confuse an in-progress teardown with process death.
    private var terminationCommitClaimed = false
    /// Changes only after a complete rollback has reopened transfer admission
    /// and recreated the namespace owners. The companion requires it together
    /// with endpoint probes before replying `false` to AppKit.
    private var servingGeneration: UInt64 = 0

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
        layout = AgentRuntimeLayout(dataRoot: configuration.dataRoot)
        startedAt = now
        processIdentity = AgentProcessIdentity.current()
        authDiagnostics = AuthDiagnosticTrail(fileURL: layout.authDiagnosticsFile)
    }

    /// The lifecycle state, for hosts and tests.
    public var currentState: AgentRunState {
        lock.lock()
        defer { lock.unlock() }
        return state
    }

    /// The agent runtime layout in use.
    public var runtimeLayout: AgentRuntimeLayout {
        layout
    }

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
        setLocked { self.events = self.authDiagnostics.restore() }

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
        } catch let SingleInstanceLockError.alreadyHeld(path) {
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
            do {
                _ = try opened.ensureRootStructure()
                setLocked { self.finderContentFailure = nil }
                record("root-structure-ready")
            } catch {
                setLocked {
                    self.finderContentFailure = AgentActionableFailure(
                        category: "storage",
                        message: "Finder structure is unavailable. Relaunch GramDrive to retry.",
                        retryable: true
                    )
                }
                record("root-structure-failed")
            }
        } catch {
            teardownAfterFailedStart()
            throw AgentStartError.storage(detail: redactedDetail(of: error))
        }

        let contentHydrator: any ContentHydrating
        do {
            let core = try DriveCore(
                config: CoreConfig(dataDir: configuration.dataRoot.path)
            )
            setLocked { self.core = core }
            if let injected = configuration.hydrator {
                contentHydrator = injected
            } else {
                contentHydrator = try CoreContentHydrator(hydrator: core.hydrator())
            }
        } catch {
            teardownAfterFailedStart()
            throw AgentStartError.storage(detail: redactedDetail(of: error))
        }

        // The control endpoint runs unconditionally: status and settings
        // are lifecycle-owned, and each engine-backed seam answers its own
        // absence truthfully (BUG-260720-3i74u1). It is established before
        // health is published: the companion uses a successful health read as
        // the process-readiness barrier, so exposing health first would let a
        // clean first launch race the still-missing control socket.
        do {
            let seams = configuration.controlSeams
            let server = try ControlServer.start(
                socketURL: layout.controlSocket,
                handlers: ControlServerHandlers(
                    status: { [weak self] in
                        self?.healthSnapshot() ?? Self.placeholderSnapshot()
                    },
                    reloadSettings: { [weak self] in
                        guard let self else { return AgentSettings() }
                        return try self.reloadSettings()
                    },
                    authorizer: seams.authorizer,
                    authDiagnostics: { [weak self] code in
                        self?.recordAuthDiagnostic(code)
                    },
                    remover: seams.remover,
                    repairer: seams.repairer,
                    contentPolicy: seams.contentPolicy,
                    historyPriority: { [weak self] request in
                        guard let self else {
                            return .failed(
                                ControlCommandFailure(
                                    category: .sourceUnavailable,
                                    detail: "agent lifecycle unavailable"
                                )
                            )
                        }
                        do {
                            let priority: AgentChatHistoryPriority =
                                switch request.priority {
                                case .background: .background
                                case .requested: .requested
                                case .visible: .visible
                                }
                            guard
                                try self.setChatHistoryPriority(
                                    accountId: request.accountId,
                                    chatId: request.chatId,
                                    priority: priority
                                )
                            else {
                                return .failed(
                                    ControlCommandFailure(
                                        category: .notFound,
                                        detail: "owned namespace unavailable"
                                    )
                                )
                            }
                            return .completed
                        } catch {
                            return .failed(ControlServer.failure(from: error))
                        }
                    },
                    providerFetchHealth: { [weak self] report in
                        guard let self else {
                            return .failed(
                                ControlCommandFailure(
                                    category: .sourceUnavailable,
                                    detail: "agent lifecycle unavailable"
                                )
                            )
                        }
                        do {
                            try self.recordProviderFetchHealth(report)
                            return .completed
                        } catch {
                            return .failed(ControlServer.failure(from: error))
                        }
                    },
                    prepareForTermination: configuration.onTerminationAccepted,
                    canPrepareTermination: { [weak self] request in
                        self?.canBeginTermination(request) ?? false
                    },
                    acceptTerminationCommit: configuration.onTerminationCommitAccepted,
                    finishAcceptedTerminationCommit: configuration.onTerminationCommitAcknowledged
                )
            )
            setLocked { self.controlServer = server }
        } catch {
            teardownAfterFailedStart()
            throw AgentStartError.controlEndpoint(underlying: error)
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

        do {
            let server = try HydrationServer.start(
                socketURL: layout.hydrationSocket,
                registry: transfers,
                admission: { [weak self] request in
                    self?.admitHydration(request)
                        ?? .refuse(
                            HydrationFailure(
                                category: .draining, detail: "agent is gone"
                            )
                        )
                },
                hydrator: contentHydrator
            )
            setLocked { self.hydrationServer = server }
        } catch {
            teardownAfterFailedStart()
            throw AgentStartError.hydrationEndpoint(underlying: error)
        }

        if let source = configuration.powerEvents {
            let observation = source.observe { [weak self] event in
                self?.handle(power: event)
            }
            setLocked { self.powerObservation = observation }
        }

        setLocked {
            self.state = .running
            self.servingGeneration += 1
        }
        startAuthorizedNamespaces()
        record("started")
    }

    /// Drains and stops the agent. Safe to call once per run; the health
    /// endpoint stays up *through* the drain so its progress is
    /// observable, then everything tears down in reverse start order.
    @discardableResult
    public func shutdown(reason: ShutdownReason) async -> DrainOutcome {
        let task = setLockedReturning { () -> Task<DrainOutcome, Never> in
            if let shutdownTask { return shutdownTask }
            self.state = .draining
            let task = Task { [weak self] in
                guard let self else { return DrainOutcome(completed: 0, cancelled: 0, abandoned: 0) }
                return await self.performShutdown(reason: reason)
            }
            shutdownTask = task
            return task
        }
        return await task.value
    }

    /// Starts the request-correlated companion drain before its control socket
    /// is closed. A lost acknowledgement is subsequently observable in health.
    public func canBeginTermination(_ request: ControlTerminationRequest) -> Bool {
        guard request.action == .prepare else { return false }
        guard processIdentity.isValidTerminationIdentity,
              request.expectedAgentInstanceID == processIdentity.instanceID
        else { return false }
        return setLockedReturning {
            shutdownTask == nil && (state == .running || state == .terminationCancelled)
        }
    }

    /// Starts the request-correlated companion drain before its control socket
    /// is closed. A lost acknowledgement is subsequently observable in health.
    public func beginTermination(_ request: ControlTerminationRequest) {
        guard canBeginTermination(request) else { return }
        let shouldStart = setLockedReturning { () -> Bool in
            guard shutdownTask == nil, state == .running || state == .terminationCancelled else {
                return false
            }
            terminationRequestID = request.requestID
            terminationRequest = request
            terminationCancellationRequested = false
            terminationCommitClaimed = false
            state = .draining
            return true
        }
        guard shouldStart else { return }
        let reason: ShutdownReason = request.reason == .update ? .update : .terminate
        Task { _ = await self.shutdown(reason: reason) }
    }

    /// Cancels only the matching in-flight drain. The recovery is applied at
    /// the bounded drain boundary, never concurrently with resource teardown.
    public func cancelTermination(_ request: ControlTerminationRequest) {
        guard request.action == .cancel else { return }
        guard processIdentity.isValidTerminationIdentity,
              request.expectedAgentInstanceID == processIdentity.instanceID
        else { return }
        let readyNamespaces = setLockedReturning { () -> Set<Int64>? in
            guard self.terminationRequestID == request.requestID else { return nil }
            // A successful commit claim is the irreversible boundary. The host
            // watchdog will terminate this exact process even if the subsequent
            // ordinary teardown wedges, so cancellation must not report a false
            // reply after this point.
            guard !self.terminationCommitClaimed else { return nil }
            guard self.state == .terminationReady else {
                self.terminationCancellationRequested = true
                return nil
            }
            self.terminationLeaseTask?.cancel()
            self.terminationLeaseTask = nil
            return self.suspendedTerminationNamespaces
        }
        if let readyNamespaces {
            rollbackTermination(namespaces: readyNamespaces)
        }
    }

    /// Claims the irreversible commit while the host arms its process-owned
    /// watchdog under the same lifecycle lock. A watchdog-arm failure leaves
    /// the ready lease intact, so cancellation/lease rollback remains legal.
    public func acceptTerminationCommit(
        _ request: ControlTerminationRequest,
        armWatchdog: () -> Bool
    ) -> Bool {
        guard request.action == .commit else { return false }
        return claimTerminationCommit(request, armWatchdog: armWatchdog)
    }

    /// Begins the non-blocking committed-exit sequence. Every potentially
    /// stallable operation (transfer and hydration drain) has already happened
    /// before the ready state. This intentionally retains the durable store,
    /// core, endpoint owners, and flock until `_exit` lets the kernel tear down
    /// the whole process atomically.
    @discardableResult
    public func finishAcceptedTerminationCommit(_ request: ControlTerminationRequest) -> Bool {
        guard setLockedReturning({
            request.action == .commit
                && terminationRequestID == request.requestID
                && terminationCommitClaimed
        }) else { return false }
        // There must never be a live `.stopped` health response: it would be an
        // unprovable intermediate result. Stop accepting new IPC synchronously;
        // retained references and the instance lock stay owned until process
        // death, including watchdog-forced `_exit`.
        let (observation, server, hydration, control) = setLockedReturning {
            (powerObservation, healthServer, hydrationServer, controlServer)
        }
        observation?.cancel()
        control?.stop()
        hydration?.stop()
        server?.stop()
        record("committed-exit")
        return true
    }

    private func performShutdown(reason: ShutdownReason) async -> DrainOutcome {
        record("draining:\(reason.rawValue)")
        // A cancellation must restore the same namespace owners that were serving
        // before the drain. Durable authorization remains the source of truth for
        // a normal restart, while this snapshot also covers an owner that was
        // already active when shutdown began.
        let suspendedNamespaceAccounts = setLockedReturning {
            Set(namespaceSessions.keys)
        }
        setLocked { self.suspendedTerminationNamespaces = suspendedNamespaceAccounts }
        stopAllNamespaces()

        let outcome = await transfers.drain(
            gracePeriod: configuration.drainGracePeriod,
            cancelWait: configuration.drainCancelWait
        )
        let cancellationRequested = setLockedReturning { terminationCancellationRequested }
        if outcome.abandoned > 0 || cancellationRequested {
            if outcome.abandoned > 0 {
                record("drain-abandoned:\(outcome.abandoned)")
            }
            rollbackTermination(namespaces: suspendedNamespaceAccounts)
            return outcome
        }

        if let request = setLockedReturning({ self.terminationRequest }) {
            // The completed transfer drain is intentionally reversible until the
            // companion has observed this request-correlated state and sent commit.
            // If that companion or its response path vanishes, the lease below
            // restores admission and namespace owners instead of permitting a late
            // helper exit after AppKit has replied false.
            setLocked { self.state = .terminationReady }
            scheduleTerminationLease(for: request)
            record("termination-ready")
            return outcome
        }

        let (observation, server, hydration, control, instance) = releaseResourcesAndStop()
        observation?.cancel()
        control?.stop()
        // Generated paths are retained through ordinary materialization EOF. The
        // lifecycle still bounds a wedged peer; its receiver-owned descriptor
        // keeps any already-started clone valid after the server-side force-close.
        await hydration?.stopAndDrain(timeout: configuration.drainCancelWait)
        server?.stop()
        instance?.release()
        record("stopped:\(reason.rawValue)")
        return outcome
    }

    private func scheduleTerminationLease(for request: ControlTerminationRequest) {
        let lease = configuration.terminationCommitLease
        let task = Task { [weak self] in
            try? await Task.sleep(for: lease)
            guard !Task.isCancelled else { return }
            var cancellation = request
            cancellation.action = .cancel
            self?.cancelTermination(cancellation)
        }
        setLocked {
            self.terminationLeaseTask?.cancel()
            self.terminationLeaseTask = task
        }
    }

    /// Restores the current executable to a usable serving state before
    /// publishing the cancellation health. Namespace start has an intentional
    /// `.running` guard, so that ordering is part of the recovery invariant.
    private func rollbackTermination(namespaces: Set<Int64>) {
        setLocked {
            self.state = .running
            self.shutdownTask = nil
            self.terminationCancellationRequested = false
            self.terminationLeaseTask?.cancel()
            self.terminationLeaseTask = nil
            self.terminationCommitClaimed = false
        }
        transfers.resumeAdmission()
        restartNamespaces(including: namespaces)
        setLocked {
            self.servingGeneration += 1
            self.state = .terminationCancelled
        }
        record("termination-cancelled")
    }

    /// Re-reads the durable settings document and applies it to the
    /// running agent — the control channel's settings-reload command, so a
    /// companion save takes effect without an agent restart.
    public func reloadSettings() throws -> AgentSettings {
        let loaded = try AgentSettingsStore(fileURL: layout.settingsFile).load()
        setLocked { self.settings = loaded }
        record("settings-reloaded")
        return loaded
    }

    /// The current health/status report (NFR-032 shape; see
    /// ``AgentHealthSnapshot`` for which fields are wired today).
    public func healthSnapshot() -> AgentHealthSnapshot {
        lock.lock()
        let state = self.state
        let terminationRequestID = self.terminationRequestID
        let settings = self.settings
        let store = self.store
        let events = Array(self.events.suffix(Self.eventWindow))
        let lastSleepMs = self.lastSleepMs
        let lastWakeMs = self.lastWakeMs
        let configuredFinderFailure = finderContentFailure
        let namespaceProgress = self.namespaceProgress
        let namespaceReadyAccounts = self.namespaceReadyAccounts
        let namespaceDegradations = self.namespaceDegradations
        let hasNamespaceBootstrapper = configuration.namespaceBootstrapper != nil
        let namespaceOwnerCount = namespaceSessions.count
        let namespaceOwnerIDs = Set(namespaceSessions.keys)
        let suspendedTerminationNamespaces = self.suspendedTerminationNamespaces
        let servingGeneration = self.servingGeneration
        let historyPriorityHints = self.historyPriorityHints
        lock.unlock()

        let contract = contractVersion()
        let accountRecords = store.flatMap { try? $0.accounts() }
        let finderHealth: (
            state: FinderContentState, itemCount: Int?, failure: AgentActionableFailure?,
            degradation: AgentActionableFailure?
        )
        #if DEBUG
            if AgentRuntimeTestOverrides.finderHierarchyReady {
                finderHealth = (.ready, 0, nil, nil)
            } else {
                finderHealth = Self.finderContentHealth(
                    store: store,
                    accounts: accountRecords,
                    configuredFailure: configuredFinderFailure,
                    namespaceProgress: namespaceProgress,
                    namespaceReadyAccounts: namespaceReadyAccounts,
                    namespaceDegradations: namespaceDegradations,
                    hasNamespaceBootstrapper: hasNamespaceBootstrapper
                )
            }
        #else
            finderHealth = Self.finderContentHealth(
                store: store,
                accounts: accountRecords,
                configuredFailure: configuredFinderFailure,
                namespaceProgress: namespaceProgress,
                namespaceReadyAccounts: namespaceReadyAccounts,
                namespaceDegradations: namespaceDegradations,
                hasNamespaceBootstrapper: hasNamespaceBootstrapper
            )
        #endif
        let providerFetchHealth = store.flatMap { try? $0.providerFetchHealth() }.map {
            ProviderFetchHealthCounts(
                callbacks: $0.callbackCount,
                succeeded: $0.successCount,
                engineFailures: $0.engineFailureCount,
                providerMappings: $0.providerMappingCount,
                noSuchItem: $0.noSuchItemCount,
                retryable: $0.retryableCount
            )
        }
        let authorizedAccountCount = accountRecords?.filter { $0.authState == "authorized" }.count ?? 0
        let namespaceOwnersRestored = !hasNamespaceBootstrapper
            || (
                namespaceOwnerCount >= authorizedAccountCount
                    && suspendedTerminationNamespaces.isSubset(of: namespaceOwnerIDs)
            )
        return AgentHealthSnapshot(
            payloadVersion: 4,
            agentVersion: AgentVersion.current,
            bundleVersion: AgentBuildVersion.current,
            contractVersion: "\(contract.major).\(contract.minor).\(contract.patch)",
            pid: ProcessInfo.processInfo.processIdentifier,
            processIdentity: processIdentity,
            state: state,
            terminationRequestID: terminationRequestID,
            servingGeneration: servingGeneration,
            transferAdmissionOpen: transfers.isAcceptingNewWork,
            namespaceOwnersRestored: namespaceOwnersRestored,
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
            recentEvents: events,
            accounts: accountRecords.map { accounts in
                accounts.map { account in
                    AccountHealthSummary(
                        accountId: account.accountId,
                        displayName: account.displayName,
                        authState: account.authState
                    )
                }
            },
            finderContentState: finderHealth.state,
            finderFirstPageItemCount: finderHealth.itemCount,
            finderContentFailure: finderHealth.failure,
            finderSourceDegradation: finderHealth.degradation,
            historyPriorityHints: historyPriorityHints,
            providerFetchHealth: providerFetchHealth
        )
    }

    /// Records a fixed installed-auth diagnostic code in the health ring,
    /// durable trail, and unified log. This is the only auth diagnostics
    /// ingress: callers cannot pass user-controlled strings.
    public func recordAuthDiagnostic(_ code: AuthDiagnosticCode) {
        authDiagnostics.record(code)
        record(code.rawValue)
    }

    // MARK: - Internals

    private static func finderContentHealth(
        store: SharedStateStore?,
        accounts: [AccountInfo]?,
        configuredFailure: AgentActionableFailure?,
        namespaceProgress: [Int64: AgentNamespaceProgress],
        namespaceReadyAccounts: Set<Int64>,
        namespaceDegradations: [Int64: AgentActionableFailure],
        hasNamespaceBootstrapper: Bool
    ) -> (
        state: FinderContentState, itemCount: Int?, failure: AgentActionableFailure?,
        degradation: AgentActionableFailure?
    ) {
        if let configuredFailure {
            return (.failed, nil, configuredFailure, nil)
        }
        guard let store, let accounts else {
            return (
                .failed, nil,
                AgentActionableFailure(
                    category: "storage",
                    message: "Account state is unavailable. Relaunch GramDrive to retry.",
                    retryable: true
                ),
                nil
            )
        }
        let authorized = accounts.filter { $0.authState == "authorized" }
        guard !authorized.isEmpty else {
            return (.waitingForAuthorization, 0, nil, nil)
        }
        var degradation = authorized.compactMap {
            namespaceDegradations[$0.accountId]
        }.first
        if hasNamespaceBootstrapper {
            for account in authorized {
                switch Self.namespaceReadinessDisposition(
                    progress: namespaceProgress[account.accountId] ?? .preparing,
                    hasReachedReady: namespaceReadyAccounts.contains(account.accountId),
                    existingDegradation: namespaceDegradations[account.accountId]
                ) {
                case .preparing:
                    return (.preparing, 0, nil, nil)
                case let .usable(accountDegradation):
                    degradation = degradation ?? accountDegradation
                case let .failed(failure):
                    return (.failed, nil, failure, nil)
                }
            }
        }
        do {
            for account in authorized {
                let firstPage = try store.children(
                    parent: account.rootItemId, after: nil,
                    limit: 256
                )
                guard !firstPage.isEmpty else { return (.preparing, 0, nil, degradation) }
            }
            return (.ready, authorized.count * 3, nil, degradation)
        } catch {
            return (
                .failed, nil,
                AgentActionableFailure(
                    category: "storage",
                    message: "Finder structure could not be read. Relaunch GramDrive to retry.",
                    retryable: true
                ),
                nil
            )
        }
    }

    /// Whether a stopped namespace owner should be recreated.
    ///
    /// The core states retryability per failure, and that statement is the
    /// whole answer: an allow-list of *categories* on this side silently
    /// turned every retryable storage, projection, and render failure into a
    /// permanent stop. On a real preserved profile that meant one transient
    /// write failure ended history backfill for the rest of the process's
    /// life, with the agent still running, still holding the domain, and
    /// reporting a failure whose own message said "relaunch to retry"
    /// (BUG-260728-2qfzbd). A category list can only ever be a list of the
    /// failures somebody happened to think of.
    ///
    /// Only `retryable: false` — an expired authorization, a structurally
    /// impossible projection — is terminal, because recreating the owner
    /// would meet exactly the same wall. Repetition is bounded by
    /// ``namespaceRecoveryDelay(attempt:)``, not by refusing to try.
    static func isRecoverableSourceFailure(category _: String, retryable: Bool) -> Bool {
        retryable
    }

    /// The delay before the `attempt`-th consecutive recovery of one account,
    /// doubling from ``AgentConfiguration/namespaceRecoveryDelay`` up to
    /// ``maxNamespaceRecoveryDelay``.
    ///
    /// A deterministic failure — one that recurs at the same point of every
    /// startup — would otherwise become an unbounded restart loop, replaying
    /// the whole snapshot cycle every second. Backing off keeps a transient
    /// failure cheap to recover from and a permanent one cheap to survive;
    /// the counter resets the moment the namespace reports ready again.
    func namespaceRecoveryDelay(attempt: Int) -> Duration {
        let base = configuration.namespaceRecoveryDelay
        let cap = Self.maxNamespaceRecoveryDelay
        guard attempt > 1, base > .zero else { return base }
        var delay = base
        for _ in 1 ..< attempt {
            delay += delay
            if delay >= cap { return cap }
        }
        return delay
    }

    /// The ceiling on namespace recovery backoff. Long enough that a
    /// permanently failing account costs nothing, short enough that a machine
    /// which recovers (disk freed, lock released) resumes without the user
    /// having to relaunch anything.
    static let maxNamespaceRecoveryDelay: Duration = .seconds(300)

    static func namespaceReadinessDisposition(
        progress: AgentNamespaceProgress,
        hasReachedReady: Bool,
        existingDegradation: AgentActionableFailure? = nil
    ) -> NamespaceReadinessDisposition {
        let failure: (String, Bool) -> AgentActionableFailure = { category, retryable in
            AgentActionableFailure(
                category: category,
                message: Self.namespaceFailureMessage(category: category),
                retryable: retryable
            )
        }
        switch progress {
        case .ready:
            return .usable(degradation: nil)
        case .preparing, .stopped:
            return hasReachedReady ? .usable(degradation: existingDegradation) : .preparing
        case let .degraded(category, retryable), let .failed(category, retryable):
            let sourceFailure = failure(category, retryable)
            guard
                hasReachedReady,
                Self.isRecoverableSourceFailure(category: category, retryable: retryable)
            else {
                return .failed(sourceFailure)
            }
            return .usable(degradation: sourceFailure)
        }
    }

    private static func namespaceFailureMessage(category: String) -> String {
        switch category {
        case "auth-required":
            return "Telegram authorization expired. Sign in again to retry."
        case "rate-limited":
            return "Telegram asked GramDrive to wait. Retry shortly."
        case "storage":
            return "Finder metadata could not be saved. GramDrive is retrying."
        case "source-unavailable":
            return "Telegram is unavailable. Check the connection and retry."
        case "integrity":
            return "Local Telegram state could not be verified. Run Repair and retry."
        default:
            return "Telegram metadata is unavailable. Check the connection and retry."
        }
    }

    /// Stops one account owner before re-authorization or removal. Safe to
    /// call for an absent account and safe against late callbacks from the
    /// session being closed.
    public func stopNamespace(accountId: Int64) {
        let owned = setLockedReturning { () -> OwnedNamespaceSession? in
            let owned = namespaceSessions.removeValue(forKey: accountId)
            namespaceProgress.removeValue(forKey: accountId)
            namespaceTokens.removeValue(forKey: accountId)
            namespaceReadyAccounts.remove(accountId)
            namespaceDegradations.removeValue(forKey: accountId)
            namespaceRecoveryScheduled.remove(accountId)
            namespaceRecoveryAttempts.removeValue(forKey: accountId)
            return owned
        }
        owned?.session.close()
    }

    /// Stops every TDLib namespace before repair, shutdown, or sign-in.
    public func stopAllNamespaces() {
        let owned = setLockedReturning { () -> [OwnedNamespaceSession] in
            let owned = Array(namespaceSessions.values)
            namespaceSessions.removeAll()
            namespaceProgress.removeAll()
            namespaceTokens.removeAll()
            namespaceReadyAccounts.removeAll()
            namespaceDegradations.removeAll()
            namespaceRecoveryScheduled.removeAll()
            namespaceRecoveryAttempts.removeAll()
            return owned
        }
        for entry in owned {
            entry.session.close()
        }
    }

    /// Re-discovers authorized accounts and recreates their namespace owners.
    public func restartNamespaces(including suspendedAccounts: Set<Int64> = []) {
        stopAllNamespaces()
        startAuthorizedNamespaces()
        for accountId in suspendedAccounts {
            startNamespace(accountId: accountId)
        }
    }

    private func startAuthorizedNamespaces() {
        guard let bootstrapper = configuration.namespaceBootstrapper else { return }
        let accounts = setLockedReturning { self.store }.flatMap { try? $0.accounts() } ?? []
        for account in accounts where account.authState == "authorized" {
            startNamespace(accountId: account.accountId, bootstrapper: bootstrapper)
        }
    }

    /// Starts one namespace through the configured owner. Kept internal so
    /// lifecycle tests can exercise relaunch/failure behavior without a real
    /// Telegram account; production discovers accounts from durable state.
    func startNamespace(accountId: Int64) {
        guard let bootstrapper = configuration.namespaceBootstrapper else { return }
        startNamespace(accountId: accountId, bootstrapper: bootstrapper)
    }

    func namespaceStatus(accountId: Int64) -> AgentNamespaceProgress? {
        setLockedReturning { namespaceProgress[accountId] }
    }

    /// Signals foreground history demand to the already-owned account
    /// session. This only updates the Rust scheduler queue; the agent worker
    /// remains the sole TDLib client owner.
    @discardableResult
    public func setChatHistoryPriority(
        accountId: Int64,
        chatId: Int64,
        priority: AgentChatHistoryPriority
    ) throws -> Bool {
        // Counted before the routing check, because "no hint ever arrived" and
        // "a hint arrived for an account with no session" are different faults
        // and only the count at this end can tell them apart on an installed
        // build (BUG-260728-2qfzbd).
        let arrivedAtMs = Int64((Date().timeIntervalSince1970 * 1000).rounded())
        guard let session = setLockedReturning({ namespaceSessions[accountId]?.session }) else {
            setLocked {
                historyPriorityHints.unroutable += 1
                historyPriorityHints.lastAtMs = arrivedAtMs
            }
            return false
        }
        try session.setChatHistoryPriority(chatId: chatId, priority: priority)
        setLocked {
            historyPriorityHints.accepted += 1
            historyPriorityHints.lastAtMs = arrivedAtMs
            switch priority {
            case .visible: historyPriorityHints.visible += 1
            case .requested: historyPriorityHints.requested += 1
            case .background: historyPriorityHints.background += 1
            }
        }
        return true
    }

    /// Persists aggregate File Provider callback health through the agent-owned
    /// store. The report has no item or account identity, so this introduces no
    /// activity trail in durable state.
    public func recordProviderFetchHealth(_ report: ProviderFetchHealthReport) throws {
        guard let store = setLockedReturning({ self.store }) else {
            throw AgentStartError.storage(detail: "provider health store unavailable")
        }
        try store.recordProviderFetchHealth(
            observation: ProviderFetchHealthObservation(
                succeeded: report.succeeded,
                engineFailure: report.engineFailure,
                providerMapping: report.providerMapping,
                noSuchItem: report.noSuchItem,
                retryable: report.retryable,
                observedAtMs: report.observedAtMs
            )
        )
    }

    private func startNamespace(
        accountId: Int64,
        bootstrapper: any AgentNamespaceBootstrapping
    ) {
        guard
            setLockedReturning({
                state == .running && namespaceSessions[accountId] == nil
            })
        else { return }
        let token = UUID()
        setLocked {
            namespaceProgress[accountId] = .preparing
            namespaceTokens[accountId] = token
        }
        do {
            let session = try bootstrapper.start(accountId: accountId) { [weak self] progress in
                self?.receiveNamespaceProgress(progress, accountId: accountId, token: token)
            }
            let accepted = setLockedReturning { () -> Bool in
                guard namespaceSessions[accountId] == nil else { return false }
                namespaceSessions[accountId] = OwnedNamespaceSession(token: token, session: session)
                return true
            }
            if !accepted { session.close() }
            record("namespace-started")
        } catch {
            let category = namespaceStartFailureCategory(error)
            receiveNamespaceProgress(
                .failed(category: category, retryable: true), accountId: accountId, token: token
            )
            record("namespace-start-failed")
        }
    }

    /// Keeps synchronous owner-construction failures actionable without
    /// allowing diagnostic detail (paths, keychain status, or source data) into
    /// the health payload. Unknown foreign errors retain the historical generic
    /// `source` category.
    private func namespaceStartFailureCategory(_ error: Error) -> String {
        let category = redactedDetail(of: error)
        return category == "unknown" ? "source" : category
    }

    private func receiveNamespaceProgress(
        _ progress: AgentNamespaceProgress,
        accountId: Int64,
        token: UUID
    ) {
        let outcome = setLockedReturning { () -> (accepted: Bool, recover: Bool) in
            // During construction the progress callback can arrive before
            // the session handle is installed. The separate generation token
            // rejects callbacks from an owner that has since been stopped.
            guard namespaceTokens[accountId] == token else { return (false, false) }
            namespaceProgress[accountId] = progress
            switch progress {
            case .ready:
                namespaceReadyAccounts.insert(accountId)
                namespaceDegradations.removeValue(forKey: accountId)
                namespaceRecoveryScheduled.remove(accountId)
                // Reaching ready is what proves the previous failure was transient,
                // so the backoff starts over from the configured delay.
                namespaceRecoveryAttempts.removeValue(forKey: accountId)
                return (true, false)
            case let .degraded(category, retryable):
                if namespaceReadyAccounts.contains(accountId),
                   Self.isRecoverableSourceFailure(category: category, retryable: retryable)
                {
                    namespaceDegradations[accountId] = AgentActionableFailure(
                        category: category,
                        message: Self.namespaceFailureMessage(category: category),
                        retryable: retryable
                    )
                }
                return (true, false)
            case let .failed(category, retryable):
                let recover =
                    namespaceReadyAccounts.contains(accountId)
                        && Self.isRecoverableSourceFailure(category: category, retryable: retryable)
                if recover {
                    namespaceDegradations[accountId] = AgentActionableFailure(
                        category: category,
                        message: Self.namespaceFailureMessage(category: category),
                        retryable: retryable
                    )
                }
                return (true, recover)
            case .preparing, .stopped:
                return (true, false)
            }
        }
        guard outcome.accepted else { return }
        switch progress {
        case .preparing:
            record("namespace-preparing")
        case .ready:
            record("namespace-ready")
            ChangeSignal.post()
        case .degraded:
            record("namespace-degraded")
        case .failed:
            record("namespace-failed")
        case .stopped:
            record("namespace-stopped")
        }
        if outcome.recover {
            scheduleNamespaceRecovery(accountId: accountId, token: token)
        }
    }

    private func scheduleNamespaceRecovery(accountId: Int64, token: UUID) {
        let attempt = setLockedReturning { () -> Int? in
            guard namespaceTokens[accountId] == token,
                  namespaceRecoveryScheduled.insert(accountId).inserted
            else { return nil }
            let attempt = (namespaceRecoveryAttempts[accountId] ?? 0) + 1
            namespaceRecoveryAttempts[accountId] = attempt
            return attempt
        }
        guard let attempt else { return }
        let delay = namespaceRecoveryDelay(attempt: attempt)
        Task.detached { [weak self] in
            try? await Task.sleep(for: delay)
            self?.recoverNamespace(accountId: accountId, token: token)
        }
    }

    private func recoverNamespace(accountId: Int64, token: UUID) {
        let result = setLockedReturning { () -> (Bool, OwnedNamespaceSession?) in
            guard
                state == .running,
                namespaceTokens[accountId] == token,
                namespaceRecoveryScheduled.remove(accountId) != nil
            else { return (false, nil) }
            let owned = namespaceSessions.removeValue(forKey: accountId)
            namespaceProgress[accountId] = .preparing
            namespaceTokens.removeValue(forKey: accountId)
            return (true, owned)
        }
        guard result.0 else { return }
        result.1?.session.close()
        record("namespace-recovering")
        startNamespace(accountId: accountId)
    }

    /// Synchronous teardown bookkeeping for ``shutdown(reason:)`` —
    /// `NSLock` may not be taken from an async context, so the locked
    /// extraction lives in this sync helper.
    private func releaseResourcesAndStop(publishStopped: Bool = true) -> (
        PowerEventObservation?, AgentHealthServer?, HydrationServer?, ControlServer?,
        SingleInstanceLock?
    ) {
        lock.lock()
        defer { lock.unlock() }
        let resources = (
            powerObservation, healthServer, hydrationServer, controlServer, instanceLock
        )
        powerObservation = nil
        healthServer = nil
        hydrationServer = nil
        controlServer = nil
        instanceLock = nil
        store = nil
        core = nil
        if publishStopped { state = .stopped }
        return resources
    }

    /// Atomically claims the prepared termination before any endpoint is
    /// removed. A late cancellation cannot race this boundary into a false
    /// companion reply followed by an unobserved helper exit.
    private func claimTerminationCommit(
        _ request: ControlTerminationRequest,
        armWatchdog: () -> Bool
    ) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard
            state == .terminationReady,
            terminationRequestID == request.requestID,
            processIdentity.isValidTerminationIdentity,
            request.expectedAgentInstanceID == processIdentity.instanceID,
            !terminationCommitClaimed
        else { return false }
        guard armWatchdog() else { return false }
        terminationLeaseTask?.cancel()
        terminationLeaseTask = nil
        terminationCommitClaimed = true
        return true
    }

    /// The hydration admission gate over durable state: everything a
    /// snapshot read can refuse is refused here, before any engine work —
    /// unknown account or item (or a POL-3 tombstone), a POL-4 availability
    /// that withholds bytes (`restricted` remains a permission denial while
    /// source-gone `unavailable` becomes `notFound`), a directory (nothing to
    /// hydrate), and a pinned content version that is
    /// no longer current (`versionConflict` — the requester re-resolves and
    /// restarts, SYNC-042).
    private func admitHydration(_ request: HydrationRequest) -> HydrationAdmission {
        let store = setLockedReturning { self.store }
        guard let store else {
            return .refuse(
                HydrationFailure(category: .draining, detail: "state not open")
            )
        }
        do {
            guard try store.account(accountId: request.accountId) != nil else {
                return .refuse(
                    HydrationFailure(category: .notFound, detail: "unknown account")
                )
            }
            guard
                let item = try store.item(id: request.itemId),
                item.deletedAtMs == nil
            else {
                return .refuse(
                    HydrationFailure(category: .notFound, detail: "unknown item")
                )
            }
            guard !item.isDirectory else {
                return .refuse(
                    HydrationFailure(
                        category: .internalError, detail: "directories have no content"
                    )
                )
            }
            if let failure = Self.hydrationAdmissionFailure(for: item.availability) {
                return .refuse(failure)
            }
            if let pinned = request.contentVersion, pinned != item.contentVersion {
                return .refuse(
                    HydrationFailure(
                        category: .versionConflict,
                        detail: "pinned content version is not current"
                    )
                )
            }
            return .admit
        } catch {
            return .refuse(
                HydrationFailure(category: .storage, detail: "state read failed")
            )
        }
    }

    /// Availability-only part of admission, factored so the distinction that
    /// crosses IPC remains directly testable. Provider metadata can race from
    /// fetchable to either state after its local precheck; the agent is the
    /// authoritative last gate and must preserve the user-visible category.
    static func hydrationAdmissionFailure(
        for availability: ItemAvailability
    ) -> HydrationFailure? {
        switch availability {
        case .fetchable:
            return nil
        case .restricted:
            return HydrationFailure(
                category: .restricted, detail: "content restricted per POL-4"
            )
        case .unavailable:
            return HydrationFailure(
                category: .notFound, detail: "content gone at the source"
            )
        }
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
                dataRoot: configuration.dataRoot.path, role: .coordinator
            )
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
            case .Restricted: return "restricted"
            case .VersionConflict: return "version-conflict"
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
        let hydration = hydrationServer
        let control = controlServer
        let instance = instanceLock
        healthServer = nil
        hydrationServer = nil
        controlServer = nil
        instanceLock = nil
        store = nil
        core = nil
        state = .stopped
        lock.unlock()
        control?.stop()
        hydration?.stop()
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
            bundleVersion: AgentBuildVersion.current,
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
            recentEvents: []
        )
    }
}
