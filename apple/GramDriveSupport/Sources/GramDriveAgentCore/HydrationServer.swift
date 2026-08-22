import Darwin
import Dispatch
import Foundation
import GramDriveCore
import GramDriveSupport

/// What actually stages bytes for a hydration request — the seam between
/// the agent's IPC surface and the transfer engine.
///
/// The production implementation drives the core's fetch/transfer engine
/// (SYNC-040..046) once the FFI exports it; the agent composes it at
/// startup. The contract the server holds it to:
///
/// - the returned ``HydratedContent/stagedPath`` names a fully verified
///   file in the shared container, promoted atomically (SYNC-042). For a
///   generated document, the server transfers an already-open descriptor and
///   retains its render-generation lease until normal peer close;
/// - `progress` is called from any background thread, monotonically, and
///   never after the call returns;
/// - `token` is honored promptly (SYNC-043): after `cancel()` the call
///   finishes quickly, typically throwing a ``HydrationFailure`` with
///   category `cancelled`;
/// - failures are thrown as ``HydrationFailure`` or FFI `DriveError`
///   (mapped by the server); anything else is reported as `internal`.
public protocol ContentHydrating: Sendable {
    func hydrate(
        _ request: HydrationRequest,
        progress: @escaping @Sendable (HydrationProgress) -> Void,
        token: CancellationToken
    ) async throws -> HydratedContent

    /// Releases a generated-document hand-off after the peer has cloned it
    /// and closed its side of the socket. Ordinary attachments and test
    /// hydrators have no lease, so the default is intentionally a no-op.
    func release(_ content: HydratedContent)
}

public extension ContentHydrating {
    func release(_ content: HydratedContent) {}
}

/// Production bridge from the agent IPC contract to the core's durable
/// hydration composition.
public final class CoreContentHydrator: ContentHydrating, @unchecked Sendable {
    private let hydrator: Hydrator

    public init(hydrator: Hydrator) {
        self.hydrator = hydrator
    }

    public func hydrate(
        _ request: HydrationRequest,
        progress: @escaping @Sendable (HydrationProgress) -> Void,
        token: CancellationToken
    ) async throws -> HydratedContent {
        if request.purpose == .thumbnail {
            guard let width = request.maxWidthPx, let height = request.maxHeightPx,
                width > 0, height > 0
            else {
                throw HydrationFailure(
                    category: .internalError, detail: "thumbnail bounds are missing")
            }
            let thumbnail = try await hydrator.thumbnail(
                accountId: request.accountId,
                itemId: request.itemId,
                contentVersion: request.contentVersion,
                maxWidthPx: width,
                maxHeightPx: height,
                token: token)
            guard let thumbnail else {
                return HydratedContent(
                    stagedPath: "", contentVersion: request.contentVersion,
                    byteCount: 0, isAvailable: false)
            }
            return HydratedContent(
                stagedPath: thumbnail.path,
                contentVersion: thumbnail.contentVersion,
                byteCount: thumbnail.byteCount,
                mimeType: thumbnail.mimeType)
        }
        let relay = CoreHydrationProgressRelay(callback: progress)
        let materialized = try await hydrator.hydrate(
            accountId: request.accountId,
            itemId: request.itemId,
            contentVersion: request.contentVersion,
            listener: relay,
            token: token)
        return HydratedContent(
            stagedPath: materialized.path,
            contentVersion: materialized.contentVersion,
            byteCount: materialized.byteCount,
            leaseID: materialized.leaseId)
    }

    public func release(_ content: HydratedContent) {
        guard let leaseID = content.leaseID else { return }
        try? hydrator.releaseHydrationLease(leaseId: leaseID)
    }
}

private final class CoreHydrationProgressRelay: ProgressListener, @unchecked Sendable {
    private let callback: @Sendable (HydrationProgress) -> Void

    init(callback: @escaping @Sendable (HydrationProgress) -> Void) {
        self.callback = callback
    }

    func onProgress(progress: TransferProgress) {
        callback(
            HydrationProgress(
                bytesTransferred: progress.bytesTransferred,
                bytesTotal: progress.bytesTotal))
    }
}

/// The agent's pre-hydration gate: everything about a request that durable
/// state alone can refuse — unknown account or item, a POL-4 availability
/// that withholds bytes, a pinned version that is no longer current — is
/// refused here, before the hydrator (and thus before any network or
/// engine work).
public enum HydrationAdmission: Sendable {
    case admit
    case refuse(HydrationFailure)
}

