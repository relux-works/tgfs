import Darwin
import Foundation
import GramDriveCore
import Testing

@testable import GramDriveSupport

/// SIGPIPE cannot be closed purely per-socket in this test process: it hosts
/// the client and a hand-rolled server together and drives 240+ tests in
/// parallel, so a `write` can still race a peer close in a window no single
/// `SO_NOSIGPIPE` covers. Ignoring the signal process-wide turns any such race
/// into the same `EPIPE` return `SO_NOSIGPIPE` yields, instead of killing the
/// whole run. Production keeps its per-socket `SO_NOSIGPIPE` and is unaffected.
/// Idempotent; the first socket suite to run installs it once. Mirrors the
/// guard in `GramDriveAgentCoreTests` (a sibling module in the same process).
let ignoreSIGPIPEInTestProcess: Void = {
    signal(SIGPIPE, SIG_IGN)
    return ()
}()

// MARK: - Scripted raw server

/// A hand-rolled single-connection socket server: accepts once, records the
/// request line, then plays a script of raw actions. This is the *other*
/// side of the wire pinned independently of the real server, so the client
/// is proven against the contract, not against a twin implementation.
private final class ScriptedSocketServer: @unchecked Sendable {
    enum Action: Sendable {
        /// Send one encoded event line.
        case send(HydrationEvent)
        /// Send raw bytes verbatim.
        case raw(Data)
        /// Close the connection.
        case close
        /// Park until the peer closes (observes the client's cancel), then
        /// signal the gate.
        case awaitPeerClose(onClosed: @Sendable () -> Void)
    }

    let socketURL: URL
    private let listener: Int32
    private let queue = DispatchQueue(label: "scripted-hydration-server")
    private let lock = NSLock()
    private var recordedRequestLine: Data?

    var requestLine: Data? {
        lock.lock()
        defer { lock.unlock() }
        return recordedRequestLine
    }

    init(socketURL: URL, script: [Action]) throws {
        self.socketURL = socketURL
        listener = socket(AF_UNIX, SOCK_STREAM, 0)
        guard listener >= 0 else {
            throw UnixSocketError.failed(operation: "socket", code: errno)
        }
        unlink(socketURL.path)
        try UnixSocketAddress.bind(descriptor: listener, path: socketURL.path)
        guard listen(listener, 4) == 0 else {
            let code = errno
            close(listener)
            throw UnixSocketError.failed(operation: "listen", code: code)
        }
        queue.async { [self] in
            serveOne(script: script)
        }
    }

    private func serveOne(script: [Action]) {
        let conn = accept(listener, nil, nil)
        guard conn >= 0 else { return }
        defer { close(conn) }
        var noSigpipe: Int32 = 1
        _ = setsockopt(
            conn, SOL_SOCKET, SO_NOSIGPIPE,
            &noSigpipe, socklen_t(MemoryLayout<Int32>.size))

        // One request line.
        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 4096)
        while !buffer.contains(0x0A) {
            let count = read(conn, &chunk, chunk.count)
            guard count > 0 else { return }
            buffer.append(contentsOf: chunk[0..<count])
        }
        lock.lock()
        recordedRequestLine = buffer.prefix(while: { $0 != 0x0A })
        lock.unlock()

        for action in script {
            switch action {
            case .send(let event):
                guard let data = try? HydrationWire.encodeLine(event) else { return }
                _ = data.withUnsafeBytes { write(conn, $0.baseAddress, $0.count) }
            case .raw(let data):
                _ = data.withUnsafeBytes { write(conn, $0.baseAddress, $0.count) }
            case .close:
                return
            case .awaitPeerClose(let onClosed):
                var probe = [UInt8](repeating: 0, count: 16)
                while true {
                    let count = read(conn, &probe, probe.count)
                    if count <= 0 { break }
                }
                onClosed()
                return
            }
        }
    }

    func stop() {
        close(listener)
        unlink(socketURL.path)
    }
}

