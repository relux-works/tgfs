import Darwin
import Foundation

/// The one-coordinator-per-container guard (shared-state module § Roles:
/// one engine-hosting process per shared container, by product shape).
///
/// Backed by `flock(LOCK_EX | LOCK_NB)` on a file in the agent's runtime
/// directory. `flock` is the right primitive here because the kernel drops
/// the lock with the holder's last descriptor: a crashed or SIGKILLed agent
/// releases it instantly, so restart-after-crash needs no stale-lock
/// heuristics and a healthy agent can never be locked out by a dead one.
/// The lock's *content* (pid, start time) is diagnostics only — never
/// authoritative, never parsed for liveness.
public final class SingleInstanceLock: @unchecked Sendable {
    /// The locked file.
    public let url: URL

    private let lock = NSLock()
    private var descriptor: Int32?

    private init(url: URL, descriptor: Int32) {
        self.url = url
        self.descriptor = descriptor
    }

    /// Acquires the lock, failing immediately when another live process
    /// holds it.
    ///
    /// On success the file content is rewritten with `pid=` and
    /// `acquired_at_ms=` diagnostic lines.
    public static func acquire(
        at url: URL,
        now: Date = Date()
    ) throws -> SingleInstanceLock {
        let fd = open(url.path, O_WRONLY | O_CREAT | O_CLOEXEC, 0o644)
        guard fd >= 0 else {
            throw SingleInstanceLockError.io(
                operation: "open", code: errno)
        }
        guard flock(fd, LOCK_EX | LOCK_NB) == 0 else {
            let code = errno
            close(fd)
            if code == EWOULDBLOCK {
                throw SingleInstanceLockError.alreadyHeld(path: url.path)
            }
            throw SingleInstanceLockError.io(operation: "flock", code: code)
        }
        // Diagnostics for a human inspecting the container; failures here
        // must not forfeit an already-held lock.
        let acquiredAtMs = Int64((now.timeIntervalSince1970 * 1000).rounded())
        let note = "pid=\(ProcessInfo.processInfo.processIdentifier)\nacquired_at_ms=\(acquiredAtMs)\n"
        _ = ftruncate(fd, 0)
        note.utf8CString.withUnsafeBufferPointer { buffer in
            // Drop the trailing NUL; write is best-effort.
            _ = write(fd, buffer.baseAddress, buffer.count - 1)
        }
        return SingleInstanceLock(url: url, descriptor: fd)
    }

    /// Releases the lock. Idempotent; also runs on deallocation.
    public func release() {
        lock.lock()
        defer { lock.unlock() }
        if let descriptor {
            // Closing the descriptor releases the flock.
            close(descriptor)
            self.descriptor = nil
        }
    }

    deinit {
        release()
    }
}

/// Why the single-instance lock could not be acquired.
public enum SingleInstanceLockError: Error, Equatable {
    /// Another live process holds the lock — a second agent over the same
    /// container must exit instead of racing the first.
    case alreadyHeld(path: String)
    /// A filesystem-level failure; `code` is the raw `errno`.
    case io(operation: String, code: Int32)
}