/// Tunables of one hydration endpoint.
public struct HydrationServerConfiguration: Sendable {
    /// Concurrent hydration bound. Connections beyond it are refused with
    /// `busy` rather than queued: the extension bounds its own concurrency
    /// below this, so a hit means a misbehaving client, and a bounded
    /// refusal is safer than an unbounded queue.
    public var maxConcurrentHydrations: Int
    /// Cap on waiting for the request line of an accepted connection.
    public var requestTimeout: Duration
    public init(
        maxConcurrentHydrations: Int = 8,
        requestTimeout: Duration = .seconds(5)
    ) {
        self.maxConcurrentHydrations = maxConcurrentHydrations
        self.requestTimeout = requestTimeout
    }
}

/// The agent's hydration endpoint: the serving side of
/// ``HydrationContract``.
///
/// One connection is one hydration. The lifecycle per connection: read the
/// single request line (size-capped, under a timeout), gate it through
/// admission and the transfer registry (drain admission control — a
/// draining agent refuses new hydrations exactly like every other
/// transfer), then run the hydrator with a fresh FFI `CancellationToken`,
/// streaming progress events back. The client closing its end is the
/// cancel: an EOF monitor fires the token, the hydrator unwinds, and the
/// connection is torn down. Every hydration is registered in the
/// ``TransferRegistry``, so shutdown drains hydrations through the same
/// grace-then-cancel machinery as every other transfer.
public final class HydrationServer: @unchecked Sendable {
    private let lock = NSLock()
    private let acceptQueue: DispatchQueue
    private let workQueue: DispatchQueue
    private let socketPath: String
    private let registry: TransferRegistry
    private let admission: @Sendable (HydrationRequest) -> HydrationAdmission
    private let hydrator: any ContentHydrating
    private let configuration: HydrationServerConfiguration
    #if GRAMDRIVE_QA_FAULT_CONTROL
    private var qaFaultControl: QAHydrationFaultControl?
    #endif

    private var listener: Int32?
    private var acceptSource: (any DispatchSourceRead)?
    private var connections: [ObjectIdentifier: Connection] = [:]
    private var stopping = false
    private var drainedWaiters: [CheckedContinuation<Void, Never>] = []

    /// Binds, listens, and starts serving. A stale socket file (from a
    /// killed predecessor) is removed first — safe because the caller holds
    /// the agent's single-instance lock.
    public static func start(
        socketURL: URL,
        registry: TransferRegistry,
        admission: @escaping @Sendable (HydrationRequest) -> HydrationAdmission,
        hydrator: any ContentHydrating,
        configuration: HydrationServerConfiguration = HydrationServerConfiguration()
    ) throws -> HydrationServer {
        let server = try makeServer(
            socketURL: socketURL,
            registry: registry,
            admission: admission,
            hydrator: hydrator,
            configuration: configuration)
        server.startAccepting()
        return server
    }

    #if GRAMDRIVE_QA_FAULT_CONTROL
    /// QA-only start surface. The symbol and its fault parser are not emitted
    /// unless the package was built with `GRAMDRIVE_QA_FAULT_CONTROL`.
    static func startQAFaultControlled(
        socketURL: URL,
        registry: TransferRegistry,
        admission: @escaping @Sendable (HydrationRequest) -> HydrationAdmission,
        hydrator: any ContentHydrating,
        faultControl: QAHydrationFaultControl,
        configuration: HydrationServerConfiguration = HydrationServerConfiguration()
    ) throws -> HydrationServer {
        let server = try makeServer(
            socketURL: socketURL,
            registry: registry,
            admission: admission,
            hydrator: hydrator,
            configuration: configuration)
        server.qaFaultControl = faultControl
        server.startAccepting()
        return server
    }
    #endif

