import Darwin
import Foundation

/// The requesting side of the hydration channel, as the File Provider
/// extension consumes it: one call per fetch, progress streamed through a
/// callback, the result the staged content's location.
///
/// A protocol so the fetch logic is exercisable without sockets or an
/// agent; ``AgentHydrationClient`` is the real transport.
public protocol HydrationRequesting: Sendable {
    /// Performs one hydration. Progress callbacks arrive on an arbitrary
    /// background thread, strictly before the call returns.
    ///
    /// Throws ``HydrationFailure`` when the agent answered with a terminal
    /// failure, ``HydrationTransportError`` when the channel itself failed,
    /// and `CancellationError` when the surrounding task was cancelled —
    /// cancellation also tears the connection down, which is how the agent
    /// learns to stop the transfer.
    func hydrate(
        _ request: HydrationRequest,
        onProgress: @escaping @Sendable (HydrationProgress) -> Void
    ) async throws -> HydratedContent
}

/// Why the hydration channel itself failed (as opposed to the agent
/// answering with a ``HydrationFailure``).
public enum HydrationTransportError: Error, Equatable {
    /// No agent is listening at the socket — not running, or not hosting
    /// the engine. The normal "companion not up" answer, which callers
    /// surface as a transient service condition.
    case agentUnavailable(path: String)
    /// The agent stopped answering within the idle timeout.
    case timedOut(path: String)
    /// The agent broke the wire contract (early close, malformed or
    /// oversized event).
    case protocolViolation(detail: String)
}

/// The real hydration client: blocking socket I/O on a utility queue —
/// never on the cooperative pool — bridged into async, with cancellation
/// delivered as `shutdown(2)` so a blocked read unblocks promptly.
public final class AgentHydrationClient: HydrationRequesting, @unchecked Sendable {
    private let resolveSocketURL: @Sendable () throws -> URL
    private let idleTimeout: Duration
    private let queue = DispatchQueue(
        label: "com.reluxworks.gramdrive.hydration.client",
        qos: .utility,
        attributes: .concurrent)

    /// - Parameters:
    ///   - socketURL: resolves the endpoint per call (the data root may not
    ///     be resolvable at construction time).
    ///   - idleTimeout: cap on the silence *between* events. A healthy
    ///     transfer keeps progress flowing; total transfer time is
    ///     deliberately uncapped.
    public init(
        socketURL: @escaping @Sendable () throws -> URL,
        idleTimeout: Duration = .seconds(60)
    ) {
        self.resolveSocketURL = socketURL
        self.idleTimeout = idleTimeout
    }

