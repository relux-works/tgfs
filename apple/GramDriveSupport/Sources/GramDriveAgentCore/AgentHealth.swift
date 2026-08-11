import Darwin
import Foundation

/// Immutable identity for one agent process. `pid` alone is never a
/// termination witness: the kernel start time prevents PID reuse, while the
/// instance UUID prevents a new endpoint at the same socket path being
/// mistaken for the old helper.
public struct AgentProcessIdentity: Codable, Equatable, Sendable {
    public var instanceID: UUID
    public var pid: Int32
    public var kernelStartSeconds: Int64
    public var kernelStartMicroseconds: Int64

    public init(
        instanceID: UUID,
        pid: Int32,
        kernelStartSeconds: Int64,
        kernelStartMicroseconds: Int64
    ) {
        self.instanceID = instanceID
        self.pid = pid
        self.kernelStartSeconds = kernelStartSeconds
        self.kernelStartMicroseconds = kernelStartMicroseconds
    }

    /// A process identity is useful as a termination witness only when all
    /// kernel-provided components are present. In particular, a PID without
    /// its start time must never be used to target a signal: it can belong to
    /// an unrelated process after reuse.
    public var isValidTerminationIdentity: Bool {
        pid > 0 && kernelStartSeconds > 0 && kernelStartMicroseconds >= 0
    }

    public static func current(instanceID: UUID = UUID()) -> AgentProcessIdentity {
        var info = proc_bsdinfo()
        let pid = getpid()
        let count = proc_pidinfo(
            pid, PROC_PIDTBSDINFO, 0, &info, Int32(MemoryLayout<proc_bsdinfo>.size)
        )
        guard count == MemoryLayout<proc_bsdinfo>.size else {
            return AgentProcessIdentity(
                instanceID: instanceID, pid: pid, kernelStartSeconds: 0,
                kernelStartMicroseconds: 0
            )
        }
        return AgentProcessIdentity(
            instanceID: instanceID,
            pid: pid,
            kernelStartSeconds: Int64(info.pbi_start_tvsec),
            kernelStartMicroseconds: Int64(info.pbi_start_tvusec)
        )
    }

    /// Observes the exact kernel process captured for a termination
    /// transaction. A failed or short `proc_pidinfo` read is deliberately
    /// indeterminate: it is not evidence that a process exited and must not
    /// authorize either an AppKit `true` reply or a signal to this PID.
    public func observe() -> AgentProcessObservation {
        guard isValidTerminationIdentity else { return .indeterminate }
        var info = proc_bsdinfo()
        errno = 0
        let count = proc_pidinfo(
            pid, PROC_PIDTBSDINFO, 0, &info, Int32(MemoryLayout<proc_bsdinfo>.size)
        )
        if count == MemoryLayout<proc_bsdinfo>.size {
            return Self.classifyObservation(
                expected: self,
                observedStartSeconds: Int64(info.pbi_start_tvsec),
                observedStartMicroseconds: Int64(info.pbi_start_tvusec)
            )
        }
        return count == 0 && errno == ESRCH ? .absent : .indeterminate
    }

    /// Pure classification seam for deterministic process-identity tests.
    public static func classifyObservation(
        expected: AgentProcessIdentity,
        observedStartSeconds: Int64?,
        observedStartMicroseconds: Int64?
    ) -> AgentProcessObservation {
        guard expected.isValidTerminationIdentity,
              let observedStartSeconds,
              let observedStartMicroseconds
        else { return .indeterminate }
        return expected.kernelStartSeconds == observedStartSeconds
            && expected.kernelStartMicroseconds == observedStartMicroseconds
            ? .matching : .replaced
    }
}

/// The only accepted outcomes of an exact process observation. `absent` and
/// `replaced` prove that the captured old instance no longer exists; an
/// indeterminate kernel query fails closed.
public enum AgentProcessObservation: Equatable, Sendable {
    case matching
    case absent
    case replaced
    case indeterminate

    public var provesCapturedProcessExited: Bool {
        self == .absent || self == .replaced
    }
}

