import Foundation
import GramDriveCore
import Testing

@testable import GramDriveAgentCore

@Suite struct TransferRegistryTests {
    @Test func beginAndEndKeepTheCount() throws {
        let registry = TransferRegistry()
        #expect(registry.pendingCount == 0)
        let first = try registry.begin(token: nil)
        let second = try registry.begin(token: nil)
        #expect(registry.pendingCount == 2)
        registry.end(first)
        #expect(registry.pendingCount == 1)
        registry.end(second)
        registry.end(second)  // idempotent
        #expect(registry.pendingCount == 0)
    }

    @Test func drainingAnEmptyRegistryIsImmediate() async {
        let registry = TransferRegistry()
        let outcome = await registry.drain(gracePeriod: .seconds(5), cancelWait: .seconds(5))
        #expect(outcome == DrainOutcome())
    }

    @Test func drainRefusesNewWork() async throws {
        let registry = TransferRegistry()
        _ = await registry.drain(gracePeriod: .zero, cancelWait: .zero)
        #expect(throws: TransferRegistryError.draining) {
            _ = try registry.begin(token: nil)
        }
    }

    @Test func workFinishingWithinGraceCountsAsCompleted() async throws {
        let registry = TransferRegistry()
        let ticket = try registry.begin(token: nil)
        let workerReady = TestSignal()
        let releaseWorker = TestSignal()
        let worker = Task {
            workerReady.signal()
            await releaseWorker.wait()
            registry.end(ticket)
        }

        await waitUntil("the worker reaches its completion gate") {
            workerReady.isSignalled
        }
        let drain = Task {
            await registry.drain(gracePeriod: .seconds(5), cancelWait: .seconds(5))
        }
        await waitUntil("the registry starts draining") {
            registry.isDraining
        }
        releaseWorker.signal()

        let outcome = await drain.value
        await worker.value
        #expect(outcome == DrainOutcome(completed: 1, cancelled: 0, abandoned: 0))
    }

    @Test func workOutlivingGraceIsCancelledThroughItsToken() async throws {
        let registry = TransferRegistry()
        let token = CancellationToken()
        let ticket = try registry.begin(token: token)
        // An operation shaped like the real ones: runs until its token is
        // cancelled, then deregisters.
        let worker = Task {
            while !token.isCancelled() {
                try? await Task.sleep(for: .milliseconds(10))
            }
            registry.end(ticket)
        }
        let outcome = await registry.drain(
            gracePeriod: .milliseconds(50), cancelWait: .seconds(5))
        await worker.value
        #expect(outcome == DrainOutcome(completed: 0, cancelled: 1, abandoned: 0))
        #expect(token.isCancelled())
    }

    @Test func workIgnoringCancellationIsReportedAbandoned() async throws {
        let registry = TransferRegistry()
        _ = try registry.begin(token: nil)  // never ended, no token
        let outcome = await registry.drain(
            gracePeriod: .milliseconds(30), cancelWait: .milliseconds(60))
        #expect(outcome == DrainOutcome(completed: 0, cancelled: 0, abandoned: 1))
    }
}

private final class TestSignal: @unchecked Sendable {
    private let lock = NSLock()
    private var signalled = false
    private var waiter: CheckedContinuation<Void, Never>?

    var isSignalled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return signalled
    }

    func signal() {
        lock.lock()
        signalled = true
        let waiter = self.waiter
        self.waiter = nil
        lock.unlock()
        waiter?.resume()
    }

    func wait() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if signalled {
                lock.unlock()
                continuation.resume()
            } else {
                waiter = continuation
                lock.unlock()
            }
        }
    }
}

private func waitUntil(
    _ description: String,
    within bound: Duration = .seconds(5),
    condition: @escaping @Sendable () -> Bool,
    sourceLocation: Testing.SourceLocation = #_sourceLocation
) async {
    let deadline = ContinuousClock.now + bound
    while ContinuousClock.now < deadline {
        if condition() { return }
        try? await Task.sleep(for: .milliseconds(10))
    }
    Issue.record("timed out waiting for \(description)", sourceLocation: sourceLocation)
}
