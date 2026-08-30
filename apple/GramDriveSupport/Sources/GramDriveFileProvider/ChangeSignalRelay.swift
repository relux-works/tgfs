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
        evictingGeneratedItems: [ProviderGeneratedItemChange],
        completionHandler: @escaping @Sendable ((any Error)?) -> Void)
}

/// `NSFileProviderManager` is a stateless handle onto the system's file
/// provider daemon; its methods are documented callable from any thread.
// swift-format-ignore: AvoidRetroactiveConformances
extension NSFileProviderManager: @retroactive @unchecked Sendable {}

extension NSFileProviderManager: ProviderChangeSignaling {
    public func signalChanges(
        includeRoot: Bool,
        changedContainers: [NSFileProviderItemIdentifier],
        evictingGeneratedItems: [ProviderGeneratedItemChange],
        completionHandler: @escaping @Sendable ((any Error)?) -> Void
    ) {
        ProviderChangeDispatcher(
            materializedEnumerator: enumeratorForMaterializedItems(),
            evict: { identifier, completion in
                self.evictItem(identifier: identifier, completionHandler: completion)
            },
            signal: { identifier, completion in
                self.signalEnumerator(for: identifier, completionHandler: completion)
            }
        ).dispatch(
            includeRoot: includeRoot,
            changedContainers: changedContainers,
            evictingGeneratedItems: evictingGeneratedItems,
            completionHandler: completionHandler)
    }
}

/// Orders generated-materialization eviction before the matching journal pull.
/// The live manager supplies the two File Provider calls above; tests inject
/// synchronous operations and exercise the same production sequencing.
final class ProviderChangeDispatcher: @unchecked Sendable {
    typealias Completion = @Sendable ((any Error)?) -> Void
    typealias Operation =
        @Sendable (
            NSFileProviderItemIdentifier, @escaping Completion
        ) -> Void
    typealias ScheduleEvictionTurn =
        @Sendable (@escaping @Sendable () -> Void) -> Void

    /// Keep each File Provider daemon turn below the monthly Markdown/NDJSON
    /// pair width. A retained-profile bootstrap can contain thousands of
    /// generated candidates; recursively issuing every `evictItem` callback
    /// in one lane prevents the daemon from scheduling unrelated
    /// `fetchContents` requests even though the agent answers those requests
    /// promptly.
    static let maxEvictionsPerTurn = 2
    private static let evictionTurnDelay: DispatchTimeInterval = .milliseconds(10)
    private static let evictionQueue = DispatchQueue(
        label: "com.reluxworks.gramdrive.fileprovider.generated-eviction",
        qos: .utility)
    static let defaultEvictionTurnScheduler: ScheduleEvictionTurn = { continuation in
        evictionQueue.asyncAfter(
            deadline: .now() + evictionTurnDelay,
            execute: continuation)
    }

    private let evict: Operation
    private let signal: Operation
    private let materializedEnumerator: any NSFileProviderEnumerator
    private let scheduleSelectionTimeout: MaterializedGeneratedItemSelector.ScheduleTimeout
    private let scheduleEvictionTurn: ScheduleEvictionTurn

    init(
        materializedEnumerator: any NSFileProviderEnumerator,
        scheduleSelectionTimeout: @escaping MaterializedGeneratedItemSelector.ScheduleTimeout =
            MaterializedGeneratedItemSelector.defaultTimeout,
        scheduleEvictionTurn: @escaping ScheduleEvictionTurn =
            ProviderChangeDispatcher.defaultEvictionTurnScheduler,
        evict: @escaping Operation,
        signal: @escaping Operation
    ) {
        self.materializedEnumerator = materializedEnumerator
        self.scheduleSelectionTimeout = scheduleSelectionTimeout
        self.scheduleEvictionTurn = scheduleEvictionTurn
        self.evict = evict
        self.signal = signal
    }