private func withServer<T>(
    _ script: [ScriptedSocketServer.Action],
    _ body: (ScriptedSocketServer, AgentHydrationClient) async throws -> T
) async throws -> T {
    let directory = FileManager.default.temporaryDirectory
        .appendingPathComponent("gramdrive-hydration-tests-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: directory) }
    let socketURL = directory.appendingPathComponent("hydration.sock")
    let server = try ScriptedSocketServer(socketURL: socketURL, script: script)
    defer { server.stop() }
    let client = AgentHydrationClient(socketURL: { socketURL }, idleTimeout: .seconds(1))
    return try await body(server, client)
}

private let sampleRequest = HydrationRequest(
    accountId: 7, itemId: "item-1", contentVersion: "v1")

private let sampleContent = HydratedContent(
    stagedPath: "/shared/cache/staged.bin", contentVersion: "v1", byteCount: 42)

/// A progress recorder callbacks append into.
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

// MARK: - The contract itself

@Suite struct HydrationContractTests {
    @Test func thumbnailRequestRoundTripsWithItsBoundedOperation() throws {
        let request = HydrationRequest(
            accountId: 7,
            itemId: "photo",
            contentVersion: "v1",
            purpose: .thumbnail,
            maxWidthPx: 256,
            maxHeightPx: 128)
        let line = try HydrationWire.encodeLine(request)
        let decoded = try HydrationWire.decodeLine(
            HydrationRequest.self, from: line.dropLast())
        #expect(decoded == request)
        #expect(decoded.protocolVersion == 2)
    }

    @Test func socketPathRuleIsFixedUnderTheDataRoot() {
        let url = HydrationContract.socketURL(
            dataRoot: URL(fileURLWithPath: "/container/data"))
        #expect(url.path == "/container/data/agent/hydration.sock")
    }

    @Test func eventsRoundTripThroughTheWire() throws {
        let events: [HydrationEvent] = [
            .progress(HydrationProgress(bytesTransferred: 1, bytesTotal: 2)),
            .progress(HydrationProgress(bytesTransferred: 2, bytesTotal: nil)),
            .done(sampleContent),
            .failure(
                HydrationFailure(
                    category: .rateLimited, detail: "flood", retryAfterMs: 1_500)),
        ]
        for event in events {
            let line = try HydrationWire.encodeLine(event)
            #expect(line.last == 0x0A)
            #expect(!line.dropLast().contains(0x0A))
            let decoded = try HydrationWire.decodeLine(
                HydrationEvent.self, from: line.dropLast())
            #expect(decoded == event)
        }
    }

    @Test func unknownFailureCategoryFoldsToInternal() throws {
        let line = Data(#"{"category":"brand-new-category","detail":"x"}"#.utf8)
        let failure = try HydrationWire.decodeLine(HydrationFailure.self, from: line)
        #expect(failure.category == .internalError)
    }
}

// MARK: - The client over the wire

@Suite struct HydrationClientTests {
    init() { _ = ignoreSIGPIPEInTestProcess }

    @Test func aMissingSocketAnswersAgentUnavailable() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-hydration-absent-\(UUID().uuidString)")
        let socketURL = directory.appendingPathComponent("hydration.sock")
        let client = AgentHydrationClient(socketURL: { socketURL })
        await #expect(throws: HydrationTransportError.agentUnavailable(path: socketURL.path)) {
            _ = try await client.hydrate(sampleRequest) { _ in }
        }
    }

    @Test func aDeadSocketFileAnswersAgentUnavailable() async throws {
        // A socket file whose owner died: connect gets ECONNREFUSED.
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-hydration-dead-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let socketURL = directory.appendingPathComponent("hydration.sock")
        let dead = socket(AF_UNIX, SOCK_STREAM, 0)
        try UnixSocketAddress.bind(descriptor: dead, path: socketURL.path)
        close(dead)
        let client = AgentHydrationClient(socketURL: { socketURL })
        await #expect(throws: HydrationTransportError.agentUnavailable(path: socketURL.path)) {
            _ = try await client.hydrate(sampleRequest) { _ in }
        }
    }

    @Test func theRequestLineArrivesVerbatimAndEventsStreamInOrder() async throws {
        _ = try await withServer([
            .send(.progress(HydrationProgress(bytesTransferred: 10, bytesTotal: 42))),
            .send(.progress(HydrationProgress(bytesTransferred: 42, bytesTotal: 42))),
            .send(.done(sampleContent)),
        ]) { server, client in
            let log = ProgressLog()
            let content = try await client.hydrate(sampleRequest) { log.append($0) }
            #expect(content == sampleContent)
            #expect(
                log.all.map(\.bytesTransferred) == [10, 42])
            let requestLine = try #require(server.requestLine)
            let decoded = try HydrationWire.decodeLine(HydrationRequest.self, from: requestLine)
            #expect(decoded == sampleRequest)
        }
    }

    @Test func stagedBytesStayOwnedUntilMaterializationClonesThem() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-hydration-materialization-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let staged = directory.appendingPathComponent("staged.generated.json")
        let destination = directory.appendingPathComponent("cloned.generated.json")
        let bytes = Data("{\"generation\":\"before-publication\"}\n".utf8)
        try bytes.write(to: staged)

        let cloneStarted = TestSignal()
        let peerClosed = TestSignal()
        let allowClone = DispatchSemaphore(value: 0)
        let content = HydratedContent(
            stagedPath: staged.path, contentVersion: "generated-v1", byteCount: UInt64(bytes.count))
        try await withServer([
            .send(.done(content)),
            .awaitPeerClose(onClosed: { peerClosed.signal() }),
        ]) { _, client in
            let task = Task { [client] in
                try await client.hydrateAndMaterialize(sampleRequest, onProgress: { _ in }) {
                    received in
                    cloneStarted.signal()
                    allowClone.wait()
                    try FileManager.default.copyItem(
                        at: URL(fileURLWithPath: received.stagedPath), to: destination)
                    return destination
                }
            }
            await cloneStarted.wait()
            #expect(!peerClosed.isSignalled, "the client must not close before copyItem")
            allowClone.signal()
            let cloned = try await task.value
            #expect(cloned == destination)
            #expect(try Data(contentsOf: cloned) == bytes, "the clone preserves exact bytes")
            await peerClosed.wait()
        }
    }

    @Test func cancellationDuringMaterializationKeepsTheStagedBytesOwnedUntilTheCloneReturns()
        async throws
    {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-hydration-cancel-materialization-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let staged = directory.appendingPathComponent("staged.generated.json")
        let destination = directory.appendingPathComponent("cloned.generated.json")
        let bytes = Data("{\"generation\":\"cancelled-after-done\"}\n".utf8)
        try bytes.write(to: staged)

        let cloneStarted = TestSignal()
        let peerClosed = TestSignal()
        let allowClone = DispatchSemaphore(value: 0)
        let content = HydratedContent(
            stagedPath: staged.path, contentVersion: "generated-v1", byteCount: UInt64(bytes.count))
        try await withServer([
            .send(.done(content)),
            .awaitPeerClose(onClosed: { peerClosed.signal() }),
        ]) { _, client in
            let task = Task { [client] in
                try await client.hydrateAndMaterialize(sampleRequest, onProgress: { _ in }) {
                    received in
                    cloneStarted.signal()
                    allowClone.wait()
                    try FileManager.default.copyItem(
                        at: URL(fileURLWithPath: received.stagedPath), to: destination)
                    return destination
                }
            }
            await cloneStarted.wait()
            task.cancel()
            // Cancellation after `done` is recorded, but it cannot expose EOF
            // and let the server release its generated-file lease before this
            // callback has copied the exact staged bytes.
            #expect(!peerClosed.isSignalled)
            allowClone.signal()
            await #expect(throws: CancellationError.self) {
                _ = try await task.value
            }
            let copied = try Data(contentsOf: destination)
            #expect(copied == bytes)
            await peerClosed.wait()
        }
    }

    @Test func aFailureEventThrowsItsFailure() async throws {
        _ = try await withServer([
            .send(
                .failure(
                    HydrationFailure(
                        category: .versionConflict, detail: "stale", retryAfterMs: nil)))
        ]) { _, client in
            await #expect(throws: HydrationFailure.self) {
                _ = try await client.hydrate(sampleRequest) { _ in }
            }
        }
    }

    @Test func anEarlyCloseIsAProtocolViolation() async throws {
        try await withServer([
            .send(.progress(HydrationProgress(bytesTransferred: 1, bytesTotal: 2))),
            .close,
        ]) { _, client in
            do {
                _ = try await client.hydrate(sampleRequest) { _ in }
                Issue.record("expected a transport error")
            } catch let error as HydrationTransportError {
                guard case .protocolViolation = error else {
                    Issue.record("unexpected transport error \(error)")
                    return
                }
            }
        }
    }

    @Test func anOversizedEventLineIsAProtocolViolation() async throws {
        try await withServer([
            .raw(Data(repeating: UInt8(ascii: "x"), count: HydrationContract.maxEventLineBytes + 64))
        ]) { _, client in
            do {
                _ = try await client.hydrate(sampleRequest) { _ in }
                Issue.record("expected a transport error")
            } catch let error as HydrationTransportError {
                guard case .protocolViolation = error else {
                    Issue.record("unexpected transport error \(error)")
                    return
                }
            }
        }
    }

    @Test func anUndecodableEventIsAProtocolViolation() async throws {
        try await withServer([
            .raw(Data("not json\n".utf8))
        ]) { _, client in
            do {
                _ = try await client.hydrate(sampleRequest) { _ in }
                Issue.record("expected a transport error")
            } catch let error as HydrationTransportError {
                guard case .protocolViolation = error else {
                    Issue.record("unexpected transport error \(error)")
                    return
                }
            }
        }
    }

    @Test func silenceBeyondTheIdleTimeoutTimesOut() async throws {
        try await withServer([
            .send(.progress(HydrationProgress(bytesTransferred: 1, bytesTotal: 2))),
            .awaitPeerClose(onClosed: {}),
        ]) { server, client in
            do {
                _ = try await client.hydrate(sampleRequest) { _ in }
                Issue.record("expected a timeout")
            } catch let error as HydrationTransportError {
                guard case .timedOut = error else {
                    Issue.record("unexpected transport error \(error)")
                    return
                }
            }
        }
    }

    @Test func cancellationTearsTheConnectionDownAndThrowsCancellation() async throws {
        let closed = TestSignal()
        let started = TestSignal()
        try await withServer([
            .send(.progress(HydrationProgress(bytesTransferred: 1, bytesTotal: 2))),
            .awaitPeerClose(onClosed: { closed.signal() }),
        ]) { _, client in
            let task = Task {
                try await client.hydrate(sampleRequest) { _ in started.signal() }
            }
            await started.wait()
            task.cancel()
            await #expect(throws: CancellationError.self) {
                _ = try await task.value
            }
            // The server observed the disconnect — the wire-level cancel.
            await closed.wait()
        }
    }

    @Test func cancellationWinsWhenTheBlockingReadTimesOutBeforeItsResultIsDelivered() async throws {
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-hydration-cancel-delivery-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let socketURL = directory.appendingPathComponent("hydration.sock")
        let deliveryBlocked = TestSignal()
        let allowDelivery = DispatchSemaphore(value: 0)
        let server = try ScriptedSocketServer(
            socketURL: socketURL,
            script: [.awaitPeerClose(onClosed: {})])
        defer { server.stop() }
        let client = AgentHydrationClient(
            testingSocketURL: { socketURL },
            idleTimeout: .seconds(1),
            beforeResultDelivery: {
                deliveryBlocked.signal()
                allowDelivery.wait()
            })

        let task = Task {
            try await client.hydrate(sampleRequest) { _ in }
        }
        // The transport has already selected its timeout result and closed the
        // descriptor. Cancellation now lands before that result is resumed
        // into the awaiting Swift task, exactly the full-suite-load race.
        await deliveryBlocked.wait()
        task.cancel()
        allowDelivery.signal()

        await #expect(throws: CancellationError.self) {
            _ = try await task.value
        }
    }
}