/// Lifecycle state of the agent process.
public enum AgentRunState: String, Codable, Sendable {
    /// Process is up; lock and shared state not yet established.
    case launching
    /// Startup reconciliation: opening durable state, recovering from a
    /// crash or corruption if needed.
    case recovering
    /// Serving. The steady state.
    case running
    /// Shutting down: no new work admitted, in-flight transfers draining.
    case draining
    /// A companion-requested drain is complete, but the agent retains its
    /// sockets and durable state until the companion explicitly commits the
    /// termination. An uncommitted request rolls back at its bounded lease.
    case terminationReady = "termination-ready"
    /// A requested termination could not safely finish. The process and its
    /// endpoints remain alive so the current app version can explain the
    /// retry/Force Quit boundary instead of mistaking a vanished socket for a
    /// successful update install.
    case terminationCancelled = "termination-cancelled"
    /// Fully stopped; the process is about to exit.
    case stopped
}

/// The agent's version, independent of the FFI contract version. Bumped
/// with agent behavior changes so the app shell can detect a stale running
/// agent after an update and ask it to restart.
public enum AgentVersion {
    public static let current = "0.1.0"
}

/// The numeric build of the packaged app bundle hosting the agent. This is
/// deliberately separate from the semantic agent protocol version: Sparkle
/// replacement needs byte-ordering identity, not a feature label.
public enum AgentBuildVersion {
    #if DEBUG
        /// The executable sets this once, before it starts any agent work. It is
        /// process-local and read-only after startup; the explicit unchecked
        /// annotation keeps the test-only CLI seam out of the runtime isolation
        /// model.
        private nonisolated(unsafe) static var testReportedBuild: String?

        /// Installs an explicit debug-only process-local build value for a
        /// subprocess integration test. Release builds cannot compile this seam,
        /// and production never derives a build from process arguments.
        public static func installTestReportedBuild(_ value: String) -> Bool {
            guard !value.isEmpty, value.allSatisfy(\.isNumber) else { return false }
            testReportedBuild = value
            return true
        }
    #endif

    public static var current: String {
        #if DEBUG
            if let testReportedBuild { return testReportedBuild }
        #endif
        let value = Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String
        return value?.allSatisfy(\.isNumber) == true ? value! : "0"
    }
}

#if DEBUG
    /// Process-local integration fixtures. The executable installs these only
    /// through named test CLI flags before the lifecycle starts; release builds do
    /// not compile this type.
    public enum AgentRuntimeTestOverrides {
        private nonisolated(unsafe) static var reportedFinderHierarchyReady = false

        public static var finderHierarchyReady: Bool {
            reportedFinderHierarchyReady
        }

        public static func installFinderHierarchyReady() {
            reportedFinderHierarchyReady = true
        }
    }
#endif

/// Readiness of the bounded local structure File Provider can enumerate
/// without waiting for Telegram history or media.
public enum FinderContentState: String, Codable, Equatable, Sendable {
    case waitingForAuthorization
    case preparing
    case ready
    case failed
}

/// A privacy-safe failure the companion can present with a concrete retry.
public struct AgentActionableFailure: Codable, Equatable, Sendable {
    public var category: String
    public var message: String
    public var retryable: Bool

    public init(category: String, message: String, retryable: Bool) {
        self.category = category
        self.message = message
        self.retryable = retryable
    }
}

