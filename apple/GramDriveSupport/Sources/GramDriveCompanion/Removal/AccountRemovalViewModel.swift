import Foundation

/// The account-removal screen's state.
public enum RemovalPhase: Equatable, Sendable {
    /// Idle; no removal in progress.
    case idle
    /// Confirmation is being collected (the user opened the destructive flow).
    case confirming
    /// The removal is in flight.
    case removing
    /// The account was removed and its on-disk trace wiped (SEC-004).
    case removed
    /// The confirmation the user gave does not authorize the removal.
    case invalidConfirmation
    /// No control channel could run it (honest, not a failure).
    case unavailable(ControlChannelUnavailable)
    /// The agent ran it and it failed, classified.
    case failed(CommandFailure)
}

/// Removes an account: the server-side logout and the trace-free on-disk wipe
/// (SEC-004), which the engine's account-removal flow owns. The shell's job is
/// to gate the irreversible action behind an explicit, typed confirmation and
/// to render the outcome — never to perform the wipe itself.
@MainActor
@Observable
public final class AccountRemovalViewModel {
    /// The account this screen removes; the user must echo it to confirm.
    public let accountLabel: String
    /// The label the user typed to confirm.
    public var typedConfirmation: String = ""
    /// Whether the user acknowledged the removal is irreversible.
    public var acknowledgedIrreversible: Bool = false
    public private(set) var phase: RemovalPhase = .idle

    private let backend: any CompanionBackend

    public init(backend: any CompanionBackend, accountLabel: String) {
        self.backend = backend
        self.accountLabel = accountLabel
    }

    /// The confirmation the current inputs describe.
    public var confirmation: RemovalConfirmation {
        RemovalConfirmation(
            accountLabel: accountLabel,
            typedConfirmation: typedConfirmation,
            acknowledgedIrreversible: acknowledgedIrreversible)
    }

    /// Whether the current inputs authorize the removal.
    public var canRemove: Bool { confirmation.isValid }

    /// Opens the destructive flow (shows the confirmation controls).
    public func beginConfirmation() {
        phase = .confirming
    }

    /// Removes the account, if the confirmation is valid. An invalid
    /// confirmation is refused locally — no command is issued.
    public func remove() async {
        guard confirmation.isValid else {
            phase = .invalidConfirmation
            return
        }
        phase = .removing
        phase = Self.phase(for: await backend.removeAccount(confirmation))
    }

    static func phase(for outcome: CommandOutcome) -> RemovalPhase {
        switch outcome {
        case .completed: return .removed
        case .unavailable(let reason): return .unavailable(reason)
        case .failed(let failure): return .failed(failure)
        }
    }
}
