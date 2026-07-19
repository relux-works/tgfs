import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport

/// The one Finder-facing signal a host process sends: "the working set has
/// changes — come enumerate them."
///
/// Seam over `NSFileProviderManager.signalEnumerator(for: .workingSet)`, so
/// the relay is exercisable without a registered domain (the manager
/// requires one; tests record the calls instead).
public protocol WorkingSetSignaling: Sendable {
    /// Asks the system to pull working-set changes. Best-effort by
    /// platform design: the completion error means the domain is not
    /// currently reachable (disconnected, being removed) and the system
    /// will enumerate on reconnect anyway.
    func signalWorkingSet(completionHandler: @escaping @Sendable ((any Error)?) -> Void)
}

/// `NSFileProviderManager` is a stateless handle onto the system's file
/// provider daemon; its methods are documented callable from any thread.
extension NSFileProviderManager: @retroactive @unchecked Sendable {}

extension NSFileProviderManager: WorkingSetSignaling {
    public func signalWorkingSet(
        completionHandler: @escaping @Sendable ((any Error)?) -> Void
    ) {
        signalEnumerator(for: .workingSet, completionHandler: completionHandler)
    }
}

/// A live doorbell subscription the relay can cancel — the shape of
/// `ChangeSignalObservation`, stated as a protocol so tests can subscribe
/// the relay to a scripted trigger instead of the host-global Darwin
/// notification namespace.
public protocol ChangeObservationToken: AnyObject, Sendable {
    /// Stops delivery. Idempotent.
    func cancel()
}

extension ChangeSignalObservation: ChangeObservationToken {}

/// Bridges GramDrive's cross-process change doorbell to Finder's change
/// pull (TASK-260715-rhcnhc; PLAT-MAC-004 change signaling).
///
/// The engine's host process rings the Darwin doorbell after committing to
/// shared state (`ChangeSignal`, GramDriveSupport). The doorbell is
/// advisory — coalescing, payload-free, lost while nobody listens — so the
/// relay treats a ring exactly as the shared-state design prescribes:
/// *check now*. It probes `SharedStateStore.dataVersion()` and calls
/// `signalEnumerator(for: .workingSet)` only when the probe moved, keeping
/// quiet doorbell storms away from the system. The signal itself is also
/// advisory: over-signaling costs one empty change enumeration, and rings
/// missed while the relay was not running are covered by the probe-on-start
/// (the first probe always differs from "never probed").
///
/// Hosted by the process that registers domains (the containing app /
/// companion, alongside `DomainReconciler`) — the extension process only
/// runs while the system has requests in flight, so it cannot watch a
/// doorbell for the system.
public final class ChangeSignalRelay: @unchecked Sendable {
    /// How the relay subscribes to the doorbell; the default is the product
    /// doorbell, tests inject their own trigger.
    public typealias Observe = (@escaping @Sendable () -> Void) throws -> any ChangeObservationToken

    private let lock = NSLock()
    private var observation: (any ChangeObservationToken)?
    private var lastVersion: Int64?
    private let probe: @Sendable () throws -> Int64
    private let signaling: any WorkingSetSignaling

    /// A relay from `probe` (the change stamp of the shared state the
    /// domain serves) to `signaling` (the domain's working-set doorbell to
    /// the system).
    public init(
        probe: @escaping @Sendable () throws -> Int64,
        signaling: any WorkingSetSignaling
    ) {
        self.probe = probe
        self.signaling = signaling
    }

    /// Starts observing the doorbell and immediately checks once, which
    /// both primes the probe baseline and covers every ring missed while
    /// the relay was not running. Idempotent-ish by construction: a second
    /// `start` replaces the previous observation.
    public func start(observe: Observe = { try ChangeSignal.observe($0) }) throws {
        let observation = try observe { [weak self] in self?.check() }
        lock.lock()
        self.observation = observation
        lock.unlock()
        check()
    }

    /// Stops observing. In-flight checks finish; no further rings arrive.
    public func stop() {
        lock.lock()
        let observation = self.observation
        self.observation = nil
        lock.unlock()
        observation?.cancel()
    }

    /// One doorbell ring or poll tick: probe, and signal only on movement.
    /// A failing probe (the store mid-recovery) signals nothing — the next
    /// ring retries, and the durable truth stays in the database.
    public func check() {
        guard let version = try? probe() else { return }
        lock.lock()
        let moved = lastVersion != version
        lastVersion = version
        lock.unlock()
        guard moved else { return }
        signaling.signalWorkingSet { _ in
            // Best-effort by design (protocol docs): an unreachable domain
            // will be enumerated when it reconnects.
        }
    }

    deinit {
        observation?.cancel()
    }
}
