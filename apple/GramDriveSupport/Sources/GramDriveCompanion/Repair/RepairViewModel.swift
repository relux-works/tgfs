import Foundation

/// The repair screen's state.
public enum RepairPhase: Equatable, Sendable {
    /// Idle; the user has not asked for a repair pass.
    case idle
    /// A repair pass is in flight.
    case running
    /// The agent completed the repair pass.
    case succeeded
    /// No control channel could run it (honest, not a failure).
    case unavailable(ControlChannelUnavailable)
    /// The agent ran it and it failed, classified.
    case failed(CommandFailure)
}

/// Runs the agent's repair/reconciliation pass and renders its outcome.
///
/// Repair is a Telegram/engine operation (re-open durable state, re-reconcile
/// the source), so it runs in the agent; the shell only asks and renders — the
/// same seam rule as every other command.
@MainActor
@Observable
public final class RepairViewModel {
    public private(set) var phase: RepairPhase = .idle

    private let backend: any CompanionBackend

    public init(backend: any CompanionBackend) {
        self.backend = backend
    }

    /// Whether a repair can be started right now.
    public var canRepair: Bool { phase != .running }

    /// Asks the agent to repair, then renders the outcome.
    public func repair() async {
        guard canRepair else { return }
        phase = .running
        phase = Self.phase(for: await backend.requestRepair())
    }

    static func phase(for outcome: CommandOutcome) -> RepairPhase {
        switch outcome {
        case .completed: return .succeeded
        case .unavailable(let reason): return .unavailable(reason)
        case .failed(let failure): return .failed(failure)
        }
    }
}
