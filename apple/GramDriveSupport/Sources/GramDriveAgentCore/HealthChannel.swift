import Darwin
import Foundation

/// The agent's bounded local IPC: a health/status endpoint on a UNIX
/// socket inside the shared container (PLAT-MAC-002's "narrow native
/// service").
///
/// Bounded means bounded in every dimension: one endpoint (health), no
/// request vocabulary at all — the server writes one JSON snapshot to each
/// accepted connection and closes; the client reads to EOF under a size
/// cap and a timeout. There is nothing to parse from the peer, so there is
/// no request-handling attack surface, and control operations stay where
/// they belong (shutdown is a signal from launchd or the app, not an IPC
/// verb). The socket lives in the App Group container, which is exactly
/// the surface the platform grants to GramDrive processes and nothing
/// else.
///
/// A UNIX socket rather than an XPC mach service, deliberately: mach
/// service registration requires the signed, bundled launchd plist, which
/// neither unit tests nor the cross-process smoke can stand up, and an
/// unprovable channel is the kind of gap this repo does not ship. The
/// transport is one type on each side; if a later decision moves agent IPC
/// to XPC, the health payload and lifecycle above it do not change.
public final class AgentHealthServer: @unchecked Sendable {
    /// Response size cap, shared with the client. A health snapshot is a
    /// few KB; a megabyte means a bug.
    public static let maxPayloadBytes = 1 << 20

    private let lock = NSLock()
    private let queue: DispatchQueue
    private var listener: Int32?
    private var acceptSource: (any DispatchSourceRead)?
    private let socketPath: String

    /// Binds, listens, and starts serving `snapshot()` to every
    /// connection. A stale socket file (from a killed predecessor) is
    /// removed first — safe because the caller holds the single-instance
    /// lock, so no live agent can own it.
    public static func start(
        socketURL: URL,
        snapshot: @escaping @Sendable () -> AgentHealthSnapshot
    ) throws -> AgentHealthServer {
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
        let server = AgentHealthServer(listener: fd, socketPath: path)
        server.startAccepting(snapshot: snapshot)
        return server
    }

    private init(listener: Int32, socketPath: String) {
        self.listener = listener
        self.socketPath = socketPath
        self.queue = DispatchQueue(label: "com.reluxworks.gramdrive.agent.health")
    }

    private func startAccepting(snapshot: @escaping @Sendable () -> AgentHealthSnapshot) {
        lock.lock()
        defer { lock.unlock() }
        guard let fd = listener else { return }
        let source = DispatchSource.makeReadSource(fileDescriptor: fd, queue: queue)
        source.setEventHandler { [weak self] in
            self?.acceptOne(snapshot: snapshot)
        }
        source.resume()
        acceptSource = source
    }

    /// Accepts and serves one connection. Serialized on the server queue:
    /// at most one response is being written at any moment, which is the
    /// concurrency bound; a slow reader can delay peers by at most the
    /// send timeout.
    private func acceptOne(snapshot: @escaping @Sendable () -> AgentHealthSnapshot) {
        lock.lock()
        let fd = listener
        lock.unlock()
        guard let fd else { return }
        let conn = accept(fd, nil, nil)
        guard conn >= 0 else { return }
        defer { close(conn) }
        _ = fcntl(conn, F_SETFD, FD_CLOEXEC)
        var sendTimeout = timeval(tv_sec: 5, tv_usec: 0)
        _ = setsockopt(
            conn, SOL_SOCKET, SO_SNDTIMEO,
            &sendTimeout, socklen_t(MemoryLayout<timeval>.size))
        // A write to a peer that already hung up raises SIGPIPE by
        // default; report EPIPE instead.
        var noSigpipe: Int32 = 1
        _ = setsockopt(
            conn, SOL_SOCKET, SO_NOSIGPIPE,
            &noSigpipe, socklen_t(MemoryLayout<Int32>.size))
        guard let payload = try? JSONEncoder().encode(snapshot()),
            payload.count <= Self.maxPayloadBytes
        else { return }
        payload.withUnsafeBytes { (bytes: UnsafeRawBufferPointer) in
            var offset = 0
            while offset < bytes.count {
                let written = write(conn, bytes.baseAddress! + offset, bytes.count - offset)
                guard written > 0 else { return }
                offset += written
            }
        }
    }

    /// Stops serving and removes the socket file. Idempotent; also runs on
    /// deallocation.
    public func stop() {
        lock.lock()
        defer { lock.unlock() }
        acceptSource?.cancel()
        acceptSource = nil
        if let fd = listener {
            close(fd)
            unlink(socketPath)
            listener = nil
        }
    }

    deinit {
        stop()
    }
}

/// The app-side reader of the agent's health endpoint.
public enum AgentHealthClient {
    /// Connects, reads one snapshot, decodes it.
    public static func fetch(
        socketURL: URL,
        timeout: Duration = .seconds(5)
    ) throws -> AgentHealthSnapshot {
        try JSONDecoder().decode(
            AgentHealthSnapshot.self,
            from: fetchRaw(socketURL: socketURL, timeout: timeout))
    }

    /// Connects and reads the raw JSON payload to EOF, under the shared
    /// size cap and a receive timeout.
    ///
    /// Throws ``AgentHealthClientError/agentUnavailable(path:)`` when
    /// nothing is listening — the normal "agent not running" answer, which
    /// callers branch on rather than parse.
    public static func fetchRaw(
        socketURL: URL,
        timeout: Duration = .seconds(5)
    ) throws -> Data {
        let path = socketURL.path
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw UnixSocketError.failed(operation: "socket", code: errno)
        }
        defer { close(fd) }
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        do {
            try UnixSocketAddress.connect(descriptor: fd, path: path)
        } catch let UnixSocketError.failed(operation, code)
            where code == ENOENT || code == ECONNREFUSED
        {
            _ = operation
            throw AgentHealthClientError.agentUnavailable(path: path)
        }
        let timeoutSeconds = max(1, Int(timeout.components.seconds))
        var receiveTimeout = timeval(tv_sec: timeoutSeconds, tv_usec: 0)
        _ = setsockopt(
            fd, SOL_SOCKET, SO_RCVTIMEO,
            &receiveTimeout, socklen_t(MemoryLayout<timeval>.size))
        var payload = Data()
        var buffer = [UInt8](repeating: 0, count: 64 * 1024)
        while payload.count <= AgentHealthServer.maxPayloadBytes {
            let count = read(fd, &buffer, buffer.count)
            if count == 0 { return payload }
            guard count > 0 else {
                let code = errno
                if code == EAGAIN || code == EWOULDBLOCK {
                    throw AgentHealthClientError.timedOut(path: path)
                }
                throw UnixSocketError.failed(operation: "read", code: code)
            }
            payload.append(contentsOf: buffer[0..<count])
        }
        throw AgentHealthClientError.payloadTooLarge(path: path)
    }
}

/// Why a health fetch failed.
public enum AgentHealthClientError: Error, Equatable {
    /// No agent is listening at the socket (not running, or not yet up).
    case agentUnavailable(path: String)
    /// The agent did not answer within the receive timeout.
    case timedOut(path: String)
    /// The response exceeded ``AgentHealthServer/maxPayloadBytes``.
    case payloadTooLarge(path: String)
}
