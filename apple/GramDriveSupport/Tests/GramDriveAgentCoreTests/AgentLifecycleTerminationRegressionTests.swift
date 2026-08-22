import Dispatch
import Foundation
@testable import GramDriveAgentCore
import GramDriveSupport
import Testing

private final class BlockingNamespaceSession: AgentNamespaceSessionHosting, @unchecked Sendable {
    private let lock = NSLock()
    private let closeEntered: DispatchSemaphore
    private let closeRelease: DispatchSemaphore
    private var didClose = false

    init(closeEntered: DispatchSemaphore, closeRelease: DispatchSemaphore) {
        self.closeEntered = closeEntered
        self.closeRelease = closeRelease
    }

    var closed: Bool {
        lock.withLock { didClose }
    }

    func setChatHistoryPriority(chatId _: Int64, priority _: AgentChatHistoryPriority) throws {}

    func close() {
        closeEntered.signal()
        closeRelease.wait()
        lock.withLock { didClose = true }
    }
}

private final class BlockingNamespaceBootstrapper: AgentNamespaceBootstrapping,
    @unchecked Sendable
{
    private let lock = NSLock()
    private let closeEntered: DispatchSemaphore
    private let closeRelease: DispatchSemaphore
    private var sessions: [Int64: BlockingNamespaceSession] = [:]
    private var starts: [Int64: Int] = [:]

    init(closeEntered: DispatchSemaphore, closeRelease: DispatchSemaphore) {
        self.closeEntered = closeEntered
        self.closeRelease = closeRelease
    }

    func start(
        accountId: Int64,
        onProgress _: @escaping @Sendable (AgentNamespaceProgress) -> Void
    ) throws -> any AgentNamespaceSessionHosting {
        let session = BlockingNamespaceSession(
            closeEntered: closeEntered,
            closeRelease: closeRelease
        )
        lock.withLock {
            sessions[accountId] = session
            starts[accountId, default: 0] += 1
        }
        return session
    }

    func session(accountId: Int64) -> BlockingNamespaceSession? {
        lock.withLock { sessions[accountId] }
    }

    func startCount(accountId: Int64) -> Int {
        lock.withLock { starts[accountId, default: 0] }
    }
}

private func waitForSignal(_ semaphore: DispatchSemaphore, timeout: DispatchTime) async -> Bool {
    await Task.detached {
        blockingWaitForSignal(semaphore, timeout: timeout)
    }.value
}

private func blockingWaitForSignal(
    _ semaphore: DispatchSemaphore,
    timeout: DispatchTime
) -> Bool {
    semaphore.wait(timeout: timeout) == .success
}

@Suite(.serialized)
struct AgentLifecycleTerminationRegressionTests {
    @Test func preparedTerminationBoundsAStalledNamespaceCloseAndRollsBackOnlyAfterRelease()
        async throws
    {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(
            "gramdrive-lifecycle-termination-\(UUID().uuidString)",
            isDirectory: true
        )
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let closeEntered = DispatchSemaphore(value: 0)
        let closeRelease = DispatchSemaphore(value: 0)
        let bootstrapper = BlockingNamespaceBootstrapper(
            closeEntered: closeEntered,
            closeRelease: closeRelease
        )
        let lifecycle = AgentLifecycle(
            configuration: AgentConfiguration(
                dataRoot: root,
                drainGracePeriod: .milliseconds(10),
                drainCancelWait: .milliseconds(20),
                namespaceBootstrapper: bootstrapper,
                terminationCommitLease: .seconds(30)
            )
        )
        try lifecycle.start()
        lifecycle.startNamespace(accountId: 42)
        let originalSession = try #require(bootstrapper.session(accountId: 42))
        let identity = try #require(lifecycle.healthSnapshot().processIdentity)
        let request = ControlTerminationRequest(
            expectedAgentInstanceID: identity.instanceID,
            reason: .userQuit
        )

        let shutdownReturned = DispatchSemaphore(value: 0)
        lifecycle.beginTermination(request)
        let shutdownTask = Task {
            let outcome = await lifecycle.shutdown(reason: .terminate)
            shutdownReturned.signal()
            return outcome
        }

        #expect(await waitForSignal(closeEntered, timeout: .now() + 5))
        let returnedBeforeNamespaceClose = await waitForSignal(
            shutdownReturned, timeout: .now() + 10
        )
        if !returnedBeforeNamespaceClose {
            // Keep an old synchronous implementation expected-red instead of
            // hanging the complete Swift gate indefinitely.
            closeRelease.signal()
        }
        let outcome = await shutdownTask.value

        #expect(returnedBeforeNamespaceClose)
        #expect(outcome.abandoned == 0)
        #expect(lifecycle.currentState == .terminationReady)
        #expect(!originalSession.closed)

        // A bounded prepare never releases process ownership. If AppKit
        // cancels, the lifecycle also does not claim a usable rollback until
        // that exact old owner has really stopped.
        let contender = AgentLifecycle(configuration: AgentConfiguration(dataRoot: root))
        #expect(throws: AgentStartError.self) {
            try contender.start()
        }
        var cancellation = request
        cancellation.action = .cancel
        lifecycle.cancelTermination(cancellation)
        #expect(lifecycle.currentState == .draining)
        #expect(bootstrapper.startCount(accountId: 42) == 1)

        if returnedBeforeNamespaceClose {
            closeRelease.signal()
        }
        for _ in 0 ..< 150 where lifecycle.currentState != .terminationCancelled {
            try await Task.sleep(for: .milliseconds(5))
        }
        #expect(lifecycle.currentState == .terminationCancelled)
        #expect(originalSession.closed)
        #expect(bootstrapper.startCount(accountId: 42) == 2)
        #expect(lifecycle.healthSnapshot().namespaceOwnersRestored == true)
    }
}