    private static func makeServer(
        socketURL: URL,
        registry: TransferRegistry,
        admission: @escaping @Sendable (HydrationRequest) -> HydrationAdmission,
        hydrator: any ContentHydrating,
        configuration: HydrationServerConfiguration
    ) throws -> HydrationServer {
        let path = socketURL.path
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw UnixSocketError.failed(operation: "socket", code: errno)
        }
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        unlink(path)
        do {
            try UnixSocketAddress.bind(descriptor: fd, path: path)
        } catch {
            close(fd)
            throw error
        }
        guard listen(fd, 16) == 0 else {
            let code = errno
            close(fd)
            unlink(path)
            throw UnixSocketError.failed(operation: "listen", code: code)
        }
        let server = HydrationServer(
            listener: fd,
            socketPath: path,
            registry: registry,
            admission: admission,
            hydrator: hydrator,
            configuration: configuration)
        return server
    }

    private init(
        listener: Int32,
        socketPath: String,
        registry: TransferRegistry,
        admission: @escaping @Sendable (HydrationRequest) -> HydrationAdmission,
        hydrator: any ContentHydrating,
        configuration: HydrationServerConfiguration
    ) {
        self.listener = listener
        self.socketPath = socketPath
        self.registry = registry
        self.admission = admission
        self.hydrator = hydrator
        self.configuration = configuration
        self.acceptQueue = DispatchQueue(label: "com.reluxworks.gramdrive.agent.hydration")
        self.workQueue = DispatchQueue(
            label: "com.reluxworks.gramdrive.agent.hydration.work",
            qos: .userInitiated,
            attributes: .concurrent)
    }

    /// Hydrations currently being served (accepted and admitted).
    public var activeConnectionCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return connections.count
    }

    /// Stops accepting, tears down the socket file, and cancels unfinished
    /// work. A successful generated terminal event has already transferred
    /// an independent descriptor to File Provider, so it neither depends on
    /// this connection nor delays server shutdown.
    public func stop() {
        lock.lock()
        stopping = true
        acceptSource?.cancel()
        acceptSource = nil
        let listener = self.listener
        self.listener = nil
        let active = Array(connections.values)
        lock.unlock()
        if let listener {
            close(listener)
            unlink(socketPath)
        }
        for connection in active {
            connection.token.cancel()
        }
    }

    /// Gracefully stops and drains active work. With a bounded timeout, a
    /// wedged peer is force-closed after the deadline; an already-transferred
    /// generated descriptor remains valid in File Provider even then.
    public func stopAndDrain(timeout: Duration? = nil) async {
        stop()
        guard let timeout else {
            await waitForConnectionsToClose()
            return
        }
        let deadline = Task { [weak self] in
            do {
                try await Task.sleep(for: timeout)
            } catch {
                return
            }
            guard !Task.isCancelled else { return }
            self?.forceCloseConnections()
        }
        await waitForConnectionsToClose()
        deadline.cancel()
    }

    deinit {
        stop()
    }

    // MARK: - Accepting

    private func startAccepting() {
        lock.lock()
        defer { lock.unlock() }
        guard let fd = listener else { return }
        let source = DispatchSource.makeReadSource(fileDescriptor: fd, queue: acceptQueue)
        source.setEventHandler { [weak self] in
            self?.acceptOne()
        }
        source.resume()
        acceptSource = source
    }

    private func acceptOne() {
        lock.lock()
        let fd = listener
        lock.unlock()
        guard let fd else { return }
        let conn = accept(fd, nil, nil)
        guard conn >= 0 else { return }
        _ = fcntl(conn, F_SETFD, FD_CLOEXEC)
        var noSigpipe: Int32 = 1
        _ = setsockopt(
            conn, SOL_SOCKET, SO_NOSIGPIPE,
            &noSigpipe, socklen_t(MemoryLayout<Int32>.size))
        var sendTimeout = timeval(tv_sec: 5, tv_usec: 0)
        _ = setsockopt(
            conn, SOL_SOCKET, SO_SNDTIMEO,
            &sendTimeout, socklen_t(MemoryLayout<timeval>.size))
        let receiveSeconds = max(1, Int(configuration.requestTimeout.components.seconds))
        var receiveTimeout = timeval(tv_sec: receiveSeconds, tv_usec: 0)
        _ = setsockopt(
            conn, SOL_SOCKET, SO_RCVTIMEO,
            &receiveTimeout, socklen_t(MemoryLayout<timeval>.size))

        let connection = Connection(descriptor: conn)
        // The accept queue must never block; everything past accept runs on
        // the concurrent work queue.
        workQueue.async { [weak self] in
            self?.serve(connection)
        }
    }

    // MARK: - Per-connection lifecycle

    private func serve(_ connection: Connection) {
        guard admit(connection) else {
            connection.refuse(
                HydrationFailure(
                    category: .busy,
                    detail: "concurrent hydration bound reached"))
            return
        }

        let request: HydrationRequest
        do {
            request = try readRequest(on: connection)
        } catch {
            connection.refuse(
                HydrationFailure(category: .internalError, detail: "unreadable request"))
            remove(connection)
            return
        }

        guard request.protocolVersion == HydrationContract.protocolVersion else {
            connection.refuse(
                HydrationFailure(
                    category: .internalError,
                    detail: "protocol version mismatch"))
            remove(connection)
            return
        }

        if case .refuse(let failure) = admission(request) {
            connection.refuse(failure)
            remove(connection)
            return
        }

        #if GRAMDRIVE_QA_FAULT_CONTROL
        if let disposition = qaFaultControl?.disposition(for: request) {
            switch disposition {
            case .timeout:
                // The QA File Provider client uses a one-second idle bound.
                // Hold the real connection without an event long enough for
                // its transport timeout, then retire this worker.
                Thread.sleep(forTimeInterval: 2)
                connection.finish()
            case .transport:
                // EOF without a terminal event exercises the real client
                // transport mapping, not a wire-level synthetic failure.
                connection.finish()
            case .failure(let category):
                connection.refuse(
                    HydrationFailure(category: category, detail: "qa injected failure"))
            }
            remove(connection)
            return
        }
        #endif

        let ticket: TransferTicket
        do {
            ticket = try registry.begin(token: connection.token)
        } catch {
            connection.refuse(
                HydrationFailure(category: .draining, detail: "agent is shutting down"))
            remove(connection)
            return
        }

        // From here the client's only legal move is closing (its cancel);
        // watch for it so an abandoned fetch stops transferring promptly.
        connection.watchForDisconnect(on: acceptQueue)

        let hydrator = self.hydrator
        let registry = self.registry
        let workQueue = self.workQueue
        workQueue.async { [weak self] in
            do {
                // The GCD queue is the native demand execution boundary. The
                // small detached task only drives the async UniFFI wrapper;
                // the Rust hydrator owns actual work on its dedicated runtime.
                // Waiting here therefore never occupies a Swift cooperative
                // executor thread, while the user-initiated queue preserves
                // the foreground scheduling contract through terminal I/O and
                // synchronous lease reclamation.
                let content = try Self.awaitOffCooperativeExecutor {
                    try await hydrator.hydrate(
                        request,
                        progress: { [weak connection] progress in
                            connection?.writeEvent(.progress(progress))
                        },
                        token: connection.token)
                }
                // A failure while publishing the terminal event (including a
                // client disconnect) must release the Rust hand-off lease as
                // well; otherwise a generated inode remains pinned until the
                // agent exits.
                defer { hydrator.release(content) }
                if let leaseID = content.leaseID {
                    // This is the process boundary: `SCM_RIGHTS` gives File
                    // Provider an independent open reference before the old
                    // pathname can be reclaimed. A successor may therefore
                    // reconcile/reclaim immediately after this write without
                    // invalidating a paused `copyItem` in the extension.
                    let descriptor = open(content.stagedPath, O_RDONLY | O_CLOEXEC)
                    guard descriptor >= 0 else {
                        throw HydrationFailure(
                            category: .storage, detail: "could not open generated hand-off")
                    }
                    defer { close(descriptor) }
                    try connection.writeEvent(
                        .done(content), transferringFileDescriptor: descriptor)
                    _ = leaseID
                    // Keep the original immutable pathname leased through a
                    // normal File Provider materialization. The client closes
                    // only after its synchronous clone returns; the received
                    // descriptor separately makes that clone crash-safe if
                    // this process is terminated before EOF.
                    try Self.awaitOffCooperativeExecutor {
                        await connection.waitForDisconnect()
                    }
                } else {
                    connection.writeEvent(.done(content))
                }
                // EOF after normal materialization, disconnect, or the
                // lifecycle's bounded force-close releases the Rust lease and
                // ticket. A receiver-held descriptor stays valid if that
                // close races a still-running File Provider clone.
            } catch let failure as HydrationFailure {
                connection.writeEvent(.failure(failure))
            } catch is CancellationError {
                connection.writeEvent(
                    .failure(HydrationFailure(category: .cancelled, detail: "cancelled")))
            } catch let error as DriveError {
                connection.writeEvent(.failure(Self.failure(from: error)))
            } catch {
                connection.writeEvent(
                    .failure(
                        HydrationFailure(
                            category: .internalError, detail: "hydrator failure")))
            }
            registry.end(ticket)
            connection.finish()
            self?.remove(connection)
        }
    }

    /// Blocks only the server's user-initiated dispatch worker while an async
    /// boundary completes. This deliberately must not be called from a Swift
    /// task: it keeps both the async UniFFI poll and the generated-document
    /// descriptor/lease cleanup out of Swift's cooperative executor.
    private static func awaitOffCooperativeExecutor<T: Sendable>(
        _ operation: @escaping @Sendable () async throws -> T
    ) throws -> T {
        let result = AsyncOperationResult<T>()
        let settled = DispatchSemaphore(value: 0)
        Task.detached(priority: .userInitiated) {
            do {
                result.store(.success(try await operation()))
            } catch {
                result.store(.failure(error))
            }
            settled.signal()
        }
        settled.wait()
        return try result.take()
    }

    /// Admits the connection into the bounded active set.
    private func admit(_ connection: Connection) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !stopping, connections.count < configuration.maxConcurrentHydrations else {
            return false
        }
        connections[ObjectIdentifier(connection)] = connection
        return true
    }

    private func remove(_ connection: Connection) {
        lock.lock()
        connections.removeValue(forKey: ObjectIdentifier(connection))
        let waiters = connections.isEmpty ? drainedWaiters : []
        if connections.isEmpty {
            drainedWaiters = []
        }
        lock.unlock()
        for waiter in waiters {
            waiter.resume()
        }
    }

    /// Breaks only server-side ownership after a lifecycle drain deadline.
    /// Generated File Provider clients have already received independent
    /// descriptors, so this cannot invalidate their current clone.
    private func forceCloseConnections() {
        lock.lock()
        let active = Array(connections.values)
        lock.unlock()
        for connection in active {
            connection.finish()
        }
    }

    private func waitForConnectionsToClose() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if connections.isEmpty {
                lock.unlock()
                continuation.resume()
                return
            }
            drainedWaiters.append(continuation)
            lock.unlock()
        }
    }

    /// Reads the single request line, under the size cap and the socket's
    /// receive timeout.
    private func readRequest(on connection: Connection) throws -> HydrationRequest {
        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 4 * 1024)
        while buffer.count <= HydrationContract.maxRequestLineBytes {
            if let lineEnd = buffer.firstIndex(of: 0x0A) {
                let line = buffer.subdata(in: buffer.startIndex..<lineEnd)
                return try HydrationWire.decodeLine(HydrationRequest.self, from: line)
            }
            let count = read(connection.descriptor, &chunk, chunk.count)
            guard count > 0 else {
                throw UnixSocketError.failed(operation: "read", code: count == 0 ? 0 : errno)
            }
            buffer.append(contentsOf: chunk[0..<count])
        }
        throw UnixSocketError.failed(operation: "read", code: EMSGSIZE)
    }

    /// Maps an FFI error thrown by an engine-backed hydrator onto the wire
    /// categories (NFR-030 alignment).
    static func failure(from error: DriveError) -> HydrationFailure {
        switch error {
        case .NotFound:
            return HydrationFailure(category: .notFound, detail: "not found")
        case .AuthRequired:
            return HydrationFailure(category: .authRequired, detail: "authorization required")
        case .RateLimited(_, let retryAfterMs):
            return HydrationFailure(
                category: .rateLimited, detail: "rate limited", retryAfterMs: retryAfterMs)
        case .SourceUnavailable:
            return HydrationFailure(category: .sourceUnavailable, detail: "source unavailable")
        case .Storage:
            return HydrationFailure(category: .storage, detail: "storage failure")
        case .Integrity:
            return HydrationFailure(category: .integrity, detail: "integrity failure")
        case .Restricted:
            return HydrationFailure(category: .restricted, detail: "restricted")
        case .VersionConflict:
            return HydrationFailure(category: .versionConflict, detail: "version conflict")
        case .Cancelled:
            return HydrationFailure(category: .cancelled, detail: "cancelled")
        case .InvalidArgument, .Internal:
            return HydrationFailure(category: .internalError, detail: "internal failure")
        }
    }
}

