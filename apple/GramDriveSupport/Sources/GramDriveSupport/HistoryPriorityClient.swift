import Darwin
import Dispatch
import Foundation

/// Best-effort, bounded sender for File Provider history-priority hints.
///
/// `signal` only appends to a small in-process queue and schedules at most one
/// worker, so item lookup/enumeration never waits for an agent socket. Missing
/// agents and torn connections are harmless: background crawl remains the
/// durable fallback and later provider callbacks send fresh hints.
public final class AgentHistoryPriorityClient: HistoryPrioritySignaling, @unchecked Sendable {
    public static let defaultPendingLimit = 256

    private let lock = NSLock()
    private let queue = DispatchQueue(
        label: "com.reluxworks.gramdrive.history-priority", qos: .utility)
    private let socketURL: @Sendable () throws -> URL
    private var pending: HistoryPriorityPendingQueue
    private var workerScheduled = false

    public init(
        socketURL: @escaping @Sendable () throws -> URL,
        pendingLimit: Int = AgentHistoryPriorityClient.defaultPendingLimit
    ) {
        self.socketURL = socketURL
        self.pending = HistoryPriorityPendingQueue(limit: pendingLimit)
    }

    public func signal(_ request: HistoryPriorityRequest) {
        guard request.accountId > 0, request.chatId != 0 else { return }
        lock.lock()
        pending.enqueue(request)
        let shouldSchedule = !workerScheduled && !pending.isEmpty
        if shouldSchedule { workerScheduled = true }
        lock.unlock()
        if shouldSchedule {
            queue.async { [weak self] in self?.drain() }
        }
    }

    private func drain() {
        while true {
            lock.lock()
            guard let request = pending.popNext() else {
                workerScheduled = false
                lock.unlock()
                return
            }
            lock.unlock()
            try? send(request)
        }
    }

    private func send(_ request: HistoryPriorityRequest) throws {
        let socket = try socketURL()
        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else {
            throw UnixSocketError.failed(operation: "socket", code: errno)
        }
        defer { close(descriptor) }
        _ = fcntl(descriptor, F_SETFD, FD_CLOEXEC)
        try UnixSocketAddress.connect(descriptor: descriptor, path: socket.path)
        var noSigpipe: Int32 = 1
        _ = setsockopt(
            descriptor, SOL_SOCKET, SO_NOSIGPIPE,
            &noSigpipe, socklen_t(MemoryLayout<Int32>.size))
        var timeout = timeval(tv_sec: 1, tv_usec: 0)
        _ = setsockopt(
            descriptor, SOL_SOCKET, SO_SNDTIMEO,
            &timeout, socklen_t(MemoryLayout<timeval>.size))
        _ = setsockopt(
            descriptor, SOL_SOCKET, SO_RCVTIMEO,
            &timeout, socklen_t(MemoryLayout<timeval>.size))
        let data = try HydrationWire.encodeLine(
            HistoryPriorityControlRequest(historyPriority: request))
        try data.withUnsafeBytes { (bytes: UnsafeRawBufferPointer) in
            var offset = 0
            while offset < bytes.count {
                let written = write(
                    descriptor, bytes.baseAddress! + offset, bytes.count - offset)
                guard written > 0 else {
                    throw UnixSocketError.failed(operation: "write", code: errno)
                }
                offset += written
            }
        }
        // Serialize transitions by waiting for this command's one-line
        // acknowledgement before the worker opens the next connection. This
        // wait is private to the utility queue; File Provider returned from
        // `signal` before any socket work began.
        var responseBytes = 0
        var chunk = [UInt8](repeating: 0, count: 256)
        while responseBytes <= 4 * 1024 {
            let count = read(descriptor, &chunk, chunk.count)
            guard count > 0 else {
                throw UnixSocketError.failed(operation: "read", code: count == 0 ? 0 : errno)
            }
            responseBytes += count
            if chunk[..<count].contains(0x0A) { return }
        }
        throw UnixSocketError.failed(operation: "read", code: EMSGSIZE)
    }
}

/// Bounded, best-effort delivery of aggregate File Provider health to the
/// coordinator. It follows the same callback-safe rule as history hints:
/// File Provider threads only enqueue, while all socket I/O happens later on
/// a utility queue. Reports contain counts/booleans only, never an item id.
public final class AgentProviderFetchHealthClient: ProviderFetchHealthSignaling, @unchecked Sendable {
    public static let defaultPendingLimit = 256

    private let lock = NSLock()
    private let queue = DispatchQueue(
        label: "com.reluxworks.gramdrive.provider-fetch-health", qos: .utility)
    private let socketURL: @Sendable () throws -> URL
    private let pendingLimit: Int
    private var pending: [ProviderFetchHealthReport] = []
    private var workerScheduled = false

    public init(
        socketURL: @escaping @Sendable () throws -> URL,
        pendingLimit: Int = AgentProviderFetchHealthClient.defaultPendingLimit
    ) {
        self.socketURL = socketURL
        self.pendingLimit = max(1, pendingLimit)
    }