    func dispatch(
        includeRoot: Bool,
        changedContainers: [NSFileProviderItemIdentifier],
        evictingGeneratedItems: [ProviderGeneratedItemChange],
        completionHandler: @escaping Completion
    ) {
        var identifiers: [NSFileProviderItemIdentifier] = [.workingSet]
        if includeRoot {
            identifiers.append(.rootContainer)
        }
        identifiers.append(contentsOf: changedContainers)
        var seen: Set<String> = []
        identifiers = identifiers.filter { seen.insert($0.rawValue).inserted }
        let signalIdentifiers = identifiers

        var seenGeneratedItems: Set<String> = []
        let generatedItems = evictingGeneratedItems.filter {
            seenGeneratedItems.insert($0.item.rawValue).inserted
        }
        let result = SignalResult()
        guard !generatedItems.isEmpty else {
            dispatchSignals(
                signalIdentifiers,
                result: result,
                completionHandler: completionHandler)
            return
        }
        MaterializedGeneratedItemSelector(
            enumerator: materializedEnumerator,
            scheduleTimeout: scheduleSelectionTimeout
        ).select(from: generatedItems) { selection in
            let selected: [NSFileProviderItemIdentifier]
            switch selection {
            case .success(let identifiers):
                selected = identifiers
            case .failure(let error):
                // A failed/partial materialized-set read is not an empty set.
                // Preserve the error so the relay does not advance its
                // journal checkpoint, but still publish the change signals;
                // File Provider can then pull the new content versions while
                // a later doorbell retries the exact eviction selection.
                result.record(error)
                selected = []
            }
            self.dispatchEvictions(
                selected,
                signalIdentifiers: signalIdentifiers,
                result: result,
                completionHandler: completionHandler)
        }
    }

    private func dispatchEvictions(
        _ generatedItems: [NSFileProviderItemIdentifier],
        signalIdentifiers: [NSFileProviderItemIdentifier],
        result: SignalResult,
        completionHandler: @escaping Completion
    ) {
        dispatchSequentially(
            generatedItems,
            at: 0,
            remainingInTurn: Self.maxEvictionsPerTurn,
            operation: evict,
            result: result
        ) { _ in
            self.dispatchSignals(
                signalIdentifiers,
                result: result,
                completionHandler: completionHandler)
        }
    }

    private func dispatchSignals(
        _ identifiers: [NSFileProviderItemIdentifier],
        result: SignalResult,
        completionHandler: @escaping Completion
    ) {
        dispatchSequentially(
            identifiers,
            at: 0,
            remainingInTurn: nil,
            operation: signal,
            result: result,
            completionHandler: completionHandler)
    }

    /// Generated documents are reproducible cache-only views. Evict their
    /// previous File Provider materialization before advertising a new
    /// content version so a Finder open cannot reuse bytes from the prior
    /// render generation while the change enumeration catches up.
    private func dispatchSequentially(
        _ identifiers: [NSFileProviderItemIdentifier],
        at index: Int,
        remainingInTurn: Int?,
        operation: @escaping Operation,
        result: SignalResult,
        completionHandler: @escaping Completion
    ) {
        guard index < identifiers.count else {
            completionHandler(result.error)
            return
        }
        if remainingInTurn == 0 {
            scheduleEvictionTurn { [self] in
                dispatchSequentially(
                    identifiers,
                    at: index,
                    remainingInTurn: Self.maxEvictionsPerTurn,
                    operation: operation,
                    result: result,
                    completionHandler: completionHandler)
            }
            return
        }
        operation(identifiers[index]) { error in
            result.record(error)
            self.dispatchSequentially(
                identifiers,
                at: index + 1,
                remainingInTurn: remainingInTurn.map { $0 - 1 },
                operation: operation,
                result: result,
                completionHandler: completionHandler)
        }
    }
}

