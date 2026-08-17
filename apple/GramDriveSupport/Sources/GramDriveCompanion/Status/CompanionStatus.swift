import Foundation
import GramDriveAgentCore

/// Whether the engine-hosting agent is answering.
public enum AgentPresence: Equatable, Sendable {
    case running(AgentRunState)
    case notRunning
    case unreachable(String)

    public var label: String {
        switch self {
        case .running(let state): return "Running (\(state.rawValue))"
        case .notRunning: return "Not running"
        case .unreachable(let why): return "Unreachable: \(why)"
        }
    }
}

/// The account's authorization standing from the durable account summaries
/// and the agent's live namespace observations carried by health.
public enum AccountStatus: Equatable, Sendable {
    /// The agent is not running, so no account can be served.
    case agentUnavailable
    /// The agent is up, but its health does not (yet) report authorization.
    case unknown
    /// At least one configured account is durably authorized.
    case authorized
    /// The agent reported no configured account.
    case notConfigured
    /// Accounts exist, but none is currently authorized.
    case authorizationRequired

    public var label: String {
        switch self {
        case .agentUnavailable: return "Agent not running"
        case .unknown: return "Status not reported yet"
        case .authorized: return "Authorized"
        case .notConfigured: return "No account configured"
        case .authorizationRequired: return "Authorization Required"
        }
    }
}

/// The File Provider domain registration standing, projected from the health
/// payload's `providerRegistrationState` (owned by the domain story; `nil`
/// today).
public enum ProviderDomainStatus: Equatable, Sendable {
    case registered
    case notRegistered
    case unknown
    case other(String)

    public var label: String {
        switch self {
        case .registered: return "Registered"
        case .notRegistered: return "Not registered"
        case .unknown: return "Not reported yet"
        case .other(let raw): return raw
        }
    }

    static func from(_ raw: String?) -> ProviderDomainStatus {
        guard let raw else { return .unknown }
        switch raw {
        case "registered": return .registered
        case "notRegistered", "not_registered": return .notRegistered
        default: return .other(raw)
        }
    }
}

/// A presentation-friendly projection of one ``AgentHealthSnapshot`` for the
/// diagnostics screen. Honest optionals throughout: a `nil` means "not wired
/// yet", never a fabricated reading.
public struct DiagnosticsReport: Equatable, Sendable {
    public var agentVersion: String
    public var contractVersion: String
    public var pid: Int32
    public var runState: AgentRunState
    public var startedAt: Date
    public var launchAtLogin: Bool?
    public var stateSchemaVersion: Int64?
    public var dataVersion: Int64?
    public var pendingTransferCount: Int
    public var lastSourceUpdate: Date?
    public var changeCursor: String?
    public var cachePressure: String?
    public var providerRegistrationState: String?
    public var lastSleep: Date?
    public var lastWake: Date?
    public var recentEvents: [String]

    public init(snapshot: AgentHealthSnapshot) {
        self.agentVersion = snapshot.agentVersion
        self.contractVersion = snapshot.contractVersion
        self.pid = snapshot.pid
        self.runState = snapshot.state
        self.startedAt = Date(millisecondsSince1970: snapshot.startedAtMs)
        self.launchAtLogin = snapshot.launchAtLogin
        self.stateSchemaVersion = snapshot.stateSchemaVersion
        self.dataVersion = snapshot.dataVersion
        self.pendingTransferCount = snapshot.pendingTransferCount
        self.lastSourceUpdate = snapshot.lastSourceUpdateMs.map(Date.init(millisecondsSince1970:))
        self.changeCursor = snapshot.changeCursor
        self.cachePressure = snapshot.cachePressure
        self.providerRegistrationState = snapshot.providerRegistrationState
        self.lastSleep = snapshot.lastSleepMs.map(Date.init(millisecondsSince1970:))
        self.lastWake = snapshot.lastWakeMs.map(Date.init(millisecondsSince1970:))
        self.recentEvents = snapshot.recentEvents
    }
}

extension Date {
    init(millisecondsSince1970 ms: Int64) {
        self.init(timeIntervalSince1970: Double(ms) / 1000.0)
    }
}

/// Renders account, provider/domain, and diagnostics status from the agent's
/// health. All derivations are pure functions of the last ``HealthReadout``,
/// so every screen state is a snapshot away in a test.
@MainActor
@Observable
public final class CompanionStatusViewModel {
    public private(set) var readout: HealthReadout = .notRunning
    private var reportedProviderStatus: ProviderDomainStatus?

    private let backend: any CompanionBackend

    public init(backend: any CompanionBackend) {
        self.backend = backend
    }

    /// Refreshes the reading from the agent.
    public func refresh() async {
        readout = await backend.fetchHealth()
    }

    public var agentPresence: AgentPresence { Self.agentPresence(from: readout) }
    public var accountStatus: AccountStatus { Self.accountStatus(from: readout) }
    public var providerStatus: ProviderDomainStatus {
        reportedProviderStatus ?? Self.providerStatus(from: readout)
    }
    public var diagnostics: DiagnosticsReport? { Self.diagnostics(from: readout) }

    /// Reports the app-owned result of domain reconciliation. Health remains
    /// the cross-process fallback for future agent-owned registration state.
    public func reportProviderStatus(_ status: ProviderDomainStatus) {
        reportedProviderStatus = status
    }

    // MARK: - Pure derivations

    // `nonisolated`: pure functions of the readout, touching no actor state,
    // so tests and callers off the main actor can exercise every screen state.

    public nonisolated static func agentPresence(from readout: HealthReadout) -> AgentPresence {
        switch readout {
        case .running(let snapshot): return .running(snapshot.state)
        case .notRunning: return .notRunning
        case .timedOut: return .unreachable("timed out")
        case .error(let detail): return .unreachable(detail)
        }
    }

    public nonisolated static func accountStatus(from readout: HealthReadout) -> AccountStatus {
        switch readout {
        case .running(let snapshot):
            guard let accounts = snapshot.accounts else { return .unknown }
            if accounts.contains(where: {
                $0.authState == "authorized"
                    && $0.observedAuthorization != .authorizationRequired
            }) {
                return .authorized
            }
            return accounts.isEmpty ? .notConfigured : .authorizationRequired
        case .notRunning, .timedOut, .error: return .agentUnavailable
        }
    }

    public nonisolated static func providerStatus(
        from readout: HealthReadout
    ) -> ProviderDomainStatus {
        switch readout {
        case .running(let snapshot): return .from(snapshot.providerRegistrationState)
        case .notRunning, .timedOut, .error: return .unknown
        }
    }

    public nonisolated static func diagnostics(from readout: HealthReadout) -> DiagnosticsReport? {
        guard case .running(let snapshot) = readout else { return nil }
        return DiagnosticsReport(snapshot: snapshot)
    }
}
