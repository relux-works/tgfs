import Darwin
import Dispatch
import Foundation
import GramDriveSupport

/// Channel-level failures of the control client, disjoint from the typed
/// command failures the agent answers with (same split as the hydration
/// client's transport errors).
public enum ControlTransportError: Error, Equatable {
    /// Nothing is listening at the socket — the agent is not running.
    case agentUnavailable(path: String)
    /// The agent did not answer within the timeout.
    case timedOut(path: String)
    /// The channel broke protocol (oversized, undecodable, torn mid-event).
    case protocolViolation(detail: String)
}

/// The requesting side of the control channel: one-shot commands and the
/// interactive auth connection. Blocking socket I/O by design — callers
/// (the companion backend) bridge onto their own utility queues, exactly
/// like the health client.
public enum ControlClient {
    /// Runs one command: connect, send the request line, read the single
    /// terminal event. `timeout` caps both the connect-to-answer wait and
    /// each socket read.
    public static func command(
        _ request: ControlRequest,
        socketURL: URL,
        timeout: Duration = .seconds(30)
    ) throws -> ControlEvent {
        let descriptor = try connect(socketURL: socketURL, receiveTimeout: timeout)
        defer { close(descriptor) }
        try writeLine(request, to: descriptor, path: socketURL.path)
        var buffer = Data()
        return try readEvent(
            from: descriptor, path: socketURL.path, buffer: &buffer)
    }

    // MARK: - Shared plumbing (also used by ControlAuthChannel)

    static func connect(socketURL: URL, receiveTimeout: Duration) throws -> Int32 {
        let path = socketURL.path
        let descriptor = socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else {
            throw UnixSocketError.failed(operation: "socket", code: errno)
        }
        _ = fcntl(descriptor, F_SETFD, FD_CLOEXEC)
        do {
            try UnixSocketAddress.connect(descriptor: descriptor, path: path)
        } catch UnixSocketError.failed(let operation, let code)
            where code == ENOENT || code == ECONNREFUSED
        {
            _ = operation
            close(descriptor)
            throw ControlTransportError.agentUnavailable(path: path)
        } catch {
            close(descriptor)
            throw error
        }
        var noSigpipe: Int32 = 1
        _ = setsockopt(
            descriptor, SOL_SOCKET, SO_NOSIGPIPE,
            &noSigpipe, socklen_t(MemoryLayout<Int32>.size))
        var sendTimeout = timeval(tv_sec: 5, tv_usec: 0)
        _ = setsockopt(
            descriptor, SOL_SOCKET, SO_SNDTIMEO,
            &sendTimeout, socklen_t(MemoryLayout<timeval>.size))
        let receiveSeconds = max(1, Int(receiveTimeout.components.seconds))
        var receive = timeval(tv_sec: receiveSeconds, tv_usec: 0)
        _ = setsockopt(
            descriptor, SOL_SOCKET, SO_RCVTIMEO,
            &receive, socklen_t(MemoryLayout<timeval>.size))
        return descriptor
    }

    static func writeLine<T: Encodable>(_ value: T, to descriptor: Int32, path: String) throws {
        let data: Data
        do {
            data = try HydrationWire.encodeLine(value)
        } catch {
            throw ControlTransportError.protocolViolation(detail: "unencodable request")
        }
        var failure: ControlTransportError?
        data.withUnsafeBytes { (bytes: UnsafeRawBufferPointer) in
            var offset = 0
            while offset < bytes.count {
                let written = write(descriptor, bytes.baseAddress! + offset, bytes.count - offset)
                guard written > 0 else {
                    failure = .protocolViolation(detail: "request write failed")
                    return
                }
                offset += written
            }
        }
        if let failure {
            throw failure
        }
    }

