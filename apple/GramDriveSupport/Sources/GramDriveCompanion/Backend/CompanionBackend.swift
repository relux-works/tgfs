import Foundation
import GramDriveAgentCore

/// The single boundary between the shell and the engine-hosting agent.
///
/// Everything the shell does that touches Telegram or drive state goes
/// through here, and only here — the AC's "UI drives the agent via IPC; no
/// Telegram operations from filesystem callbacks" is this seam existing and
/// the shell holding nothing else. Two shapes of operation:
///
/// - **Reads that exist today** — ``fetchHealth()`` over the agent's bounded
///   health socket, and ``loadSettings()``/``saveSettings(_:)`` over the
///   host-owned settings document. Both are wired for real in
///   ``LiveCompanionBackend``.
/// - **Commands that need an agent control channel** — authorization,
///   repair, account removal. The health socket is read-only by design
///   (`AgentHealthServer`: "control operations stay where they belong … not
///   an IPC verb"), and the FFI exposes no auth/repair/removal surface yet,
///   so the control channel is a future story. The live backend reports
///   these honestly as ``ControlChannelUnavailable`` rather than pretending;
///   a scripted backend drives every state for tests and previews.
public protocol CompanionBackend: Sendable {
    /// One point-in-time reading of the agent's health/status.
    func fetchHealth() async -> HealthReadout
    /// Loads the durable host-owned settings (missing file is the defaults).
    func loadSettings() throws -> AgentSettings
    /// Persists the durable host-owned settings atomically.
    func saveSettings(_ settings: AgentSettings) throws
    /// A fresh authorization session to drive one sign-in flow.
    func makeAuthorizationSession() -> any AuthorizationSession
    /// Asks the agent to run its repair/reconciliation pass.
    func requestRepair() async -> CommandOutcome
    /// Removes an account: the trace-free on-disk wipe and server logout
    /// (SEC-004), owned by the engine's account-removal flow. Requires an
    /// explicit ``RemovalConfirmation`` — this is irreversible.
    func removeAccount(_ confirmation: RemovalConfirmation) async -> CommandOutcome
}

/// One reading of the agent's health, in the shape the shell branches on
/// rather than parses (the same "branch, don't parse" rule the health client
/// follows for its errors).
public enum HealthReadout: Equatable, Sendable {
    /// The agent answered; here is its snapshot.
    case running(AgentHealthSnapshot)
    /// Nothing is listening — the agent is not running (or not up yet).
    case notRunning
    /// The agent did not answer within the timeout.
    case timedOut
    /// The read failed for another reason; diagnostic detail, not contractual.
    case error(String)
}

/// Why an agent control channel could not serve a command. A first-class,
/// honest state — not an error the user caused.
public enum ControlChannelUnavailable: Equatable, Sendable {
    /// No control channel exists in this build yet (the command-IPC story
    /// has not landed). The one the live backend reports today.
    case notWired
    /// The agent is not running, so there is nothing to command.
    case agentNotRunning
    /// The channel existed but dropped mid-operation.
    case dropped

    /// A short, user-facing explanation.
    public var message: String {
        switch self {
        case .notWired:
            return "This action needs the agent control channel, which is not available in this build yet."
        case .agentNotRunning:
            return "The GramDrive agent is not running."
        case .dropped:
            return "Lost the connection to the GramDrive agent."
        }
    }
}

/// The stable category of a failed command, mirroring the FFI `DriveError`
/// categories so a UI can pick an actionable state without parsing detail.
public enum CommandFailure: Equatable, Sendable {
    case invalidArgument
    case notFound
    case authRequired
    case rateLimited(retryAfterMs: UInt64?)
    case sourceUnavailable
    case storage
    case integrity
    case cancelled
    case internalError

    /// A short, user-facing explanation.
    public var message: String {
        switch self {
        case .invalidArgument: return "The request was invalid."
        case .notFound: return "Nothing to act on was found."
        case .authRequired: return "Sign in first — this action needs an authorized account."
        case .rateLimited(let ms):
            if let ms { return "Rate limited — try again in \(ms / 1000)s." }
            return "Rate limited — try again shortly."
        case .sourceUnavailable: return "Telegram is unreachable right now."
        case .storage: return "A local storage error occurred."
        case .integrity: return "An integrity check failed."
        case .cancelled: return "The operation was cancelled."
        case .internalError: return "An internal error occurred."
        }
    }
}

/// The outcome of a command the shell issued to the agent.
public enum CommandOutcome: Equatable, Sendable {
    /// The agent completed the command.
    case completed
    /// No control channel could serve it — honest, not a failure.
    case unavailable(ControlChannelUnavailable)
    /// The agent ran it and it failed, classified.
    case failed(CommandFailure)
}

/// Explicit confirmation for an irreversible account removal (SEC-004).
///
/// A typed gate, not a bare `Bool`: the view model can only produce a
/// confirmation by echoing the account's own label, so an accidental or
/// mis-wired call cannot trigger a wipe.
public struct RemovalConfirmation: Equatable, Sendable {
    /// The account label the removal targets.
    public let accountLabel: String
    /// The label the user typed to confirm; must equal ``accountLabel``.
    public let typedConfirmation: String
    /// The user explicitly acknowledged the removal is irreversible.
    public let acknowledgedIrreversible: Bool

    public init(accountLabel: String, typedConfirmation: String, acknowledgedIrreversible: Bool) {
        self.accountLabel = accountLabel
        self.typedConfirmation = typedConfirmation
        self.acknowledgedIrreversible = acknowledgedIrreversible
    }

    /// Whether this confirmation authorizes the removal: the typed text
    /// matches the label (case- and whitespace-insensitively) and the user
    /// acknowledged irreversibility.
    public var isValid: Bool {
        acknowledgedIrreversible
            && !accountLabel.trimmed.isEmpty
            && typedConfirmation.trimmed.caseInsensitiveEquals(accountLabel.trimmed)
    }
}

extension String {
    var trimmed: String { trimmingCharacters(in: .whitespacesAndNewlines) }
    func caseInsensitiveEquals(_ other: String) -> Bool {
        caseInsensitiveCompare(other) == .orderedSame
    }
}
