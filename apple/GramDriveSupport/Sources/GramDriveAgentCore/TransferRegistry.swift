import Foundation
import GramDriveCore

/// Receipt for one in-flight operation; hand it back to
/// ``TransferRegistry/end(_:)`` when the operation finishes, however it
/// finishes.
public struct TransferTicket: Equatable, Sendable {
    fileprivate let id: UInt64
}

/// How a drain concluded, per operation.
public struct DrainOutcome: Equatable, Sendable {
    /// Operations that finished on their own within the grace period.
    public var completed: Int
    /// Operations cancelled after the grace period and then finished.
    public var cancelled: Int
    /// Operations still pending after cancellation and the cancel wait — a
    /// bug in the operation (a missed cancellation point), reported rather
    /// than hidden.
    public var abandoned: Int

    public init(completed: Int = 0, cancelled: Int = 0, abandoned: Int = 0) {
        self.completed = completed
        self.cancelled = cancelled
        self.abandoned = abandoned
    }
}

/// The agent's ledger of in-flight transfers, and the machinery of a clean
/// shutdown: no new work once draining, a grace period for work to finish,
/// cancellation (through each operation's FFI `CancellationToken`) for
/// work that does not.
///
/// The ledger is process-local bookkeeping only. Durable transfer state
/// lives in the engine's database; after a crash the ledger is simply
/// empty and startup reconciliation resumes from durable state — which is
/// why recovery involves no duplicate-work risk from this side.
public final class TransferRegistry: @unchecked Sendable {
    private let lock = NSLock()
    private var nextId: UInt64 = 0
    private var entries: [UInt64: CancellationToken?] = [:]
    private var draining = false
    private var drainReadinessWaiters: [CheckedContinuation<Void, Never>] = []

    public init() {}

    /// Registers one operation. `token` is the operation's cancellation
    /// token when it has one; drain uses it for operations that outlive
    /// the grace period.
    ///
    /// Throws ``TransferRegistryError/draining`` once a drain has begun:
    /// admission control is the first half of a clean shutdown.
    public func begin(token: CancellationToken?) throws -> TransferTicket {
        lock.lock()
        defer { lock.unlock() }
        guard !draining else { throw TransferRegistryError.draining }
        nextId += 1
        entries[nextId] = token
        return TransferTicket(id: nextId)
    }

    /// Removes one operation from the ledger. Idempotent.
    public func end(_ ticket: TransferTicket) {
        lock.lock()
        defer { lock.unlock() }
        entries.removeValue(forKey: ticket.id)
    }

    /// Reopens new-work admission when a bounded shutdown is cancelled.
    /// Existing cancellation tickets remain valid and continue to drain.
    public func resumeAdmission() {
        lock.lock()
        draining = false
        lock.unlock()
    }

    /// Operations currently in flight.
    public var pendingCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return entries.count
    }

    /// Whether a drain is currently refusing new work. This returns to false
    /// only when a cancelled shutdown explicitly reopens admission.
    public var isDraining: Bool {
        lock.lock()
        defer { lock.unlock() }
        return draining
    }

    /// Whether the registry is currently admitting new transfers. This is a
    /// health-only serving proof used after a cancelled termination: a live
    /// socket alone must not be mistaken for a recovered File Provider agent.
    public var isAcceptingNewWork: Bool {
        lock.lock()
        defer { lock.unlock() }
        return !draining
    }

    /// Suspends until a drain has synchronously closed new-work admission.
    ///
    /// This is an internal lifecycle synchronization seam. Unlike polling
    /// ``isDraining``, it does not depend on the observing task winning
    /// repeated executor slices while a drain task is waiting to start.
    func waitUntilDraining() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if draining {
                lock.unlock()
                continuation.resume()
            } else {
                drainReadinessWaiters.append(continuation)
                lock.unlock()
            }
        }
    }

    /// Drains the registry: refuses new work, waits `gracePeriod` for
    /// in-flight operations to end, cancels the remainder, then waits
    /// `cancelWait` for the cancellations to land.
    ///
    /// Idempotent in effect; a second concurrent call observes the same
    /// shrinking ledger.
    public func drain(
        gracePeriod: Duration,
        cancelWait: Duration
    ) async -> DrainOutcome {
        let pendingAtStart = closeToNewWork()

        await waitForEmpty(within: gracePeriod)

        let survivors = survivingTokens()
        let completed = pendingAtStart - survivors.count
        for token in survivors {
            token?.cancel()
        }

        await waitForEmpty(within: cancelWait)

        let abandoned = pendingCount
        return DrainOutcome(
            completed: completed,
            cancelled: survivors.count - abandoned,
            abandoned: abandoned)
    }

    // `NSLock` may not be taken from an async context; the locked steps of
    // a drain live in these sync helpers.

    private func closeToNewWork() -> Int {
        lock.lock()
        draining = true
        let pendingCount = entries.count
        let readinessWaiters = drainReadinessWaiters
        drainReadinessWaiters.removeAll(keepingCapacity: true)
        lock.unlock()

        for waiter in readinessWaiters {
            waiter.resume()
        }
        return pendingCount
    }

    private func survivingTokens() -> [CancellationToken?] {
        lock.lock()
        defer { lock.unlock() }
        return Array(entries.values)
    }

    private func waitForEmpty(within limit: Duration) async {
        let deadline = ContinuousClock.now + limit
        while pendingCount > 0, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(20))
        }
    }
}

/// Why an operation could not be admitted.
public enum TransferRegistryError: Error, Equatable {
    /// The agent is shutting down; no new work is admitted.
    case draining
}