/// One point-in-time health/status report (NFR-032).
///
/// Fields the engine does not populate yet — source update time, change
/// cursor, cache pressure, provider registration — are optionals that stay
/// `nil` until the owning stories wire them. An honest `nil` beats a
/// fabricated value: consumers can distinguish "not wired yet" from a real
/// reading.
public struct AgentHealthSnapshot: Codable, Equatable, Sendable {
    /// Version of this payload's shape; consumers must tolerate unknown
    /// fields (additive evolution, same rule as the FFI contract).
    public var payloadVersion: Int
    /// ``AgentVersion/current`` of the responding agent.
    public var agentVersion: String
    /// Packaged numeric `CFBundleVersion`; optional for decoding an agent
    /// predating this update protocol.
    public var bundleVersion: String?
    /// FFI contract version of the core the agent links, `major.minor.patch`.
    public var contractVersion: String
    /// Process identifier of the agent.
    public var pid: Int32
    /// Strong identity of the responding process. Optional for an old agent
    /// so a new companion can fail closed rather than guessing on PID reuse.
    public var processIdentity: AgentProcessIdentity?
    /// Lifecycle state at snapshot time.
    public var state: AgentRunState
    /// Request identity while a companion-requested drain is active or has
    /// been cancelled. This is additive for older agents.
    public var terminationRequestID: UUID?
    /// Monotonically advances whenever a cancelled termination has restored
    /// the current process to serving. A rollback proof is never inferred
    /// from a stale cancellation state without this generation.
    public var servingGeneration: UInt64?
    /// Explicit local admission proof for a cancelled termination. `true`
    /// means File Provider hydration may register new transfers again.
    public var transferAdmissionOpen: Bool?
    /// Whether every namespace owner captured before the termination drain
    /// has been restored. Older agents omit this and are not usable as a
    /// rollback proof.
    public var namespaceOwnersRestored: Bool?
    /// When the agent process started, ms since the Unix epoch.
    public var startedAtMs: Int64
    /// The user's launch-at-login preference as the agent read it; `nil`
    /// when settings were unreadable.
    public var launchAtLogin: Bool?
    /// Schema version of the shared state database, when open.
    public var stateSchemaVersion: Int64?
    /// The shared state change stamp, when open. Meaningful only relative
    /// to earlier snapshots from the same agent run.
    public var dataVersion: Int64?
    /// In-flight transfer count in this agent.
    public var pendingTransferCount: Int
    /// Last successful source update, ms since epoch. Not wired yet (the
    /// engine's source loop owns it); always `nil` today.
    public var lastSourceUpdateMs: Int64?
    /// Durable change cursor position. Not wired yet; always `nil` today.
    public var changeCursor: String?
    /// Managed cache pressure indicator. Not wired yet; always `nil` today.
    public var cachePressure: String?
    /// File Provider domain registration state. Owned by the domain story;
    /// always `nil` today.
    public var providerRegistrationState: String?
    /// Last system sleep observed, ms since epoch.
    public var lastSleepMs: Int64?
    /// Last system wake observed, ms since epoch.
    public var lastWakeMs: Int64?
    /// Recent lifecycle events and failures, newest last. Redacted by
    /// construction: fixed vocabulary composed by the agent, never user
    /// data, paths, or account material (NFR-032).
    public var recentEvents: [String]
    /// The container's configured accounts as durable state reports them
    /// (identity, display name, auth state — never secret material). `nil`
    /// when the snapshot predates this field or the state is not open;
    /// an empty array is a real "no accounts configured" reading.
    public var accounts: [AccountHealthSummary]?
    /// Whether Finder's local first page is available. Additive and optional
    /// so an updated app can still decode an older running agent.
    public var finderContentState: FinderContentState?
    /// Request-bounded fixed-item count (three per authorized account).
    public var finderFirstPageItemCount: Int?
    /// Actionable reason when ``finderContentState`` is ``failed``.
    public var finderContentFailure: AgentActionableFailure?
    /// A retryable source problem that does not invalidate the already
    /// projected Finder namespace. Present only while the local namespace
    /// remains usable and source recovery is pending.
    public var finderSourceDegradation: AgentActionableFailure?
    /// How many foreground history hints the File Provider has actually
    /// delivered, by kind. Counts only — never a chat identity.
    ///
    /// This exists because the demand path spans two processes and a socket,
    /// and "the opened chat did not advance" has two completely different
    /// causes: the provider never sent a hint, or the agent received one and
    /// did not honor it. Without a count at the receiving end there is no way
    /// to tell them apart on an installed build (BUG-260728-2qfzbd).
    public var historyPriorityHints: HistoryPriorityHintCounts?
    /// Durable aggregate File Provider fetch health. This excludes callback
    /// identifiers and payloads by construction.
    public var providerFetchHealth: ProviderFetchHealthCounts?

