import Darwin
import Foundation

/// Process-owned hard-exit watchdog for an accepted termination commit.
///
/// The handler is installed and unblocked before engine work starts. A
/// successful arm therefore means both that the process can receive
/// `SIGALRM` and that the real-time timer was configured successfully.
public final class CommitExitWatchdog: @unchecked Sendable {
    public struct SystemCalls: @unchecked Sendable {
        let installExitHandler: @Sendable () -> Bool
        let unblockAlarm: @Sendable () -> Bool
        let armTimer: @Sendable (Duration) -> Bool

        public init(
            installExitHandler: @escaping @Sendable () -> Bool,
            unblockAlarm: @escaping @Sendable () -> Bool,
            armTimer: @escaping @Sendable (Duration) -> Bool
        ) {
            self.installExitHandler = installExitHandler
            self.unblockAlarm = unblockAlarm
            self.armTimer = armTimer
        }

        public static let live = Self(
            installExitHandler: {
                let previousHandler = Darwin.signal(SIGALRM, CommitExitWatchdog.exitOnAlarm)
                let previousAddress = unsafeBitCast(previousHandler, to: UnsafeRawPointer?.self)
                let errorAddress = unsafeBitCast(SIG_ERR, to: UnsafeRawPointer?.self)
                return previousAddress != errorAddress
            },
            unblockAlarm: {
                var signals = sigset_t()
                guard sigemptyset(&signals) == 0, sigaddset(&signals, SIGALRM) == 0 else {
                    return false
                }
                return pthread_sigmask(SIG_UNBLOCK, &signals, nil) == 0
            },
            armTimer: { duration in
                let components = duration.components
                let seconds = max(0, components.seconds)
                let microseconds = max(0, Int64(components.attoseconds / 1_000_000_000_000))
                var timer = itimerval()
                timer.it_value.tv_sec = Int(seconds)
                timer.it_value.tv_usec = Int32(microseconds)
                return Darwin.setitimer(ITIMER_REAL, &timer, nil) == 0
            }
        )
    }

    private static let exitOnAlarm: @convention(c) (Int32) -> Void = { _ in
        Darwin._exit(0)
    }

    private let lock = NSLock()
    private let system: SystemCalls
    private var installationAttempted = false
    private var installed = false

    public init(system: SystemCalls = .live) {
        self.system = system
    }

    /// Installs the only permitted hard-exit handler. A failed installation is
    /// remembered so later commit attempts cannot manufacture a permit.
    @discardableResult
    public func install() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        guard !installationAttempted else { return installed }
        installationAttempted = true
        guard system.installExitHandler(), system.unblockAlarm() else { return false }
        installed = true
        return true
    }

    /// Arms the watchdog only after a truthful startup installation.
    @discardableResult
    public func arm(after duration: Duration) -> Bool {
        lock.lock()
        guard installed else {
            lock.unlock()
            return false
        }
        lock.unlock()
        return system.armTimer(duration)
    }
}