// MARK: - The cancellation connection's fd guard

/// Pins ``HydrationConnection``'s descriptor lifecycle — the fix for the
/// fd-reuse race where a late `cancel()` could `shutdown()` a number the OS
/// already reused. The reuse itself is timing- and process-global and cannot
/// be forced portably (the runner executes suites in parallel, churning
/// fds), so the guard is pinned by its observable state machine instead:
/// cancel shuts a *live* descriptor down, finish closes it exactly once and
/// retires it, and a post-finish cancel is a safe no-op.
@Suite struct HydrationConnectionTests {
    @Test func adoptRefusesADescriptorOnceCancelled() {
        let connection = HydrationConnection()
        connection.cancel()
        #expect(connection.isCancelled)
        // A socket created before the cancel loses the race: adopt refuses it
        // so the exchanging thread closes it itself and never connects.
        #expect(!connection.adopt(descriptor: 42))
    }

    @Test func cancelWhileLiveShutsTheDescriptorDown() {
        // The wire cancel: while the descriptor is live, `cancel()` shuts it
        // down so a blocked read unblocks with EOF and the server observes
        // the disconnect. Preserved behavior — the guard must not weaken it.
        var pair = [Int32](repeating: -1, count: 2)
        #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &pair) == 0)
        let local = pair[0]
        let peer = pair[1]
        defer { close(peer) }

        let connection = HydrationConnection()
        #expect(connection.adopt(descriptor: local))
        connection.cancel()

        // The write half is shut, so the peer reads EOF.
        var byte: UInt8 = 0
        #expect(read(peer, &byte, 1) == 0)

        // Unwinding retires the descriptor: finish closes it exactly once and
        // a repeated finish/cancel is a safe no-op that never touches the (now
        // possibly reused) number again. Pinned as observable behaviour — the
        // absence of a double-close crash — because reading the fd number back
        // from the process-global table would race a parallel suite reusing it
        // the instant it is freed.
        connection.finish()
        connection.finish()  // no double close
        connection.cancel()  // no shutdown of a retired number
        #expect(connection.isCancelled)
    }

