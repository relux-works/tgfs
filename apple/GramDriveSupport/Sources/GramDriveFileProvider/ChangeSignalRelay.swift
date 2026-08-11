import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport

/// The Finder-facing signal a host process sends after durable namespace
/// changes: always pull the domain-wide working set, and optionally refresh
/// the root container during initial reconciliation. Journaled parent
/// containers are signaled too, including the initial backlog that may have
/// committed before this process began observing.
///
/// Seam over `NSFileProviderManager.signalEnumerator` for the working set and
/// root container, so the relay is exercisable without a registered domain
/// (the manager requires one; tests record the calls instead).
public protocol ProviderChangeSignaling: Sendable {
    /// Asks the system to pull working-set changes. `includeRoot` is reserved
    /// for relay startup, when authorization may have installed the fixed root
    /// children while no observer was alive. Ordinary history/render commits
    /// keep it false and enumerate only their journaled affected items.
    /// Best-effort by
    /// platform design: the completion error means the domain is not
    /// currently reachable (disconnected, being removed) and the system
    /// will enumerate on reconnect anyway.
    func signalChanges(
        includeRoot: Bool,
        changedContainers: [NSFileProviderItemIdentifier],
        completionHandler: @escaping @Sendable ((any Error)?) -> Void)
}

/// `NSFileProviderManager` is a stateless handle onto the system's file
/// provider daemon; its methods are documented callable from any thread.
extension NSFileProviderManager: @retroactive @unchecked Sendable {}

extension NSFileProviderManager: ProviderChangeSignaling {
    public func signalChanges(
        includeRoot: Bool,
        changedContainers: [NSFileProviderItemIdentifier],
        completionHandler: @escaping @Sendable ((any Error)?) -> Void
    ) {
        var identifiers: [NSFileProviderItemIdentifier] = [.workingSet]
        if includeRoot {
            identifiers.append(.rootContainer)
        }
        identifiers.append(contentsOf: changedContainers)
        var seen: Set<String> = []
        identifiers = identifiers.filter { seen.insert($0.rawValue).inserted }

        let result = SignalResult()
        signalSequentially(
            identifiers,
            at: 0,
            result: result,
            completionHandler: completionHandler)
    }

    private func signalSequentially(
        _ identifiers: [NSFileProviderItemIdentifier],
        at index: Int,
        result: SignalResult,
        completionHandler: @escaping @Sendable ((any Error)?) -> Void
    ) {
        guard index < identifiers.count else {
            completionHandler(result.error)
            return
        }
        signalEnumerator(for: identifiers[index]) { error in
            result.record(error)
            self.signalSequentially(
                identifiers,
                at: index + 1,
                result: result,
                completionHandler: completionHandler)
        }
    }
}

private final class SignalResult: @unchecked Sendable {
    private let lock = NSLock()
    private var firstError: (any Error)?

    var error: (any Error)? {
        lock.lock()
        defer { lock.unlock() }
        return firstError
    }

    func record(_ error: (any Error)?) {
        guard let error else { return }
        lock.lock()
        if firstError == nil {
            firstError = error
        }
        lock.unlock()
    }
}

/// Journal position plus the parent containers whose child metadata moved.
public struct ProviderContainerChanges: Sendable {
    public let journal: ChangeJournalState
    public let containers: [NSFileProviderItemIdentifier]

    public init(
        journal: ChangeJournalState,
        containers: [NSFileProviderItemIdentifier]
    ) {
        self.journal = journal
        self.containers = containers
    }
}

/// Resolves the journal delta into the containers File Provider must pull.
public enum ProviderContainerChangeResolver {
    private static let pageSize: UInt32 = 256

    public static func changes(
        store: any SharedStateStoreProtocol,
        account: AccountInfo,
        after prior: ChangeJournalState?
    ) throws -> ProviderContainerChanges {
        let current = try store.changeJournalState()
        var sequence =
            prior?.instanceId == current.instanceId ? prior?.latestSequence ?? 0 : 0
        var identifiers: Set<String> = []
        while true {
            let page = try store.itemChangesSince(
                accountId: account.accountId,
                afterSequence: sequence,
                limit: pageSize)
            for change in page {
                identifiers.insert(
                    ItemIdentifierMapping.parentIdentifier(
                        forParentCoreItemId: change.metadata.parent,
                        accountRootId: account.rootItemId
                    ).rawValue)
            }
            guard let last = page.last else { break }
            sequence = last.sequence
            if page.count < Int(pageSize) || sequence >= current.latestSequence {
                break
            }
        }
        let position = ChangeJournalState(
            instanceId: current.instanceId,
            latestSequence: max(current.latestSequence, sequence))
        return ProviderContainerChanges(
            journal: position,
            containers: identifiers.sorted().map(NSFileProviderItemIdentifier.init(rawValue:)))
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
/// both enumerators only when the probe moved, keeping
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
    public typealias ContainerProbe =
        @Sendable (ChangeJournalState?) throws -> ProviderContainerChanges

    private let lock = NSLock()
    private var observation: (any ChangeObservationToken)?
    private var lastVersion: Int64?
    private var lastJournal: ChangeJournalState?
    private let probe: @Sendable () throws -> Int64
    private let containerProbe: ContainerProbe
    private let signaling: any ProviderChangeSignaling

    /// A relay from `probe` (the change stamp of the shared state the
    /// domain serves) to `signaling` (the domain's working-set/root doorbell
    /// to the system).
    public init(
        probe: @escaping @Sendable () throws -> Int64,
        containerProbe: @escaping ContainerProbe = { prior in
            ProviderContainerChanges(
                journal: prior ?? ChangeJournalState(instanceId: "", latestSequence: 0),
                containers: [])
        },
        signaling: any ProviderChangeSignaling
    ) {
        self.probe = probe
        self.containerProbe = containerProbe
        self.signaling = signaling
    }

    /// Starts observing the doorbell and immediately checks once, which
    /// both primes the probe baseline and pulls every journaled parent whose
    /// ring may have been missed while the relay was not running.
    /// Idempotent-ish by construction: a second `start` replaces the previous
    /// observation.
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
        let initial = lastVersion == nil
        let moved = lastVersion != version
        let priorJournal = lastJournal
        lock.unlock()
        guard moved else { return }
        guard let containerChanges = try? containerProbe(priorJournal) else {
            return
        }
        lock.lock()
        lastVersion = version
        lastJournal = containerChanges.journal
        lock.unlock()
        signaling.signalChanges(
            includeRoot: initial,
            changedContainers: containerChanges.containers
        ) { _ in
            // Best-effort by design (protocol docs): an unreachable domain
            // will be enumerated when it reconnects.
        }
    }

    /// Reasserts the working set and root after the agent was replaced. This
    /// is intentionally not conditional on a new state version: Finder may
    /// have held an enumerator across the short agent gap, and the matching
    /// replacement's ready hierarchy is the event that makes retry safe.
    public func signalEnumeratorsAfterAgentReplacement() {
        lock.lock()
        let priorJournal = lastJournal
        lock.unlock()
        guard let containerChanges = try? containerProbe(priorJournal) else {
            return
        }
        lock.lock()
        lastJournal = containerChanges.journal
        lock.unlock()
        signaling.signalChanges(
            includeRoot: true,
            changedContainers: containerChanges.containers
        ) { _ in
            // Best-effort: the system re-enumerates an unavailable domain on
            // reconnect, so a transient File Provider daemon error is safe.
        }
    }

    deinit {
        observation?.cancel()
    }
}
