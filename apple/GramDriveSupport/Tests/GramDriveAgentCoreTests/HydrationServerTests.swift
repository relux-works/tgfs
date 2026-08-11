import Darwin
import Foundation
import GramDriveCore
import GramDriveSupport
import Testing

@testable import GramDriveAgentCore

// MARK: - Fixtures

private let sampleRequest = HydrationRequest(
    accountId: 7, itemId: "item-1", contentVersion: "v1")

private let sampleContent = HydratedContent(
    stagedPath: "/shared/cache/staged.bin", contentVersion: "v1", byteCount: 42)

/// SIGPIPE cannot be closed purely per-socket in this test process: it hosts
/// the client and the server together and drives 240+ tests in parallel, so a
/// `write` can still race a peer close in a window no single `SO_NOSIGPIPE`
/// covers (e.g. the server refuses `busy`/admission *before* reading, closing
/// while the client is mid-write). Ignoring the signal process-wide turns any
/// such race into the same `EPIPE` return `SO_NOSIGPIPE` yields, instead of
/// killing the whole run. Production keeps its per-socket `SO_NOSIGPIPE` and is
/// unaffected. Idempotent; the first socket suite to run installs it once.
let ignoreSIGPIPEInTestProcess: Void = {
    signal(SIGPIPE, SIG_IGN)
    return ()
}()

/// A scripted `ContentHydrating` driven by one handler closure.
private final class ScriptedHydrator: ContentHydrating, @unchecked Sendable {
    typealias Handler = @Sendable (
        HydrationRequest,
        @escaping @Sendable (HydrationProgress) -> Void,
        CancellationToken
    ) async throws -> HydratedContent

    private let lock = NSLock()
    private let handler: Handler
    private let releaseHandler: (@Sendable (HydratedContent) -> Void)?
    private(set) var recordedRequests: [HydrationRequest] = []
    private var recordedPriorities: [TaskPriority] = []
    private var releasedContents: [HydratedContent] = []
    private var releaseQos: [qos_class_t] = []

    init(
        _ handler: @escaping Handler,
        release: (@Sendable (HydratedContent) -> Void)? = nil
    ) {
        self.handler = handler
        self.releaseHandler = release
    }

    var requests: [HydrationRequest] {
        lock.lock()
        defer { lock.unlock() }
        return recordedRequests
    }

    var released: [HydratedContent] {
        lock.lock()
        defer { lock.unlock() }
        return releasedContents
    }

    var priorities: [TaskPriority] {
        lock.lock()
        defer { lock.unlock() }
        return recordedPriorities
    }

    var releasedAtUserInitiatedQos: Bool {
        lock.lock()
        defer { lock.unlock() }
        return !releaseQos.isEmpty && releaseQos.allSatisfy { $0 == QOS_CLASS_USER_INITIATED }
    }

    func hydrate(
        _ request: HydrationRequest,
        progress: @escaping @Sendable (HydrationProgress) -> Void,
        token: CancellationToken
    ) async throws -> HydratedContent {
        record(request)
        return try await handler(request, progress, token)
    }

    func release(_ content: HydratedContent) {
        lock.lock()
        releasedContents.append(content)
        releaseQos.append(qos_class_self())
        lock.unlock()
        releaseHandler?(content)
    }

    // `NSLock` may not be taken from an async context.
    private func record(_ request: HydrationRequest) {
        lock.lock()
        recordedRequests.append(request)
        recordedPriorities.append(Task.currentPriority)
        lock.unlock()
    }
}

/// A one-shot signal for cross-thread test rendezvous.
private final class TestSignal: @unchecked Sendable {
    private let lock = NSLock()
    private var signalled = false
    private var waiters: [CheckedContinuation<Void, Never>] = []

    func signal() {
        lock.lock()
        signalled = true
        let resumable = waiters
        waiters = []
        lock.unlock()
        for waiter in resumable {
            waiter.resume()
        }
    }

    func wait() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if signalled {
                lock.unlock()
                continuation.resume()
                return
            }
            waiters.append(continuation)
            lock.unlock()
        }
    }
}

/// One running endpoint and the ways tests talk to it.
private struct EndpointHarness {
    let server: HydrationServer
    let client: AgentHydrationClient
    let socketURL: URL
}

/// A last-value-wins recorder for progress callbacks.
private final class ProgressLog: @unchecked Sendable {
    private let lock = NSLock()
    private var events: [HydrationProgress] = []

