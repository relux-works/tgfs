import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport
import Testing

@testable import GramDriveFileProvider

// MARK: - Scripted hydration

/// A scripted `HydrationRequesting`: each fetch consumes the next step, and
/// every request is recorded — which is how tests pin both the outcomes and
/// the *absence* of agent contact (POL-4's zero-request refusals).
final class ScriptedHydration: HydrationRequesting, @unchecked Sendable {
    typealias Step = @Sendable (
        HydrationRequest, @escaping @Sendable (HydrationProgress) -> Void
    ) async throws -> HydratedContent

    private let lock = NSLock()
    private var steps: [Step] = []
    private(set) var recordedRequests: [HydrationRequest] = []
    /// High-water mark of concurrently running steps — what the bounded
    /// gate is measured by.
    private(set) var concurrentHighWater = 0
    private var running = 0

    var requests: [HydrationRequest] {
        lock.lock()
        defer { lock.unlock() }
        return recordedRequests
    }

    func enqueue(_ step: @escaping Step) {
        lock.lock()
        steps.append(step)
        lock.unlock()
    }

    func enqueueSuccess(_ content: HydratedContent, progress: [HydrationProgress] = []) {
        enqueue { _, onProgress in
            for update in progress {
                onProgress(update)
            }
            return content
        }
    }

    func enqueueFailure(_ failure: HydrationFailure) {
        enqueue { _, _ in throw failure }
    }

    func enqueueTransportFailure(_ error: HydrationTransportError) {
        enqueue { _, _ in throw error }
    }

    /// A raw `UnixSocketError` — what `AgentHydrationClient` throws when the
    /// channel breaks below the protocol layer (fd exhaustion, EPIPE,
    /// ECONNRESET, a sandbox-denied connect, an unrepresentable path).
    func enqueueSocketFailure(_ error: UnixSocketError) {
        enqueue { _, _ in throw error }
    }

    func hydrate(
        _ request: HydrationRequest,
        onProgress: @escaping @Sendable (HydrationProgress) -> Void
    ) async throws -> HydratedContent {
        let step = begin(request)
        defer { end() }
        guard let step else {
            throw HydrationFailure(category: .internalError, detail: "unscripted hydration call")
        }
        return try await step(request, onProgress)
    }

    // `NSLock` may not be taken from an async context; the locked
    // bookkeeping lives in these sync helpers.

    private func begin(_ request: HydrationRequest) -> Step? {
        lock.lock()
        defer { lock.unlock() }
        recordedRequests.append(request)
        running += 1
        concurrentHighWater = max(concurrentHighWater, running)
        return steps.isEmpty ? nil : steps.removeFirst()
    }

    private func end() {
        lock.lock()
        running -= 1
        lock.unlock()
    }
}

// MARK: - Async rendezvous helpers

/// A one-shot, many-reader value cell: completions fulfill it, tests await
/// it. Fulfillment is first-wins, which doubles as the called-exactly-once
/// check when paired with `fulfillmentCount`.
final class TestFuture<Value: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Value?
    private(set) var fulfillmentCount = 0
    private var waiters: [CheckedContinuation<Value, Never>] = []

    func fulfill(_ newValue: Value) {
        lock.lock()
        fulfillmentCount += 1
        guard value == nil else {
            lock.unlock()
            return
        }
        value = newValue
        let resumable = waiters
        waiters = []
        lock.unlock()
        for waiter in resumable {
            waiter.resume(returning: newValue)
        }
    }

    var settled: Value {
        get async {
            await withCheckedContinuation { continuation in
                lock.lock()
                if let value {
                    lock.unlock()
                    continuation.resume(returning: value)
                    return
                }
                waiters.append(continuation)
                lock.unlock()
            }
        }
    }
}

/// A reusable open/wait gate: steps park on `waitUntilOpen()` until the
/// test calls `open()`. Latching — late waiters pass straight through.
final class ManualGate: @unchecked Sendable {
    private let lock = NSLock()
    private var isOpen = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func open() {
        lock.lock()
        isOpen = true
        let resumable = waiters
        waiters = []
        lock.unlock()
        for waiter in resumable {
            waiter.resume()
        }
    }

    func waitUntilOpen() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if isOpen {
                lock.unlock()
                continuation.resume()
                return
            }
            waiters.append(continuation)
            lock.unlock()
        }
    }
}

/// A latching counter tests await: steps `signal()` arrival, tests await a
/// target count.
final class ArrivalCounter: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0
    private var waiters: [(target: Int, continuation: CheckedContinuation<Void, Never>)] = []

    func signal() {
        lock.lock()
        count += 1
        let current = count
        var resumable: [CheckedContinuation<Void, Never>] = []
        waiters.removeAll { waiter in
            guard waiter.target <= current else { return false }
            resumable.append(waiter.continuation)
            return true
        }
        lock.unlock()
        for continuation in resumable {
            continuation.resume()
        }
    }

    func waitFor(_ target: Int) async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if count >= target {
                lock.unlock()
                continuation.resume()
                return
            }
            waiters.append((target, continuation))
            lock.unlock()
        }
    }
}

// MARK: - Fetch-completion recording

/// One recorded `fetchContents` completion.
struct FetchOutcome: @unchecked Sendable {
    var url: URL?
    var item: (any NSFileProviderItem)?
    var error: (any Error)?

    var nsError: NSError? {
        error.map { $0 as NSError }
    }

    /// The returned item as the concrete mapped type.
    var fetchedItem: GramDriveFileProviderItem? {
        item as? GramDriveFileProviderItem
    }

    func expectProviderError(_ code: NSFileProviderError.Code) {
        #expect(url == nil)
        #expect(nsError?.domain == NSFileProviderError.errorDomain)
        #expect(nsError?.code == code.rawValue)
    }

    func expectCocoaError(_ code: CocoaError.Code) {
        #expect(url == nil)
        #expect(nsError?.domain == CocoaError.errorDomain)
        #expect(nsError?.code == code.rawValue)
    }
}

/// A scratch directory for one test, removed afterwards.
func withFetchScratchDirectory<T>(_ body: (URL) async throws -> T) async throws -> T {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent("gramdrive-fetch-tests-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: url) }
    return try await body(url)
}

/// Writes a staged content file and returns its path.
func stageContent(_ bytes: Data, in directory: URL, name: String = "staged.bin") throws -> String {
    let url = directory.appendingPathComponent(name, isDirectory: false)
    try bytes.write(to: url)
    return url.path
}