    public func signal(_ report: ProviderFetchHealthReport) {
        lock.lock()
        if pending.count == pendingLimit {
            // Retain the most recent aggregate facts; dropping an old report
            // is preferable to blocking a provider callback or retaining
            // item-level context for replay.
            pending.removeFirst()
        }
        pending.append(report)
        let shouldSchedule = !workerScheduled
        if shouldSchedule { workerScheduled = true }
        lock.unlock()
        if shouldSchedule { queue.async { [weak self] in self?.drain() } }
    }

    private func drain() {
        while true {
            lock.lock()
            guard !pending.isEmpty else {
                workerScheduled = false
                lock.unlock()
                return
            }
            let report = pending.removeFirst()
            lock.unlock()
            try? send(report)
        }
    }

    private func send(_ report: ProviderFetchHealthReport) throws {
        let socket = try socketURL()
        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else {
            throw UnixSocketError.failed(operation: "socket", code: errno)
        }
        defer { close(descriptor) }
        _ = fcntl(descriptor, F_SETFD, FD_CLOEXEC)
        try UnixSocketAddress.connect(descriptor: descriptor, path: socket.path)
        var noSigpipe: Int32 = 1
        _ = setsockopt(descriptor, SOL_SOCKET, SO_NOSIGPIPE,
            &noSigpipe, socklen_t(MemoryLayout<Int32>.size))
        var timeout = timeval(tv_sec: 1, tv_usec: 0)
        _ = setsockopt(descriptor, SOL_SOCKET, SO_SNDTIMEO,
            &timeout, socklen_t(MemoryLayout<timeval>.size))
        _ = setsockopt(descriptor, SOL_SOCKET, SO_RCVTIMEO,
            &timeout, socklen_t(MemoryLayout<timeval>.size))
        let data = try HydrationWire.encodeLine(
            ProviderFetchHealthControlRequest(providerFetchHealth: report))
        try data.withUnsafeBytes { bytes in
            var offset = 0
            while offset < bytes.count {
                let written = write(descriptor, bytes.baseAddress! + offset, bytes.count - offset)
                guard written > 0 else {
                    throw UnixSocketError.failed(operation: "write", code: errno)
                }
                offset += written
            }
        }
        var responseBytes = 0
        var chunk = [UInt8](repeating: 0, count: 256)
        while responseBytes <= 4 * 1024 {
            let count = read(descriptor, &chunk, chunk.count)
            guard count > 0 else {
                throw UnixSocketError.failed(operation: "read", code: count == 0 ? 0 : errno)
            }
            responseBytes += count
            if chunk[..<count].contains(0x0A) { return }
        }
        throw UnixSocketError.failed(operation: "read", code: EMSGSIZE)
    }
}

/// Bounded/coalescing policy kept separate from socket I/O so saturation is
/// deterministic and directly testable.
struct HistoryPriorityPendingQueue {
    let limit: Int
    private(set) var entries: [HistoryPriorityRequest] = []
    private var priorityMode = false

    init(limit: Int) {
        self.limit = max(1, limit)
    }

    var isEmpty: Bool { entries.isEmpty }

    mutating func enqueue(_ request: HistoryPriorityRequest) {
        guard entries.count >= limit else {
            // Below saturation every transition is delivered in order. This
            // preserves requested -> visible -> background lifecycle edges.
            entries.append(request)
            return
        }
        priorityMode = true
        if let existing = entries.lastIndex(where: {
            $0.accountId == request.accountId && $0.chatId == request.chatId
        }) {
            // Under pressure the newest pending state wins for one chat,
            // including invalidation's background release.
            entries[existing] = request
            return
        }

        let lowestRank = entries.map(\.priority.queueRank).min() ?? 0
        let incomingRank = request.priority.queueRank
        // Background discovery never displaces foreground work. Foreground
        // hints replace the oldest lowest-priority entry; equal-priority
        // replacement keeps the newest requested/visible chat admitted.
        guard incomingRank > lowestRank || (incomingRank == lowestRank && incomingRank > 0),
            let victim = entries.firstIndex(where: { $0.priority.queueRank == lowestRank })
        else {
            return
        }
        entries[victim] = request
    }

    mutating func popNext() -> HistoryPriorityRequest? {
        guard !entries.isEmpty else { return nil }
        let index: Int
        if priorityMode {
            let highestRank = entries.map(\.priority.queueRank).max() ?? 0
            index = entries.firstIndex(where: { $0.priority.queueRank == highestRank }) ?? 0
        } else {
            index = 0
        }
        let request = entries.remove(at: index)
        if entries.isEmpty { priorityMode = false }
        return request
    }
}

private extension HistoryPriorityHint {
    var queueRank: Int {
        switch self {
        case .background: 0
        case .requested: 1
        case .visible: 2
        }
    }
}