    /// Public memberwise initializer so consumers of the payload (the app
    /// shell) and their tests can construct a snapshot; in production the
    /// snapshot is decoded from the agent's JSON, never built by hand.
    public init(
        payloadVersion: Int,
        agentVersion: String,
        bundleVersion: String? = nil,
        contractVersion: String,
        pid: Int32,
        processIdentity: AgentProcessIdentity? = nil,
        state: AgentRunState,
        terminationRequestID: UUID? = nil,
        servingGeneration: UInt64? = nil,
        transferAdmissionOpen: Bool? = nil,
        namespaceOwnersRestored: Bool? = nil,
        startedAtMs: Int64,
        launchAtLogin: Bool?,
        stateSchemaVersion: Int64?,
        dataVersion: Int64?,
        pendingTransferCount: Int,
        lastSourceUpdateMs: Int64?,
        changeCursor: String?,
        cachePressure: String?,
        providerRegistrationState: String?,
        lastSleepMs: Int64?,
        lastWakeMs: Int64?,
        recentEvents: [String],
        accounts: [AccountHealthSummary]? = nil,
        finderContentState: FinderContentState? = nil,
        finderFirstPageItemCount: Int? = nil,
        finderContentFailure: AgentActionableFailure? = nil,
        finderSourceDegradation: AgentActionableFailure? = nil,
        historyPriorityHints: HistoryPriorityHintCounts? = nil,
        providerFetchHealth: ProviderFetchHealthCounts? = nil
    ) {
        self.payloadVersion = payloadVersion
        self.agentVersion = agentVersion
        self.bundleVersion = bundleVersion
        self.contractVersion = contractVersion
        self.pid = pid
        self.processIdentity = processIdentity
        self.state = state
        self.terminationRequestID = terminationRequestID
        self.servingGeneration = servingGeneration
        self.transferAdmissionOpen = transferAdmissionOpen
        self.namespaceOwnersRestored = namespaceOwnersRestored
        self.startedAtMs = startedAtMs
        self.launchAtLogin = launchAtLogin
        self.stateSchemaVersion = stateSchemaVersion
        self.dataVersion = dataVersion
        self.pendingTransferCount = pendingTransferCount
        self.lastSourceUpdateMs = lastSourceUpdateMs
        self.changeCursor = changeCursor
        self.cachePressure = cachePressure
        self.providerRegistrationState = providerRegistrationState
        self.lastSleepMs = lastSleepMs
        self.lastWakeMs = lastWakeMs
        self.recentEvents = recentEvents
        self.accounts = accounts
        self.finderContentState = finderContentState
        self.finderFirstPageItemCount = finderFirstPageItemCount
        self.finderContentFailure = finderContentFailure
        self.finderSourceDegradation = finderSourceDegradation
        self.historyPriorityHints = historyPriorityHints
        self.providerFetchHealth = providerFetchHealth
    }
}

/// Foreground history hints the agent has accepted, by kind.
///
/// Counts and one timestamp only: a chat identity would make the health
/// payload describe *which* chat a user opened, which it must never do.
public struct HistoryPriorityHintCounts: Codable, Equatable, Sendable {
    /// Hints the provider delivered that named a chat with a live session.
    public var accepted: Int
    /// Accepted hints that raised a chat to visible.
    public var visible: Int
    /// Accepted hints that raised a chat to requested.
    public var requested: Int
    /// Accepted hints that released a chat back to background.
    public var background: Int
    /// Hints that named an account with no owned session — delivered, and
    /// dropped. Separated from `accepted` so a silent routing failure cannot
    /// read as a delivery failure.
    public var unroutable: Int
    /// When the last hint of any kind arrived.
    public var lastAtMs: Int64?

    public init(
        accepted: Int = 0,
        visible: Int = 0,
        requested: Int = 0,
        background: Int = 0,
        unroutable: Int = 0,
        lastAtMs: Int64? = nil
    ) {
        self.accepted = accepted
        self.visible = visible
        self.requested = requested
        self.background = background
        self.unroutable = unroutable
        self.lastAtMs = lastAtMs
    }
}

/// Durable, identity-free File Provider fetch counters.
public struct ProviderFetchHealthCounts: Codable, Equatable, Sendable {
    /// Callbacks recorded by the coordinator.
    public var callbacks: UInt64
    /// Verified content returns.
    public var succeeded: UInt64
    /// Hydration engine or transport failures.
    public var engineFailures: UInt64
    /// Error mappings returned to macOS.
    public var providerMappings: UInt64
    /// Mappings that asserted `noSuchItem`.
    public var noSuchItem: UInt64
    /// Outcomes macOS may retry.
    public var retryable: UInt64

    public init(
        callbacks: UInt64 = 0,
        succeeded: UInt64 = 0,
        engineFailures: UInt64 = 0,
        providerMappings: UInt64 = 0,
        noSuchItem: UInt64 = 0,
        retryable: UInt64 = 0
    ) {
        self.callbacks = callbacks
        self.succeeded = succeeded
        self.engineFailures = engineFailures
        self.providerMappings = providerMappings
        self.noSuchItem = noSuchItem
        self.retryable = retryable
    }
}

/// One account as health reports it — the status projection of the durable
/// account row (never secret material, NFR-032).
public struct AccountHealthSummary: Codable, Equatable, Sendable {
    /// The account's stable Telegram identity.
    public var accountId: Int64
    /// The account's display name.
    public var displayName: String
    /// The durable auth-state marker (`authorized`, …).
    public var authState: String

    public init(accountId: Int64, displayName: String, authState: String) {
        self.accountId = accountId
        self.displayName = displayName
        self.authState = authState
    }
}