/// Synchronously retrieves an async operation's result from the dedicated
/// dispatch worker. The lock is intentionally confined to this bridge; its
/// task never waits while holding it.
private final class AsyncOperationResult<Value: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var result: Result<Value, Error>?

    func store(_ result: Result<Value, Error>) {
        lock.lock()
        self.result = result
        lock.unlock()
    }

    func take() throws -> Value {
        lock.lock()
        let result = self.result
        lock.unlock()
        guard let result else {
            preconditionFailure("async operation signalled without a result")
        }
        return try result.get()
    }
}

/// One accepted hydration connection: the descriptor, its cancellation
/// token, the serialized writer, and the disconnect monitor.
private final class Connection: @unchecked Sendable {
    let descriptor: Int32
    let token = CancellationToken()

    private let lock = NSLock()
    private var closed = false
    private var disconnected = false
    private var monitor: (any DispatchSourceRead)?
    private var disconnectWaiters: [CheckedContinuation<Void, Never>] = []

    init(descriptor: Int32) {
        self.descriptor = descriptor
    }

    /// Writes one event line; serialized, best-effort (a peer that hung up
    /// makes writes fail, which is fine — EOF already cancelled the token).
    func writeEvent(_ event: HydrationEvent) {
        guard let data = try? HydrationWire.encodeLine(event) else { return }
        lock.lock()
        defer { lock.unlock() }
        guard !closed else { return }
        data.withUnsafeBytes { (bytes: UnsafeRawBufferPointer) in
            var offset = 0
            while offset < bytes.count {
                let written = write(descriptor, bytes.baseAddress! + offset, bytes.count - offset)
                guard written > 0 else { return }
                offset += written
            }
        }
    }