    /// Reads one event line into `buffer`-persistent framing.
    static func readEvent(
        from descriptor: Int32, path: String, buffer: inout Data
    ) throws -> ControlEvent {
        var chunk = [UInt8](repeating: 0, count: 32 * 1024)
        while true {
            if let lineEnd = buffer.firstIndex(of: 0x0A) {
                let line = buffer.subdata(in: buffer.startIndex..<lineEnd)
                buffer.removeSubrange(buffer.startIndex...lineEnd)
                do {
                    return try HydrationWire.decodeLine(ControlEvent.self, from: line)
                } catch {
                    throw ControlTransportError.protocolViolation(detail: "undecodable event")
                }
            }
            guard buffer.count <= ControlContract.maxEventLineBytes else {
                throw ControlTransportError.protocolViolation(detail: "event line too long")
            }
            let count = read(descriptor, &chunk, chunk.count)
            if count == 0 {
                throw ControlTransportError.protocolViolation(
                    detail: "connection closed before a terminal event")
            }
            if count < 0 {
                if errno == EAGAIN || errno == EWOULDBLOCK {
                    throw ControlTransportError.timedOut(path: path)
                }
                throw ControlTransportError.protocolViolation(detail: "read failed")
            }
            buffer.append(contentsOf: chunk[0..<count])
        }
    }
}

/// One open sign-in connection: the client side of the control channel's
/// auth upgrade. Events (states, submit answers, or the upgrade refusal)
/// arrive on ``events``; inputs go out through ``send(_:)``; ``close()``
/// abandons the flow (the agent cancels the sign-in on EOF).
public final class ControlAuthChannel: @unchecked Sendable {
    private let descriptor: Int32
    private let path: String
    private let lock = NSLock()
    private var closed = false
    private let stream: AsyncStream<ControlEvent>
    private let continuation: AsyncStream<ControlEvent>.Continuation

    /// Connects and sends the auth-start request; the server's answer —
    /// the first state, or a refusal — arrives on ``events``.
    public static func open(
        socketURL: URL,
        connectTimeout: Duration = .seconds(10)
    ) throws -> ControlAuthChannel {
        let descriptor = try ControlClient.connect(
            socketURL: socketURL, receiveTimeout: connectTimeout)
        do {
            try ControlClient.writeLine(
                ControlRequest(operation: .authStart), to: descriptor, path: socketURL.path)
        } catch {
            Darwin.close(descriptor)
            throw error
        }
        // The session idles at human speed; only connecting had a deadline.
        var unbounded = timeval(tv_sec: 0, tv_usec: 0)
        _ = setsockopt(
            descriptor, SOL_SOCKET, SO_RCVTIMEO,
            &unbounded, socklen_t(MemoryLayout<timeval>.size))
        return ControlAuthChannel(descriptor: descriptor, path: socketURL.path)
    }

    private init(descriptor: Int32, path: String) {
        self.descriptor = descriptor
        self.path = path
        (self.stream, self.continuation) = AsyncStream.makeStream(of: ControlEvent.self)
        startReading()
    }

    /// The server's events, in order. Finishes when the connection ends —
    /// the session completing, either side closing, or a protocol breach.
    public var events: AsyncStream<ControlEvent> {
        stream
    }

    /// Sends one input frame.
    public func send(_ frame: ControlAuthInputFrame) throws {
        lock.lock()
        defer { lock.unlock() }
        guard !closed else {
            throw ControlTransportError.protocolViolation(detail: "channel is closed")
        }
        try ControlClient.writeLine(frame, to: descriptor, path: path)
    }

    /// Abandons the flow; the agent observes EOF and cancels the sign-in.
    /// Idempotent.
    public func close() {
        lock.lock()
        let wasClosed = closed
        closed = true
        lock.unlock()
        guard !wasClosed else { return }
        shutdown(descriptor, SHUT_RDWR)
        // The reader observes the shutdown, finishes the stream, and closes
        // the descriptor exactly once on its way out.
    }

    private func startReading() {
        let queue = DispatchQueue(
            label: "com.reluxworks.gramdrive.control.auth-reader", qos: .utility)
        queue.async { [weak self] in
            guard let self else { return }
            var buffer = Data()
            while true {
                do {
                    let event = try ControlClient.readEvent(
                        from: self.descriptor, path: self.path, buffer: &buffer)
                    self.continuation.yield(event)
                } catch {
                    break
                }
            }
            self.continuation.finish()
            self.retire()
        }
    }

    private func retire() {
        lock.lock()
        closed = true
        lock.unlock()
        Darwin.close(descriptor)
    }

    deinit {
        self.close()
    }
}