    func append(_ progress: HydrationProgress) {
        lock.lock()
        events.append(progress)
        lock.unlock()
    }

    var all: [HydrationProgress] {
        lock.lock()
        defer { lock.unlock() }
        return events
    }
}

/// Server + real client over a temporary socket; the default admission
/// admits everything.
private func withHydrationServer<T>(
    hydrator: ScriptedHydrator,
    registry: TransferRegistry = TransferRegistry(),
    admission: @escaping @Sendable (HydrationRequest) -> HydrationAdmission = { _ in .admit },
    configuration: HydrationServerConfiguration = HydrationServerConfiguration(),
    idleTimeout: Duration = .seconds(5),
    _ body: (EndpointHarness) async throws -> T
) async throws -> T {
    let directory = FileManager.default.temporaryDirectory
        .appendingPathComponent("gramdrive-hydration-server-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    let socketURL = directory.appendingPathComponent("hydration.sock")
    let server = try HydrationServer.start(
        socketURL: socketURL,
        registry: registry,
        admission: admission,
        hydrator: hydrator,
        configuration: configuration)
    defer { server.stop() }
    let client = AgentHydrationClient(socketURL: { socketURL }, idleTimeout: idleTimeout)
    return try await body(
        EndpointHarness(server: server, client: client, socketURL: socketURL))
}

@Suite("Hydration endpoint")
struct HydrationServerTests {
    init() { _ = ignoreSIGPIPEInTestProcess }

    @Test("A hydration round-trips: request, progress stream, staged result")
    func roundTripSuccess() async throws {
        let hydrator = ScriptedHydrator { _, progress, _ in
            progress(HydrationProgress(bytesTransferred: 10, bytesTotal: 42))
            progress(HydrationProgress(bytesTransferred: 42, bytesTotal: 42))
            return sampleContent
        }
        try await withHydrationServer(hydrator: hydrator) { harness in
            let log = ProgressLog()
            let content = try await harness.client.hydrate(sampleRequest) { log.append($0) }
            #expect(content == sampleContent)
            #expect(log.all.map(\.bytesTransferred) == [10, 42])
            #expect(hydrator.requests == [sampleRequest])
        }
    }

    @Test("An admitted demand hydrator runs at user-initiated task priority")
    func admittedDemandUsesForegroundPriority() async throws {
        let hydrator = ScriptedHydrator { _, _, _ in sampleContent }
        try await withHydrationServer(hydrator: hydrator) { harness in
            _ = try await harness.client.hydrate(sampleRequest) { _ in }
            #expect(hydrator.priorities == [.high])
        }
    }

    @Test("Raw hydration never returns a scoped generated descriptor")
    func rawHydrationRefusesScopedGeneratedDescriptor() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-hydration-server-success-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let staged = directory.appendingPathComponent("generated.json")
        try Data("success".utf8).write(to: staged)
        let leased = HydratedContent(
            stagedPath: staged.path,
            contentVersion: "generated-v1",
            byteCount: 7,
            leaseID: "generated-lease")
        let hydrator = ScriptedHydrator { _, _, _ in leased }
        try await withHydrationServer(hydrator: hydrator) { harness in
            await #expect(throws: HydrationTransportError.self) {
                _ = try await harness.client.hydrate(sampleRequest) { _ in }
            }
            var waited = 0
            while hydrator.released.isEmpty && waited < 100 {
                try await Task.sleep(for: .milliseconds(5))
                waited += 1
            }
            #expect(hydrator.released == [leased])
            #expect(hydrator.releasedAtUserInitiatedQos)
            #expect(harness.server.activeConnectionCount == 0)
        }
    }

    @Test("Cancellation during materialization keeps its transferred descriptor alive")
    func cancellationDuringMaterializationKeepsDescriptorAlive() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-hydration-server-cancel-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let staged = directory.appendingPathComponent("generated.json")
        let destination = directory.appendingPathComponent("cloned.json")
        let bytes = Data("{\"generation\":\"leased-through-cancel\"}\n".utf8)
        try bytes.write(to: staged)
        let leased = HydratedContent(
            stagedPath: staged.path,
            contentVersion: "generated-v1",
            byteCount: UInt64(bytes.count),
            leaseID: "cancelled-materialization-lease")
        let cloneStarted = TestSignal()
        let allowClone = DispatchSemaphore(value: 0)
        let hydrator = ScriptedHydrator { _, _, _ in leased }

        try await withHydrationServer(hydrator: hydrator) { harness in
            let task = Task { [client = harness.client] in
                try await client.hydrateAndMaterialize(sampleRequest, onProgress: { _ in }) {
                    content in
                    cloneStarted.signal()
                    allowClone.wait()
                    try content.cloneMaterializationSource(to: destination)
                    return destination
                }
            }
            await cloneStarted.wait()
            task.cancel()
            // Post-done cancellation does not release the pathname lease
            // before the synchronous clone returns.
            #expect(hydrator.released.isEmpty)
            allowClone.signal()
            await #expect(throws: CancellationError.self) {
                _ = try await task.value
            }
            let copied = try Data(contentsOf: destination)
            #expect(copied == bytes)

            var waited = 0
            while (hydrator.released != [leased] || harness.server.activeConnectionCount != 0)
                && waited < 100
            {
                try await Task.sleep(for: .milliseconds(5))
                waited += 1
            }
            #expect(hydrator.released == [leased])
            #expect(harness.server.activeConnectionCount == 0)
        }
    }

    @Test("Generated pathname survives until File Provider materializes it")
    func generatedPathnameSurvivesUntilMaterialization() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-hydration-descriptor-handoff-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let staged = directory.appendingPathComponent("obsolete-generated.json")
        let destination = directory.appendingPathComponent("cloned.json")
        let bytes = Data("{\"generation\":\"survives-reclaim\"}\n".utf8)
        try bytes.write(to: staged)
        let leased = HydratedContent(
            stagedPath: staged.path,
            contentVersion: "generated-v1",
            byteCount: UInt64(bytes.count),
            leaseID: "descriptor-handoff-lease")
        let descriptorReceived = TestSignal()
        let hydrator = ScriptedHydrator(
            { _, _, _ in leased },
            release: { _ in
                try? FileManager.default.removeItem(at: staged)
            })

        try await withHydrationServer(hydrator: hydrator) { harness in
            let materialization = Task { [client = harness.client] in
                try await client.hydrateAndMaterialize(sampleRequest, onProgress: { _ in }) {
                    content in
                    #expect(content.transferredFileDescriptor != nil)
                    descriptorReceived.signal()
                    #expect(FileManager.default.fileExists(atPath: staged.path))
                    try content.cloneMaterializationSource(to: destination)
                    return destination
                }
            }
            await descriptorReceived.wait()
            let copied = try await materialization.value
            #expect(try Data(contentsOf: copied) == bytes)

            var waited = 0
            while harness.server.activeConnectionCount != 0 && waited < 100 {
                try await Task.sleep(for: .milliseconds(5))
                waited += 1
            }
            #expect(hydrator.released == [leased])
            #expect(harness.server.activeConnectionCount == 0)
            #expect(!FileManager.default.fileExists(atPath: staged.path))
        }
    }

    @Test("A disconnected post-done client releases its generation lease")
    func disconnectedMaterializationReleasesLease() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-hydration-server-disconnect-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let staged = directory.appendingPathComponent("generated.json")
        try Data("disconnect".utf8).write(to: staged)
        let leased = HydratedContent(
            stagedPath: staged.path,
            contentVersion: "generated-v1",
            byteCount: 10,
            leaseID: "abandoned-generated-lease")
        let hydrator = ScriptedHydrator { _, _, _ in leased }
        try await withHydrationServer(hydrator: hydrator) { harness in
            let fd = socket(AF_UNIX, SOCK_STREAM, 0)
            #expect(fd >= 0)
            try UnixSocketAddress.connect(descriptor: fd, path: harness.socketURL.path)
            let request = try HydrationWire.encodeLine(sampleRequest)
            let written = request.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
            #expect(written == request.count)
            var buffer = Data()
            var chunk = [UInt8](repeating: 0, count: 1024)
            while !buffer.contains(0x0A) {
                let received = try UnixFileDescriptorTransfer.receive(into: &chunk, on: fd)
                let count = received.count
                if let descriptor = received.fileDescriptor { close(descriptor) }
                #expect(count > 0)
                guard count > 0 else { return }
                buffer.append(contentsOf: chunk[0..<count])
            }
            let event = try HydrationWire.decodeLine(
                HydrationEvent.self, from: Data(buffer.prefix(while: { $0 != 0x0A })))
            guard case .done(let returned) = event else {
                Issue.record("expected done before the client disconnect")
                return
            }
            #expect(returned.leaseID == leased.leaseID)
            // Disconnect abandons the materialization boundary and releases
            // the server-side pathname lease without a timer.
            close(fd)
            var waited = 0
            while (harness.server.activeConnectionCount != 0 || hydrator.released.isEmpty)
                && waited < 100
            {
                try await Task.sleep(for: .milliseconds(5))
                waited += 1
            }
            #expect(harness.server.activeConnectionCount == 0)
            #expect(hydrator.released == [leased])
        }
    }

    @Test("Graceful shutdown is bounded while descriptor materialization is paused")
    func shutdownDrainsBeforePausedDescriptorMaterializationReturns() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-hydration-server-shutdown-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let staged = directory.appendingPathComponent("generated.json")
        let destination = directory.appendingPathComponent("cloned.json")
        let bytes = Data("{\"generation\":\"leased-through-shutdown\"}\n".utf8)
        try bytes.write(to: staged)
        let leased = HydratedContent(
            stagedPath: staged.path,
            contentVersion: "generated-v1",
            byteCount: UInt64(bytes.count),
            leaseID: "shutdown-materialization-lease")
        let cloneStarted = TestSignal()
        let allowClone = DispatchSemaphore(value: 0)
        let stopped = TestSignal()
        let hydrator = ScriptedHydrator(
            { _, _, _ in leased },
            release: { _ in
                // Models successor reconciliation after the bounded lifecycle
                // close releases the server's generated-path lease.
                try? FileManager.default.removeItem(at: staged)
            })

        try await withHydrationServer(hydrator: hydrator) { harness in
            let materialization = Task { [client = harness.client] in
                try await client.hydrateAndMaterialize(sampleRequest, onProgress: { _ in }) {
                    content in
                    cloneStarted.signal()
                    allowClone.wait()
                    try content.cloneMaterializationSource(to: destination)
                    return destination
                }
            }
            await cloneStarted.wait()
            let shutdown = Task {
                await harness.server.stopAndDrain(timeout: .milliseconds(20))
                stopped.signal()
            }

            // Shutdown reaches its terminal state without waiting for a
            // wedged callback: the File Provider owns the descriptor now.
            await stopped.wait()
            #expect(hydrator.released == [leased])
            #expect(harness.server.activeConnectionCount == 0)
            #expect(!FileManager.default.fileExists(atPath: staged.path))

            allowClone.signal()
            let copied = try await materialization.value
            #expect(copied == destination)
            #expect(try Data(contentsOf: copied) == bytes)
            await stopped.wait()
            _ = await shutdown.value
            #expect(hydrator.released == [leased])
            #expect(harness.server.activeConnectionCount == 0)
        }
    }

    @Test("An admission refusal is terminal and never reaches the hydrator")
    func admissionRefusalIsTerminal() async throws {
        let hydrator = ScriptedHydrator { _, _, _ in
            Issue.record("hydrator must not run")
            return sampleContent
        }
        try await withHydrationServer(
            hydrator: hydrator,
            admission: { _ in
                .refuse(
                    HydrationFailure(
                        category: .restricted, detail: "content restricted per POL-4"))
            }
        ) { harness in
            do {
                _ = try await harness.client.hydrate(sampleRequest) { _ in }
                Issue.record("expected a failure")
            } catch let failure as HydrationFailure {
                #expect(failure.category == .restricted)
            }
            #expect(hydrator.requests.isEmpty)
        }
    }

    @Test("A draining registry refuses new hydrations")
    func drainingRefusal() async throws {
        let hydrator = ScriptedHydrator { _, _, _ in sampleContent }
        let registry = TransferRegistry()
        _ = await registry.drain(gracePeriod: .zero, cancelWait: .zero)
        try await withHydrationServer(hydrator: hydrator, registry: registry) { harness in
            do {
                _ = try await harness.client.hydrate(sampleRequest) { _ in }
                Issue.record("expected a failure")
            } catch let failure as HydrationFailure {
                #expect(failure.category == .draining)
            }
            #expect(hydrator.requests.isEmpty)
        }
    }

    @Test("A hydrator failure crosses the wire with its category intact")
    func hydratorFailureCrossesTheWire() async throws {
        let hydrator = ScriptedHydrator { _, _, _ in
            throw HydrationFailure(
                category: .rateLimited, detail: "flood wait", retryAfterMs: 2_500)
        }
        try await withHydrationServer(hydrator: hydrator) { harness in
            do {
                _ = try await harness.client.hydrate(sampleRequest) { _ in }
                Issue.record("expected a failure")
            } catch let failure as HydrationFailure {
                #expect(failure.category == .rateLimited)
                #expect(failure.retryAfterMs == 2_500)
            }
        }
    }

    @Test("An FFI DriveError from an engine-backed hydrator maps by category")
    func driveErrorMapsByCategory() async throws {
        let hydrator = ScriptedHydrator { _, _, _ in
            throw DriveError.SourceUnavailable(detail: "network down")
        }
        try await withHydrationServer(hydrator: hydrator) { harness in
            do {
                _ = try await harness.client.hydrate(sampleRequest) { _ in }
                Issue.record("expected a failure")
            } catch let failure as HydrationFailure {
                #expect(failure.category == .sourceUnavailable)
            }
        }
    }

    @Test("The client disconnecting cancels the hydration through its token")
    func clientDisconnectCancels() async throws {
        let started = TestSignal()
        let observedCancel = TestSignal()
        let hydrator = ScriptedHydrator { _, _, token in
            started.signal()
            while !token.isCancelled() {
                try await Task.sleep(for: .milliseconds(10))
            }
            observedCancel.signal()
            throw HydrationFailure(category: .cancelled, detail: "cancelled")
        }
        try await withHydrationServer(hydrator: hydrator) { harness in
            let task = Task { [client = harness.client] in
                try await client.hydrate(sampleRequest) { _ in }
            }
            await started.wait()
            task.cancel()
            await #expect(throws: CancellationError.self) {
                _ = try await task.value
            }
            // The EOF monitor fired the FFI token — the engine-side cancel.
            await observedCancel.wait()
            var waited = 0
            while harness.server.activeConnectionCount != 0 && waited < 100 {
                try await Task.sleep(for: .milliseconds(5))
                waited += 1
            }
            #expect(harness.server.activeConnectionCount == 0)
        }
    }

    @Test("Hydrations register in the transfer registry while running")
    func hydrationRegistersInTheLedger() async throws {
        let started = TestSignal()
        let release = TestSignal()
        let hydrator = ScriptedHydrator { _, _, _ in
            started.signal()
            await release.wait()
            return sampleContent
        }
        let registry = TransferRegistry()
        try await withHydrationServer(hydrator: hydrator, registry: registry) { harness in
            let task = Task { [client = harness.client] in
                try await client.hydrate(sampleRequest) { _ in }
            }
            await started.wait()
            #expect(registry.pendingCount == 1)
            release.signal()
            _ = try await task.value
            // The server retires the ledger entry (`registry.end`) right after
            // it writes `done` — the very event that unblocks the client — so
            // `hydrate` can return a hair before that cleanup lands. Poll
            // (bounded) for the drain instead of sampling it synchronously and
            // racing the server's connection unwind under load.
            var pending = registry.pendingCount
            var waited = 0
            while pending != 0 && waited < 500 {
                try await Task.sleep(for: .milliseconds(5))
                pending = registry.pendingCount
                waited += 1
            }
            #expect(pending == 0)
        }
    }

    @Test("Connections beyond the concurrency bound are refused busy")
    func busyBound() async throws {
        let started = TestSignal()
        let release = TestSignal()
        let hydrator = ScriptedHydrator { _, _, _ in
            started.signal()
            await release.wait()
            return sampleContent
        }
        try await withHydrationServer(
            hydrator: hydrator,
            configuration: HydrationServerConfiguration(maxConcurrentHydrations: 1)
        ) { harness in
            let first = Task { [client = harness.client] in
                try await client.hydrate(sampleRequest) { _ in }
            }
            await started.wait()
            do {
                _ = try await harness.client.hydrate(sampleRequest) { _ in }
                Issue.record("expected the busy refusal")
            } catch let failure as HydrationFailure {
                #expect(failure.category == .busy)
            } catch is UnixSocketError {
                // The busy refusal races the client's request write: the server
                // admits before reading, so it can `refuse()` and close the
                // socket while `send()` is still writing. That surfaces a raw
                // transport fault (EPIPE/ECONNRESET) instead of a structured
                // busy event. Both are legitimate outcomes of the bounded path,
                // and Fix 2 maps exactly this transport fault to
                // `.serverUnreachable` downstream.
            } catch is HydrationTransportError {
                // Same race seen from the read side: an early close after the
                // write lands surfaces as a transport error rather than a
                // structured refusal. Still a valid bounded-concurrency answer.
            }
            release.signal()
            _ = try await first.value
        }
    }

    @Test("A malformed request line is refused, not crashed on")
    func malformedRequestRefused() async throws {
        let hydrator = ScriptedHydrator { _, _, _ in sampleContent }
        try await withHydrationServer(hydrator: hydrator) { harness in
            // Raw connection speaking garbage.
            let path = harness.socketURL.path
            let fd = socket(AF_UNIX, SOCK_STREAM, 0)
            defer { close(fd) }
            try UnixSocketAddress.connect(descriptor: fd, path: path)
            // Every production socket sets SO_NOSIGPIPE; this raw test fd must
            // too. Without it, if the server closes first under parallel load
            // this `write` raises SIGPIPE and terminates the whole test process
            // (macOS has no MSG_NOSIGNAL).
            var noSigpipe: Int32 = 1
            _ = setsockopt(
                fd, SOL_SOCKET, SO_NOSIGPIPE,
                &noSigpipe, socklen_t(MemoryLayout<Int32>.size))
            let junk = Data("this is not json\n".utf8)
            _ = junk.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
            var buffer = Data()
            var chunk = [UInt8](repeating: 0, count: 4096)
            while !buffer.contains(0x0A) {
                let count = read(fd, &chunk, chunk.count)
                guard count > 0 else { break }
                buffer.append(contentsOf: chunk[0..<count])
            }
            let line = buffer.prefix(while: { $0 != 0x0A })
            let event = try HydrationWire.decodeLine(HydrationEvent.self, from: Data(line))
            guard case .failure(let failure) = event else {
                Issue.record("expected a failure event, got \(event)")
                return
            }
            #expect(failure.category == .internalError)
            #expect(hydrator.requests.isEmpty)
        }
    }

    @Test("A foreign protocol version is refused")
    func protocolVersionMismatchRefused() async throws {
        let hydrator = ScriptedHydrator { _, _, _ in sampleContent }
        try await withHydrationServer(hydrator: hydrator) { harness in
            var request = sampleRequest
            request.protocolVersion = 99
            do {
                _ = try await harness.client.hydrate(request) { _ in }
                Issue.record("expected a failure")
            } catch let failure as HydrationFailure {
                #expect(failure.category == .internalError)
            }
            #expect(hydrator.requests.isEmpty)
        }
    }
}

