import AppKit
import Foundation

/// System power transitions the agent reacts to.
public enum PowerEvent: Equatable, Sendable {
    /// The machine is about to sleep.
    case willSleep
    /// The machine woke from sleep.
    case didWake
}

/// A live power-event subscription. Cancel explicitly or let it
/// deallocate; both stop delivery.
public final class PowerEventObservation: @unchecked Sendable {
    private let lock = NSLock()
    private var cancelHandler: (() -> Void)?

    public init(onCancel: @escaping () -> Void) {
        self.cancelHandler = onCancel
    }

    /// Stops delivery. Idempotent.
    public func cancel() {
        lock.lock()
        defer { lock.unlock() }
        cancelHandler?()
        cancelHandler = nil
    }

    deinit {
        cancel()
    }
}

/// Source of ``PowerEvent``s. The product implementation is
/// ``WorkspacePowerEventSource``; tests substitute a hand-driven fake.
public protocol PowerEventSource {
    /// Starts delivering events to `handler` until the observation is
    /// cancelled.
    func observe(_ handler: @escaping @Sendable (PowerEvent) -> Void) -> PowerEventObservation
}

/// The product source: `NSWorkspace` sleep/wake notifications. Requires a
/// running main run loop, which the agent executable provides.
///
/// Sleep needs no special handling from the agent — TDLib and the engine
/// treat a network drop as a network drop — but *wake* is a moment the
/// doorbell may have been missed (Darwin notifications vanish for sleeping
/// observers), so the lifecycle re-probes shared state on it.
public struct WorkspacePowerEventSource: PowerEventSource {
    public init() {}

    public func observe(
        _ handler: @escaping @Sendable (PowerEvent) -> Void
    ) -> PowerEventObservation {
        let center = NSWorkspace.shared.notificationCenter
        let sleepToken = center.addObserver(
            forName: NSWorkspace.willSleepNotification, object: nil, queue: nil
        ) { _ in handler(.willSleep) }
        let wakeToken = center.addObserver(
            forName: NSWorkspace.didWakeNotification, object: nil, queue: nil
        ) { _ in handler(.didWake) }
        return PowerEventObservation {
            center.removeObserver(sleepToken)
            center.removeObserver(wakeToken)
        }
    }
}