/// Resolves generated candidates whose parent containers File Provider has
/// materialized. Apple's materialized-items enumerator reports materialized
/// *containers*, not materialized files. A generated child beneath one of
/// those containers may therefore have stale Finder bytes and must be evicted
/// before its new content version is published through change enumeration.
///
/// The item-change journal is intentionally coalesced across the database
/// lifetime, so replaying it after process start may contain thousands of live
/// generated documents. Intersecting their parents with the system-owned
/// materialized-container set keeps eviction scoped to Finder-visible
/// subtrees without inventing a file-level materialization API the platform
/// does not provide.
final class MaterializedGeneratedItemSelector: @unchecked Sendable {
    typealias Completion =
        @Sendable (
            Result<[NSFileProviderItemIdentifier], any Error>
        ) -> Void
    typealias CancelTimeout = @Sendable () -> Void
    typealias ScheduleTimeout =
        @Sendable (
            @escaping @Sendable () -> Void
        ) -> CancelTimeout

    static let selectionTimeout: TimeInterval = 8
    private static let timeoutQueue = DispatchQueue(
        label: "com.reluxworks.gramdrive.fileprovider.materialized-selection-watchdog",
        qos: .userInitiated,
        attributes: .concurrent)

    static let defaultTimeout: ScheduleTimeout = { timeout in
        let work = TimeoutWorkItem(timeout)
        timeoutQueue.asyncAfter(
            deadline: .now() + selectionTimeout,
            execute: work.item)
        return { work.cancel() }
    }

    private let enumerator: any NSFileProviderEnumerator
    private let scheduleTimeout: ScheduleTimeout

    init(
        enumerator: any NSFileProviderEnumerator,
        scheduleTimeout: @escaping ScheduleTimeout = defaultTimeout
    ) {
        self.enumerator = enumerator
        self.scheduleTimeout = scheduleTimeout
    }

    func select(
        from candidates: [ProviderGeneratedItemChange],
        completion: @escaping Completion
    ) {
        let request = MaterializedSelectionRequest(
            enumerator: enumerator,
            candidates: candidates,
            completion: completion)
        request.start(scheduleTimeout: scheduleTimeout)
    }
}

private final class TimeoutWorkItem: @unchecked Sendable {
    private let lock = NSLock()
    private var cancelled = false
    private let operation: @Sendable () -> Void
    lazy var item = DispatchWorkItem { [weak self] in
        self?.run()
    }

    init(_ operation: @escaping @Sendable () -> Void) {
        self.operation = operation
    }

    func cancel() {
        lock.lock()
        cancelled = true
        lock.unlock()
        item.cancel()
    }

    private func run() {
        lock.lock()
        let shouldRun = !cancelled
        lock.unlock()
        if shouldRun {
            operation()
        }
    }
}