// MARK: - Lifecycle wiring

@Suite struct AgentLifecycleHydrationTests {
    init() { _ = ignoreSIGPIPEInTestProcess }

    @Test("Agent admission preserves unavailable versus restricted")
    func admissionAvailabilityCategories() throws {
        #expect(AgentLifecycle.hydrationAdmissionFailure(for: .fetchable) == nil)
        #expect(
            AgentLifecycle.hydrationAdmissionFailure(for: .restricted)?.category
                == .restricted)
        #expect(
            AgentLifecycle.hydrationAdmissionFailure(for: .unavailable)?.category
                == .notFound)
    }

    @Test("A wired hydrator brings the endpoint up; admission runs over real state")
    func lifecycleServesHydration() async throws {
        try await withTemporaryDirectoryAsync { root in
            let hydrator = ScriptedHydrator { _, _, _ in sampleContent }
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(dataRoot: root, hydrator: hydrator))
            try lifecycle.start()
            defer { Task { await lifecycle.shutdown(reason: .terminate) } }

            let client = AgentHydrationClient(
                socketURL: { lifecycle.runtimeLayout.hydrationSocket })
            do {
                _ = try await client.hydrate(sampleRequest) { _ in }
                Issue.record("expected the real-state admission to refuse")
            } catch let failure as HydrationFailure {
                // No account is configured in a fresh container; the
                // store-backed admission refuses before the hydrator.
                #expect(failure.category == .notFound)
            }
            #expect(hydrator.requests.isEmpty)

            let outcome = await lifecycle.shutdown(reason: .terminate)
            #expect(outcome.abandoned == 0)
            // The endpoint is gone after shutdown.
            await #expect(throws: HydrationTransportError.self) {
                _ = try await client.hydrate(sampleRequest) { _ in }
            }
        }
    }

    @Test("The production core hydrator endpoint is offered without injection")
    func productionHydratorEndpointStarts() async throws {
        try await withTemporaryDirectoryAsync { root in
            let lifecycle = AgentLifecycle(configuration: AgentConfiguration(dataRoot: root))
            try lifecycle.start()
            #expect(
                FileManager.default.fileExists(
                    atPath: lifecycle.runtimeLayout.hydrationSocket.path))
            _ = await lifecycle.shutdown(reason: .terminate)
            #expect(
                !FileManager.default.fileExists(
                    atPath: lifecycle.runtimeLayout.hydrationSocket.path))
        }
    }
}
