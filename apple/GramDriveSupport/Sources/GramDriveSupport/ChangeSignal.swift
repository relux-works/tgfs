import Foundation
import notify

/// The cross-process change doorbell: a payload-free Darwin notification
/// the writing process posts after committing to shared state, so reading
/// processes re-check instead of polling hot.
///
/// A doorbell is advisory, never authoritative. Darwin notifications carry
/// no payload, coalesce under load, and vanish when the observer is not
/// running — so observers treat a ring (or a periodic tick, or a wake) as
/// *check now*: compare `SharedStateStore.dataVersion()` against the last
/// value seen and re-read only on change. The durable truth is always the
/// database, per the multi-process design (no shared-memory assumptions).
///
/// The name carries the App Group prefix, which is what sandboxed
/// processes are permitted to post and observe. Signaling *Finder* about
/// provider changes is a different channel (`NSFileProviderManager
/// .signalEnumerator`, bridged from this doorbell by the File Provider
/// layer's `ChangeSignalRelay`); this doorbell coordinates GramDrive's
/// own processes.
public enum ChangeSignal {
    /// The Darwin notification name, derived from the App Group identifier
    /// (DEC-019 namespace rule).
    public static let name = AppGroup.identifier + ".state-changed"

    /// Rings the doorbell. Writers post after commit — never before, so a
    /// woken reader always finds the commit it was woken for.
    public static func post() {
        post(name: name)
    }

    /// Observes the doorbell, delivering `handler` on `queue` for every
    /// ring (possibly coalesced) until the returned observation is
    /// cancelled or deallocated.
    public static func observe(
        on queue: DispatchQueue = DispatchQueue(
            label: "com.reluxworks.gramdrive.change-signal"
        ),
        _ handler: @escaping @Sendable () -> Void
    ) throws -> ChangeSignalObservation {
        try observe(name: name, on: queue, handler)
    }

    // Internal name-scoped seam: Darwin notification names are one global
    // namespace per host, so tests observing the product name would hear
    // each other. Same mechanism, isolated names.

    static func post(name: String) {
        notify_post(name)
    }

    static func observe(
        name: String,
        on queue: DispatchQueue = DispatchQueue(
            label: "com.reluxworks.gramdrive.change-signal"
        ),
        _ handler: @escaping @Sendable () -> Void
    ) throws -> ChangeSignalObservation {
        var token: Int32 = 0
        let status = notify_register_dispatch(name, &token, queue) { _ in
            handler()
        }
        guard status == NOTIFY_STATUS_OK else {
            throw ChangeSignalError.registrationFailed(status: status)
        }
        return ChangeSignalObservation(token: token)
    }
}

/// A live doorbell observation. Cancel explicitly or let it deallocate;
/// both stop delivery.
public final class ChangeSignalObservation: @unchecked Sendable {
    private let lock = NSLock()
    private var token: Int32?

    fileprivate init(token: Int32) {
        self.token = token
    }

    /// Stops delivery. Idempotent.
    public func cancel() {
        lock.lock()
        defer { lock.unlock() }
        if let token {
            notify_cancel(token)
            self.token = nil
        }
    }

    deinit {
        cancel()
    }
}

/// Why a doorbell observation could not be registered.
public enum ChangeSignalError: Error, Equatable {
    /// `notify_register_dispatch` refused; the status is the raw
    /// `NOTIFY_STATUS_*` value.
    case registrationFailed(status: UInt32)
}