private final class MaterializedSelectionRequest: NSObject,
    NSFileProviderEnumerationObserver, @unchecked Sendable
{
    private let lock = NSLock()
    private let enumerator: any NSFileProviderEnumerator
    private let candidates: [ProviderGeneratedItemChange]
    private let candidatesByParent: [String: [NSFileProviderItemIdentifier]]
    private var selectedItemValues: Set<String> = []
    private var completion: MaterializedGeneratedItemSelector.Completion?
    private var cancelTimeout: MaterializedGeneratedItemSelector.CancelTimeout?

    var suggestedPageSize: Int { 256 }

    init(
        enumerator: any NSFileProviderEnumerator,
        candidates: [ProviderGeneratedItemChange],
        completion: @escaping MaterializedGeneratedItemSelector.Completion
    ) {
        self.enumerator = enumerator
        self.candidates = candidates
        self.candidatesByParent = Dictionary(grouping: candidates, by: { $0.parent.rawValue })
            .mapValues { $0.map(\.item) }
        self.completion = completion
    }

    func start(
        scheduleTimeout: MaterializedGeneratedItemSelector.ScheduleTimeout
    ) {
        let cancel = scheduleTimeout { [self] in
            timedOut()
        }
        lock.lock()
        guard completion != nil else {
            lock.unlock()
            cancel()
            return
        }
        cancelTimeout = cancel
        lock.unlock()
        enumerator.enumerateItems(
            for: self,
            startingAt: NSFileProviderPage(Data()))
    }

    func didEnumerate(_ updatedItems: [any NSFileProviderItem]) {
        lock.lock()
        guard completion != nil else {
            lock.unlock()
            return
        }
        for container in updatedItems {
            for item in candidatesByParent[container.itemIdentifier.rawValue] ?? [] {
                selectedItemValues.insert(item.rawValue)
            }
        }
        lock.unlock()
    }

    func finishEnumerating(upTo nextPage: NSFileProviderPage?) {
        if let nextPage {
            enumerator.enumerateItems(for: self, startingAt: nextPage)
            return
        }
        lock.lock()
        let selected = selectedItemValues
        lock.unlock()
        resolve(.success(candidates.map(\.item).filter { selected.contains($0.rawValue) }))
    }

    func finishEnumeratingWithError(_ error: any Error) {
        resolve(.failure(error))
    }

    private func timedOut() {
        guard let (completion, cancelTimeout) = takeCompletion() else { return }
        enumerator.invalidate()
        cancelTimeout?()
        completion(.failure(NSFileProviderError(.cannotSynchronize)))
    }

    private func resolve(
        _ result: Result<[NSFileProviderItemIdentifier], any Error>
    ) {
        guard let (completion, cancelTimeout) = takeCompletion() else { return }
        cancelTimeout?()
        completion(result)
    }

    private func takeCompletion() -> (
        MaterializedGeneratedItemSelector.Completion,
        MaterializedGeneratedItemSelector.CancelTimeout?
    )? {
        lock.lock()
        guard let completion else {
            lock.unlock()
            return nil
        }
        self.completion = nil
        let cancelTimeout = self.cancelTimeout
        self.cancelTimeout = nil
        lock.unlock()
        return (completion, cancelTimeout)
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
public struct ProviderGeneratedItemChange: Sendable, Equatable {
    public let item: NSFileProviderItemIdentifier
    public let parent: NSFileProviderItemIdentifier

    public init(
        item: NSFileProviderItemIdentifier,
        parent: NSFileProviderItemIdentifier
    ) {
        self.item = item
        self.parent = parent
    }
}

public struct ProviderContainerChanges: Sendable {
    public let journal: ChangeJournalState
    public let containers: [NSFileProviderItemIdentifier]
    public let generatedItems: [ProviderGeneratedItemChange]

    public init(
        journal: ChangeJournalState,
        containers: [NSFileProviderItemIdentifier],
        generatedItems: [ProviderGeneratedItemChange] = []
    ) {
        self.journal = journal
        self.containers = containers
        self.generatedItems = generatedItems
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
        var generatedItems: [String: ProviderGeneratedItemChange] = [:]
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
                if change.metadata.kind == .generatedDoc,
                    change.metadata.deletedAtMs == nil
                {
                    let item = ItemIdentifierMapping.providerIdentifier(
                        forCoreItemId: change.metadata.id,
                        accountRootId: account.rootItemId)
                    let parent = ItemIdentifierMapping.parentIdentifier(
                        forParentCoreItemId: change.metadata.parent,
                        accountRootId: account.rootItemId)
                    generatedItems[item.rawValue] = ProviderGeneratedItemChange(
                        item: item, parent: parent)
                }
            }
            guard let last = page.last else { break }
            sequence = last.sequence
            if page.count < Int(pageSize) || sequence >= current.latestSequence {
                break
            }
        }
        // An upgraded installed database can contain generated documents
        // whose current rows predate the item-change journal. Snapshot this
        // narrow class exactly once, before the first provider publication;
        // all later checks remain journal-delta-only.
        if prior == nil {
            for metadata in try store.liveGeneratedItems(accountId: account.accountId) {
                guard let parentID = metadata.parent else { continue }
                let item = ItemIdentifierMapping.providerIdentifier(
                    forCoreItemId: metadata.id,
                    accountRootId: account.rootItemId)
                let parent = ItemIdentifierMapping.parentIdentifier(
                    forParentCoreItemId: parentID,
                    accountRootId: account.rootItemId)
                generatedItems[item.rawValue] = ProviderGeneratedItemChange(
                    item: item, parent: parent)
            }
        }
        let position = ChangeJournalState(
            instanceId: current.instanceId,
            latestSequence: max(current.latestSequence, sequence))
        return ProviderContainerChanges(
            journal: position,
            containers: identifiers.sorted().map(NSFileProviderItemIdentifier.init(rawValue:)),
            generatedItems: generatedItems.values.sorted {
                $0.item.rawValue < $1.item.rawValue
            })
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
    private var dispatchInFlight = false
    private var pendingCheck = false
    private var pendingReplacement = false
    private let probe: @Sendable () throws -> Int64
    private let containerProbe: ContainerProbe
    private let signaling: any ProviderChangeSignaling

    private enum DispatchRequest {
        case check
        case replacement
    }

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
        enqueue(.check)
    }

    private func enqueue(_ request: DispatchRequest) {
        lock.lock()
        guard !dispatchInFlight else {
            switch request {
            case .check:
                pendingCheck = true
            case .replacement:
                pendingReplacement = true
            }
            lock.unlock()
            return
        }
        dispatchInFlight = true
        lock.unlock()
        perform(request)
    }

    private func perform(_ request: DispatchRequest) {
        switch request {
        case .check:
            performCheck()
        case .replacement:
            performReplacement()
        }
    }

    private func performCheck() {
        guard let version = try? probe() else {
            finishDispatch()
            return
        }
        lock.lock()
        let priorVersion = lastVersion
        let initial = priorVersion == nil
        let moved = priorVersion != version
        let priorJournal = lastJournal
        lock.unlock()
        guard moved else {
            finishDispatch()
            return
        }
        guard let containerChanges = try? containerProbe(priorJournal) else {
            finishDispatch()
            return
        }
        signaling.signalChanges(
            includeRoot: initial,
            changedContainers: containerChanges.containers,
            evictingGeneratedItems: containerChanges.generatedItems
        ) { [weak self] error in
            guard let self else { return }
            if error == nil {
                self.lock.lock()
                self.lastVersion = version
                self.lastJournal = containerChanges.journal
                self.lock.unlock()
            }
            self.finishDispatch()
        }
    }

    /// Reasserts the working set and root after the agent was replaced. This
    /// is intentionally not conditional on a new state version: Finder may
    /// have held an enumerator across the short agent gap, and the matching
    /// replacement's ready hierarchy is the event that makes retry safe.
    public func signalEnumeratorsAfterAgentReplacement() {
        enqueue(.replacement)
    }

    private func performReplacement() {
        lock.lock()
        let priorJournal = lastJournal
        lock.unlock()
        guard let containerChanges = try? containerProbe(priorJournal) else {
            finishDispatch()
            return
        }
        signaling.signalChanges(
            includeRoot: true,
            changedContainers: containerChanges.containers,
            evictingGeneratedItems: containerChanges.generatedItems
        ) { [weak self] error in
            guard let self else { return }
            if error == nil {
                self.lock.lock()
                self.lastJournal = containerChanges.journal
                self.lock.unlock()
            }
            self.finishDispatch()
        }
    }

    private func finishDispatch() {
        let next: DispatchRequest?
        lock.lock()
        if pendingReplacement {
            pendingReplacement = false
            next = .replacement
        } else if pendingCheck {
            pendingCheck = false
            next = .check
        } else {
            dispatchInFlight = false
            next = nil
        }
        lock.unlock()
        if let next {
            perform(next)
        }
    }

    deinit {
        observation?.cancel()
    }
}
