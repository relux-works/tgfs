import Foundation

/// AppKit-facing adapter for a single asynchronous termination request.
/// Keeping the reply boundary in the companion module makes the executable
/// delegate a thin platform adapter and leaves its exactly-once behavior
/// deterministic under joined requests, cancellation, and bounded leases.
@MainActor
public final class ApplicationTerminationRequestDriver {
    private let replyGate = ApplicationTerminationReplyGate()
    private let requestTermination: @MainActor (CompanionTerminationCoordinator.Intent) async -> Bool
    private let cancelTermination: @MainActor () async -> Bool

    public init(
        requestTermination: @escaping @MainActor (CompanionTerminationCoordinator.Intent) async -> Bool,
        cancelTermination: @escaping @MainActor () async -> Bool
    ) {
        self.requestTermination = requestTermination
        self.cancelTermination = cancelTermination
    }

    public var isPending: Bool {
        replyGate.isPending
    }

    /// Returns `true` only when this call started the one AppKit request. A
    /// joined AppKit callback keeps waiting for the same eventual reply.
    @discardableResult
    public func applicationShouldTerminate(
        intent: CompanionTerminationCoordinator.Intent,
        reply: @escaping @MainActor (Bool) -> Void
    ) -> Bool {
        guard replyGate.begin() else { return false }
        Task { [weak self] in
            guard let self else { return }
            let allowed = await requestTermination(intent)
            replyOnce(allowed, reply: reply)
        }
        return true
    }

    /// The Keep GramDrive Open affordance joins the existing request; it never
    /// manufactures another AppKit reply or another agent drain.
    public func cancelPendingTermination(reply: @escaping @MainActor (Bool) -> Void) {
        guard replyGate.isPending else { return }
        Task { [weak self] in
            guard let self else { return }
            let allowed = await cancelTermination()
            replyOnce(allowed, reply: reply)
        }
    }

    private func replyOnce(_ allowed: Bool, reply: @escaping @MainActor (Bool) -> Void) {
        guard let allowed = replyGate.takeReply(allowed) else { return }
        reply(allowed)
    }
}