    /// Writes a terminal event and atomically passes an open source
    /// descriptor to the peer. The fd itself remains caller-owned.
    func writeEvent(_ event: HydrationEvent, transferringFileDescriptor descriptor: Int32) throws {
        let data = try HydrationWire.encodeLine(event)
        lock.lock()
        defer { lock.unlock() }
        guard !closed else {
            throw UnixSocketError.failed(operation: "sendmsg", code: EPIPE)
        }
        try UnixFileDescriptorTransfer.send(data, fileDescriptor: descriptor, on: self.descriptor)
    }

    /// Installs the EOF monitor: any readable event after the request line
    /// means the client closed (its cancel) or broke protocol — either way
    /// the hydration stops.
    func watchForDisconnect(on queue: DispatchQueue) {
        let source = DispatchSource.makeReadSource(fileDescriptor: descriptor, queue: queue)
        source.setEventHandler { [weak self] in
            guard let self else { return }
            var probe = [UInt8](repeating: 0, count: 1024)
            _ = read(self.descriptor, &probe, probe.count)
            // EOF, error, or unexpected bytes: nothing legal arrives here,
            // so every firing is a reason to stop.
            self.markDisconnected()
        }
        lock.lock()
        if closed {
            lock.unlock()
            return
        }
        monitor = source
        lock.unlock()
        source.resume()
    }

