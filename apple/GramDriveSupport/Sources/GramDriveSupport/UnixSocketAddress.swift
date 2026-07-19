import Darwin
import Foundation

/// `bind`/`connect` for UNIX sockets at paths of any length.
///
/// `sockaddr_un.sun_path` holds at most 103 bytes plus NUL on macOS, and
/// App Group container paths routinely exceed that. When the absolute path
/// fits, it is used directly; when it does not, the operation runs with the
/// process working directory temporarily moved to the socket's parent (the
/// classic `fchdir` + relative-leaf technique). The working directory is
/// process-global state, so long-path operations serialize behind one lock
/// and restore the original directory before returning — every GramDrive
/// process uses absolute paths elsewhere, and socket setup is rare
/// (startup, health probes, one hydration connect per fetch), so the brief
/// cwd excursion is bounded and contained.
///
/// Lives in the support package because both sides of every agent IPC
/// channel need it: the agent binds (`GramDriveAgentCore`), while clients —
/// the app and the File Provider extension — connect.
public enum UnixSocketAddress {
    /// Maximum byte length of a directly representable path (excludes NUL).
    public static let maxDirectPathLength = MemoryLayout.size(ofValue: sockaddr_un().sun_path) - 1

    private static let workingDirectoryLock = NSLock()

    public static func bind(descriptor: Int32, path: String) throws {
        try operate(on: descriptor, path: path, operation: "bind") { fd, addr, len in
            Darwin.bind(fd, addr, len)
        }
    }

    public static func connect(descriptor: Int32, path: String) throws {
        try operate(on: descriptor, path: path, operation: "connect") { fd, addr, len in
            Darwin.connect(fd, addr, len)
        }
    }

    private static func operate(
        on descriptor: Int32,
        path: String,
        operation: String,
        _ call: (Int32, UnsafePointer<sockaddr>, socklen_t) -> Int32
    ) throws {
        if path.utf8.count <= maxDirectPathLength {
            try withSockaddrUn(path: path) { addr, len in
                guard call(descriptor, addr, len) == 0 else {
                    throw UnixSocketError.failed(operation: operation, code: errno)
                }
            }
            return
        }
        let url = URL(fileURLWithPath: path)
        let leaf = url.lastPathComponent
        guard leaf.utf8.count <= maxDirectPathLength else {
            throw UnixSocketError.pathUnrepresentable(path: path)
        }
        try inWorkingDirectory(url.deletingLastPathComponent().path) {
            try withSockaddrUn(path: leaf) { addr, len in
                guard call(descriptor, addr, len) == 0 else {
                    throw UnixSocketError.failed(operation: operation, code: errno)
                }
            }
        }
    }

    private static func withSockaddrUn(
        path: String,
        _ body: (UnsafePointer<sockaddr>, socklen_t) throws -> Void
    ) throws {
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let copied: Bool = withUnsafeMutableBytes(of: &address.sun_path) { buffer in
            let bytes = Array(path.utf8)
            guard bytes.count < buffer.count else { return false }
            buffer.baseAddress!.copyMemory(from: bytes, byteCount: bytes.count)
            buffer[bytes.count] = 0
            return true
        }
        guard copied else {
            throw UnixSocketError.pathUnrepresentable(path: path)
        }
        address.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        try withUnsafePointer(to: &address) { pointer in
            try pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { addr in
                try body(addr, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
    }

    /// Runs `body` with the process cwd at `directory`, restoring the
    /// original cwd afterwards via a held descriptor (the original path may
    /// have been unlinked; the descriptor still restores it).
    private static func inWorkingDirectory(
        _ directory: String, _ body: () throws -> Void
    ) throws {
        workingDirectoryLock.lock()
        defer { workingDirectoryLock.unlock() }
        let previous = open(".", O_RDONLY | O_CLOEXEC)
        guard previous >= 0 else {
            throw UnixSocketError.failed(operation: "open-cwd", code: errno)
        }
        defer { close(previous) }
        guard chdir(directory) == 0 else {
            throw UnixSocketError.failed(operation: "chdir", code: errno)
        }
        defer { _ = fchdir(previous) }
        try body()
    }
}

/// Why a UNIX-socket address operation failed.
public enum UnixSocketError: Error, Equatable {
    /// A syscall failed; `code` is the raw `errno`.
    case failed(operation: String, code: Int32)
    /// Even the path's final component exceeds `sun_path` — no technique
    /// can address it.
    case pathUnrepresentable(path: String)
}