    @Test func cancellationDuringMaterializationDefersWireCloseUntilFinish() {
        var pair = [Int32](repeating: -1, count: 2)
        #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &pair) == 0)
        let local = pair[0]
        let peer = pair[1]
        defer { close(peer) }
        _ = fcntl(peer, F_SETFL, O_NONBLOCK)

        let connection = HydrationConnection()
        #expect(connection.adopt(descriptor: local))
        #expect(connection.beginMaterialization())
        connection.cancel()
        #expect(connection.isCancelled)

        // Cancellation is remembered but must not publish EOF while a clone
        // may still be reading the generated source.
        var byte: UInt8 = 0
        #expect(read(peer, &byte, 1) == -1)
        #expect(errno == EAGAIN || errno == EWOULDBLOCK)

        connection.finish()
        #expect(read(peer, &byte, 1) == 0)
    }

    @Test func finishRetiresTheDescriptorSoALaterCancelIsANoOp() {
        // A socketpair gives a peer to observe the close through, so "finish
        // closed the descriptor" is proven without reading the fd number back
        // from the process-global table (a parallel suite can reuse it the
        // instant it is freed, which made a raw `fcntl` check flaky).
        var pair = [Int32](repeating: -1, count: 2)
        #expect(socketpair(AF_UNIX, SOCK_STREAM, 0, &pair) == 0)
        let local = pair[0]
        let peer = pair[1]
        defer { close(peer) }
        // Non-blocking so a regression (finish not closing) fails fast with
        // EAGAIN instead of hanging the read.
        _ = fcntl(peer, F_SETFL, O_NONBLOCK)

        let connection = HydrationConnection()
        #expect(connection.adopt(descriptor: local))

        connection.finish()
        // Closing `local` propagates EOF to its peer: read returns 0. A still-
        // open descriptor would return -1/EAGAIN instead.
        var byte: UInt8 = 0
        #expect(read(peer, &byte, 1) == 0)

        // The late cancel — legal until the task's cancellation handler
        // deregisters — finds no descriptor and never `shutdown()`s a
        // possibly reused number. Safe and idempotent.
        connection.cancel()
        #expect(connection.isCancelled)
        connection.finish()  // no double close
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

    var isSignalled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return signalled
    }
}
