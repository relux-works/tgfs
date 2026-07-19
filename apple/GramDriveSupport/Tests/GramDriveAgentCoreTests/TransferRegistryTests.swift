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
        let worker = Task {
            try? await Task.sleep(for: .milliseconds(60))
            registry.end(ticket)
        }
        let outcome = await registry.drain(gracePeriod: .seconds(5), cancelWait: .seconds(5))
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
