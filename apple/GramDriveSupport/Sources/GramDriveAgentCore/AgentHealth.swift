import Foundation

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
    /// Fully stopped; the process is about to exit.
    case stopped
}

/// The agent's version, independent of the FFI contract version. Bumped
/// with agent behavior changes so the app shell can detect a stale running
/// agent after an update and ask it to restart.
public enum AgentVersion {
    public static let current = "0.1.0"
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
    /// FFI contract version of the core the agent links, `major.minor.patch`.
    public var contractVersion: String
    /// Process identifier of the agent.
    public var pid: Int32
    /// Lifecycle state at snapshot time.
    public var state: AgentRunState
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

    /// Public memberwise initializer so consumers of the payload (the app
    /// shell) and their tests can construct a snapshot; in production the
    /// snapshot is decoded from the agent's JSON, never built by hand.
    public init(
        payloadVersion: Int,
        agentVersion: String,
        contractVersion: String,
        pid: Int32,
        state: AgentRunState,
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
        accounts: [AccountHealthSummary]? = nil
    ) {
        self.payloadVersion = payloadVersion
        self.agentVersion = agentVersion
        self.contractVersion = contractVersion
        self.pid = pid
        self.state = state
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
