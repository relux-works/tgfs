import Foundation
@testable import GramDriveAgentCore
import Testing

private final class ArmProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var armed = false

    func recordArm() {
        lock.lock()
        armed = true
        lock.unlock()
    }

    var wasArmed: Bool {
        lock.lock()
        defer { lock.unlock() }
        return armed
    }
}

struct CommitExitWatchdogTests {
    @Test func productionCommittedExitDeadlineIsTwoSeconds() {
        #expect(CommitExitWatchdog.committedExitDeadline == .seconds(2))
    }

    @Test func installFailurePreventsEveryLaterArm() {
        let watchdog = CommitExitWatchdog(
            system: .init(
                installExitHandler: { false },
                unblockAlarm: { Issue.record("unblock must not run after signal failure"); return false },
                armTimer: { _ in Issue.record("timer must not arm without installation"); return false }
            )
        )

        #expect(!watchdog.install())
        #expect(!watchdog.install())
        #expect(!watchdog.arm(after: .milliseconds(1)))
    }

    @Test func unblockFailurePreventsEveryLaterArm() {
        let watchdog = CommitExitWatchdog(
            system: .init(
                installExitHandler: { true },
                unblockAlarm: { false },
                armTimer: { _ in Issue.record("timer must not arm without an unblocked handler"); return false }
            )
        )

        #expect(!watchdog.install())
        #expect(!watchdog.arm(after: .milliseconds(1)))
    }

    @Test func armUsesTheTimerOnlyAfterSuccessfulInstallation() {
        let probe = ArmProbe()
        let watchdog = CommitExitWatchdog(
            system: .init(
                installExitHandler: { true },
                unblockAlarm: { true },
                armTimer: { _ in probe.recordArm(); return true }
            )
        )

        #expect(watchdog.install())
        #expect(watchdog.arm(after: .milliseconds(1)))
        #expect(probe.wasArmed)
    }
}