    public func hydrate(
        _ request: HydrationRequest,
        onProgress: @escaping @Sendable (HydrationProgress) -> Void
    ) async throws -> HydratedContent {
        let connection = HydrationConnection()
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                queue.async {
                    continuation.resume(
                        with: Result {
                            try self.exchange(
                                request, over: connection, onProgress: onProgress)
                        })
                }
            }
        } onCancel: {
            connection.cancel()
        }
    }

    // MARK: - Blocking exchange

    private func exchange(
        _ request: HydrationRequest,
        over connection: HydrationConnection,
        onProgress: @escaping @Sendable (HydrationProgress) -> Void
    ) throws -> HydratedContent {
        let path = try resolveSocketURL().path
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else {
            throw UnixSocketError.failed(operation: "socket", code: errno)
        }
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        guard connection.adopt(descriptor: fd) else {
            // Cancelled before the socket existed; the connection never took
            // ownership, so close the descriptor here and stop.
            close(fd)
            throw CancellationError()
        }
        // The connection now owns the descriptor: `finish()` closes it once
        // as the exchange unwinds and retires it so a racing `cancel()`
        // cannot `shutdown()` a reused number.
        defer { connection.finish() }

        do {
            try UnixSocketAddress.connect(descriptor: fd, path: path)
        } catch let UnixSocketError.failed(_, code)
            where code == ENOENT || code == ECONNREFUSED
        {
            if connection.isCancelled { throw CancellationError() }
            throw HydrationTransportError.agentUnavailable(path: path)
        }

        var noSigpipe: Int32 = 1
        _ = setsockopt(
            fd, SOL_SOCKET, SO_NOSIGPIPE,
            &noSigpipe, socklen_t(MemoryLayout<Int32>.size))
        let timeoutSeconds = max(1, Int(idleTimeout.components.seconds))
        var timeout = timeval(tv_sec: timeoutSeconds, tv_usec: 0)
        _ = setsockopt(
            fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))
        _ = setsockopt(
            fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size))

        try send(HydrationWire.encodeLine(request), on: fd, connection: connection)
        return try readEvents(
            on: fd, path: path, connection: connection, onProgress: onProgress)
    }

    private func send(_ data: Data, on fd: Int32, connection: HydrationConnection) throws {
        try data.withUnsafeBytes { (bytes: UnsafeRawBufferPointer) in
            var offset = 0
            while offset < bytes.count {
                let written = write(fd, bytes.baseAddress! + offset, bytes.count - offset)
                guard written > 0 else {
                    if connection.isCancelled { throw CancellationError() }
                    throw UnixSocketError.failed(operation: "write", code: errno)
                }
                offset += written
            }
        }
    }

    /// Reads `\n`-framed events until the terminal one. Bounded memory: the
    /// buffer never exceeds one capped line plus one read chunk.
    private func readEvents(
        on fd: Int32,
        path: String,
        connection: HydrationConnection,
        onProgress: @escaping @Sendable (HydrationProgress) -> Void
    ) throws -> HydratedContent {
        var buffer = Data()
        var chunk = [UInt8](repeating: 0, count: 32 * 1024)
        while true {
            while let lineEnd = buffer.firstIndex(of: 0x0A) {
                let line = buffer.subdata(in: buffer.startIndex..<lineEnd)
                buffer.removeSubrange(buffer.startIndex...lineEnd)
                let event: HydrationEvent
                do {
                    event = try HydrationWire.decodeLine(HydrationEvent.self, from: line)
                } catch {
                    throw HydrationTransportError.protocolViolation(detail: "undecodable event")
                }
                switch event {
                case .progress(let progress):
                    onProgress(progress)
                case .done(let content):
                    return content
                case .failure(let failure):
                    throw failure
                }
            }
            guard buffer.count <= HydrationContract.maxEventLineBytes else {
                throw HydrationTransportError.protocolViolation(detail: "event line too long")
            }
            let count = read(fd, &chunk, chunk.count)
            if count == 0 {
                if connection.isCancelled { throw CancellationError() }
                throw HydrationTransportError.protocolViolation(
                    detail: "connection closed before a terminal event")
            }
            guard count > 0 else {
                let code = errno
                if connection.isCancelled { throw CancellationError() }
                if code == EAGAIN || code == EWOULDBLOCK {
                    throw HydrationTransportError.timedOut(path: path)
                }
                throw UnixSocketError.failed(operation: "read", code: code)
            }
            buffer.append(contentsOf: chunk[0..<count])
        }
    }
}

/// The cancellation rendezvous of one hydration call: `cancel()` may arrive
/// on any thread at any moment, before or after the socket exists, and stays
/// legal until `withTaskCancellationHandler` returns — i.e. past the moment
/// the descriptor is closed.
///
/// The connection owns the descriptor once adopted. During the exchange,
/// `cancel()` shuts the live descriptor down, which unblocks a blocked read
/// with EOF and is the wire's cancel to the server. As the exchange unwinds,
/// `finish()` closes the descriptor exactly once and retires it under the
/// lock; a `cancel()` racing that unwind then finds no descriptor and skips
/// `shutdown()`, so it can never hit a number the OS has already handed to
/// an unrelated fd. This mirrors the server's `Connection.finish()`
/// closed-flag guard.
///
/// Internal (not private) so the late-cancel guard is unit-testable.
final class HydrationConnection: @unchecked Sendable {
    private let lock = NSLock()
    private var descriptor: Int32?
    private var cancelled = false
    private var closed = false

    /// Hands the descriptor to the connection. Returns `false` when the
    /// call was cancelled before the socket existed — the caller then closes
    /// the fd itself and stops without connecting.
    func adopt(descriptor: Int32) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if cancelled { return false }
        self.descriptor = descriptor
        return true
    }

    var isCancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return cancelled
    }

    /// Closes the adopted descriptor exactly once and retires it from the
    /// connection's view, so a later `cancel()` is a no-op rather than a
    /// `shutdown()` on a potentially reused descriptor number.
    func finish() {
        lock.lock()
        let fd = descriptor
        descriptor = nil
        let wasClosed = closed
        closed = true
        lock.unlock()
        if !wasClosed, let fd {
            close(fd)
        }
    }

    func cancel() {
        lock.lock()
        defer { lock.unlock() }
        cancelled = true
        if !closed, let descriptor {
            shutdown(descriptor, SHUT_RDWR)
        }
    }
}
