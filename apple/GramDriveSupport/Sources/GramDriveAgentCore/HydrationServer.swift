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
///   file in the shared container, promoted atomically (SYNC-042), valid at
///   least until the request's connection closes;
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

    private var listener: Int32?
    private var acceptSource: (any DispatchSourceRead)?
    private var connections: [ObjectIdentifier: Connection] = [:]

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
        server.startAccepting()
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
            qos: .utility,
            attributes: .concurrent)
    }

    /// Hydrations currently being served (accepted and admitted).
    public var activeConnectionCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return connections.count
    }

    /// Stops accepting, tears down the socket file, and unwinds every
    /// in-flight hydration through its cancellation token. Idempotent; also
    /// runs on deallocation.
    public func stop() {
        lock.lock()
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
            connection.shutdownWire()
        }
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
        Task { [weak self] in
            do {
                let content = try await hydrator.hydrate(
                    request,
                    progress: { [weak connection] progress in
                        connection?.writeEvent(.progress(progress))
                    },
                    token: connection.token)
                connection.writeEvent(.done(content))
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

    /// Admits the connection into the bounded active set.
    private func admit(_ connection: Connection) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard connections.count < configuration.maxConcurrentHydrations else {
            return false
        }
        connections[ObjectIdentifier(connection)] = connection
        return true
    }

    private func remove(_ connection: Connection) {
        lock.lock()
        connections.removeValue(forKey: ObjectIdentifier(connection))
        lock.unlock()
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
        case .Cancelled:
            return HydrationFailure(category: .cancelled, detail: "cancelled")
        case .InvalidArgument, .Internal:
            return HydrationFailure(category: .internalError, detail: "internal failure")
        }
    }
}

/// One accepted hydration connection: the descriptor, its cancellation
/// token, the serialized writer, and the disconnect monitor.
private final class Connection: @unchecked Sendable {
    let descriptor: Int32
    let token = CancellationToken()

    private let lock = NSLock()
    private var closed = false
    private var monitor: (any DispatchSourceRead)?

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
            self.token.cancel()
            self.cancelMonitor()
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

    /// Half-closes the wire from the server side (used by `stop()`); the
    /// owning flow still runs `finish()` for the actual close.
    func shutdownWire() {
        lock.lock()
        defer { lock.unlock() }
        guard !closed else { return }
        shutdown(descriptor, SHUT_RDWR)
    }

    /// Closes exactly once; cancels the monitor first so no source watches
    /// a dead descriptor.
    func finish() {
        lock.lock()
        let wasClosed = closed
        closed = true
        let source = monitor
        monitor = nil
        lock.unlock()
        source?.cancel()
        if !wasClosed {
            close(descriptor)
        }
    }

    private func cancelMonitor() {
        lock.lock()
        let source = monitor
        monitor = nil
        lock.unlock()
        source?.cancel()
    }

    deinit {
        finish()
    }
}