    /// Terminal refusal for connections that never reached the hydrator.
    func refuse(_ failure: HydrationFailure) {
        writeEvent(.failure(failure))
        finish()
    }

    /// Waits for the peer's close after a successful `done`. A connected peer
    /// may still be inside its synchronous File Provider clone, so elapsed
    /// time is never evidence that its generated source can be reclaimed.
    /// A client crash, cancellation, and normal completion all close the
    /// socket, which resumes this waiter and bounds the server-side ticket and
    /// generation lease without putting an upper bound on `copyItem`.
    func waitForDisconnect() async {
        await withCheckedContinuation { continuation in
            lock.lock()
            if disconnected || closed {
                lock.unlock()
                continuation.resume()
                return
            }
            disconnectWaiters.append(continuation)
            lock.unlock()
        }
    }

    /// Closes exactly once; cancels the monitor first so no source watches
    /// a dead descriptor.
    func finish() {
        lock.lock()
        let wasClosed = closed
        closed = true
        let source = monitor
        monitor = nil
        let waiters = disconnectWaiters
        disconnectWaiters = []
        lock.unlock()
        source?.cancel()
        if !wasClosed {
            close(descriptor)
        }
        for waiter in waiters {
            waiter.resume()
        }
    }

    private func cancelMonitor() {
        lock.lock()
        let source = monitor
        monitor = nil
        lock.unlock()
        source?.cancel()
    }

    private func markDisconnected() {
        lock.lock()
        if disconnected {
            lock.unlock()
            return
        }
        disconnected = true
        let waiters = disconnectWaiters
        disconnectWaiters = []
        lock.unlock()
        token.cancel()
        cancelMonitor()
        for waiter in waiters {
            waiter.resume()
        }
    }

    deinit {
        finish()
    }
}
