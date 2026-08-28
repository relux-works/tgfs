import Darwin
import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport

@testable import GramDriveFileProvider

final class RecordingHistoryPrioritySignaler: HistoryPrioritySignaling, @unchecked Sendable {
    private let lock = NSLock()
    private var requests: [HistoryPriorityRequest] = []

    func signal(_ request: HistoryPriorityRequest) {
        lock.lock()
        requests.append(request)
        lock.unlock()
    }

    func snapshot() -> [HistoryPriorityRequest] {
        lock.lock()
        defer { lock.unlock() }
        return requests
    }
}

// MARK: - The scripted store

/// An in-memory `SharedStateStoreProtocol` mirroring the core semantics the
/// enumerators lean on — keyset `children` pages in stable id order, the
/// coalesced change journal with strictly increasing issuance, no-op writes
/// journal-quiet — plus scripting hooks that mutate the tree *between*
/// listing pages, which is exactly the concurrency the task's AC is about.
///
/// A fake is the sanctioned harness here, not an evasion: DEC-006 keeps
/// durable writes off the FFI, so Swift tests cannot seed a real store (the
/// cross-process real-store proof is `make smoke-shared-state`), and the
/// core-side semantics this fake restates are pinned by the Rust suites
/// (`repo_item_changes.rs`, `shared_state.rs`).
final class ScriptedStore: SharedStateStoreProtocol, @unchecked Sendable {
    private let lock = NSLock()
    private var accountsById: [Int64: AccountInfo]
    private var itemsById: [String: ItemMetadata] = [:]
    /// The coalesced journal: at most one row per item, ordered by issue.
    private var journal: [(sequence: Int64, itemId: String)] = []
    /// The high-water mark of issued sequences; never rewinds.
    private var issued: Int64 = 0
    /// The journal's database-life identity.
    private var journalInstance: String
    /// The `dataVersion` stamp the relay probes.
    var stampedDataVersion: Int64 = 1
    /// Mutations to run at the start of upcoming `children` calls, in
    /// order — one script per call, sustaining "the tree moved between
    /// pages" without threads.
    private var childrenScripts: [() -> Void] = []
    private var nextChildrenError: Error?
    private var changeScripts: [() -> Void] = []
    private var nextChangeError: Error?
    private var observedItemQos: [qos_class_t] = []

    init(account: AccountInfo, journalInstance: String = "life-1") {
        self.accountsById = [account.accountId: account]
        self.journalInstance = journalInstance
    }

    // MARK: Scripting surface (what the "engine" does)

    /// Upserts one item the way the engine would: journals a fresh sequence
    /// exactly when the row actually changed.
    func apply(_ item: ItemMetadata) {
        lock.lock()
        defer { lock.unlock() }
        if itemsById[item.id] == item {
            return
        }
        itemsById[item.id] = item
        journal.removeAll { $0.itemId == item.id }
        issued += 1
        journal.append((sequence: issued, itemId: item.id))
    }

    /// Tombstones one item (POL-3): the row stays, flagged deleted, and the
    /// transition journals once.
    func tombstone(id: String, atMs deletedAtMs: Int64) {
        lock.lock()
        let existing = itemsById[id]
        lock.unlock()
        guard var item = existing, item.deletedAtMs == nil else { return }
        item.deletedAtMs = deletedAtMs
        item.metadataVersion += "-tombstone"
        apply(item)
    }

    /// Queues a mutation to run when the *next* `children` call arrives
    /// (further calls dequeue further scripts): the concurrent-writer seam.
    func scriptBeforeNextChildrenCall(_ mutation: @escaping () -> Void) {
        lock.lock()
        childrenScripts.append(mutation)
        lock.unlock()
    }

    /// Fails exactly the next page read, then recovers for a retry.
    func failNextChildrenCall(with error: Error) {
        lock.lock()
        nextChildrenError = error
        lock.unlock()
    }

    /// Queues work at the start of the next change-journal page read.
    func scriptBeforeNextChangeCall(_ mutation: @escaping () -> Void) {
        lock.lock()
        changeScripts.append(mutation)
        lock.unlock()
    }

    /// Fails exactly the next change-journal page read.
    func failNextChangeCall(with error: Error) {
        lock.lock()
        nextChangeError = error
        lock.unlock()
    }

    /// Replaces the account row, e.g. with a bumped namespace epoch.
    func replaceAccount(_ account: AccountInfo) {
        lock.lock()
        accountsById[account.accountId] = account
        lock.unlock()
    }

    /// Removes the account row (mid-removal domain).
    func removeAccount(accountId: Int64) {
        lock.lock()
        accountsById[accountId] = nil
        lock.unlock()
    }

    /// A new database life: recovery quarantined the old file. Sequences
    /// restart; the instance names the difference.
    func restartJournalLife(instance: String) {
        lock.lock()
        journalInstance = instance
        journal = []
        issued = 0
        lock.unlock()
    }

    /// The sequence most recently issued.
    var latestSequence: Int64 {
        lock.lock()
        defer { lock.unlock() }
        return issued
    }

    // MARK: SharedStateStoreProtocol

    func account(accountId: Int64) throws -> AccountInfo? {
        lock.lock()
        defer { lock.unlock() }
        return accountsById[accountId]
    }

    func accounts() throws -> [AccountInfo] {
        lock.lock()
        defer { lock.unlock() }
        return accountsById.values.sorted { $0.accountId < $1.accountId }
    }

    func changeJournalState() throws -> ChangeJournalState {
        lock.lock()
        defer { lock.unlock() }
        return ChangeJournalState(instanceId: journalInstance, latestSequence: issued)
    }

    func childByName(parent: String, safeName: String) throws -> ItemMetadata? {
        lock.lock()
        defer { lock.unlock() }
        return itemsById.values.first {
            $0.parent == parent && $0.safeName == safeName && $0.deletedAtMs == nil
        }
    }

    func children(parent: String, after: String?, limit: UInt32) throws -> [ItemMetadata] {
        lock.lock()
        let script = childrenScripts.isEmpty ? nil : childrenScripts.removeFirst()
        let error = nextChildrenError
        nextChildrenError = nil
        lock.unlock()
        script?()
        if let error { throw error }

        lock.lock()
        defer { lock.unlock() }
        return itemsById.values
            .filter { $0.parent == parent && $0.deletedAtMs == nil }
            .sorted { $0.id < $1.id }
            .filter { item in after.map { anchor in item.id > anchor } ?? true }
            .prefix(Int(limit))
            .map { $0 }
    }

    func childrenPage(parent: String, after: String?, limit: UInt32) throws -> ItemPage {
        guard limit > 0 else {
            throw DriveError.InvalidArgument(detail: "child page limit must be positive")
        }
        if let after {
            lock.lock()
            let valid = itemsById[after]?.parent == parent
            lock.unlock()
            guard valid else {
                throw DriveError.NotFound(detail: "foreign child page anchor")
            }
        }
        let pageSize = min(limit, GramDriveEnumerator.defaultPageSize)
        let children = try children(
            parent: parent,
            after: after,
            limit: pageSize + 1)
        let hasMore = children.count > Int(pageSize)
        let items = Array(children.prefix(Int(pageSize)))
        return ItemPage(items: items, nextAfter: hasMore ? items.last?.id : nil)
    }

    func dataVersion() throws -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        return stampedDataVersion
    }

    func ensureRootStructure() throws -> RootStructureReadiness {
        throw DriveError.InvalidArgument(detail: "scripted provider is read-only")
    }

    func item(id: String) throws -> ItemMetadata? {
        lock.lock()
        defer { lock.unlock() }
        observedItemQos.append(qos_class_self())
        return itemsById[id]
    }

    var itemQos: [qos_class_t] {
        lock.lock()
        defer { lock.unlock() }
        return observedItemQos
    }

    func itemChangesSince(
        accountId: Int64, afterSequence: Int64, limit: UInt32
    ) throws -> [ItemChange] {
        lock.lock()
        let script = changeScripts.isEmpty ? nil : changeScripts.removeFirst()
        let error = nextChangeError
        nextChangeError = nil
        lock.unlock()
        script?()
        if let error { throw error }

        lock.lock()
        defer { lock.unlock() }
        return
            journal
            .filter { $0.sequence > afterSequence }
            .sorted { $0.sequence < $1.sequence }
            .prefix(Int(limit))
            .compactMap { row in
                itemsById[row.itemId].map { ItemChange(sequence: row.sequence, metadata: $0) }
            }
    }

    func liveGeneratedItems(accountId: Int64) throws -> [ItemMetadata] {
        lock.lock()
        defer { lock.unlock() }
        return itemsById.values
            .filter { $0.kind == .generatedDoc && $0.deletedAtMs == nil }
            .sorted {
                ($0.parent ?? "", $0.id) < ($1.parent ?? "", $1.id)
            }
    }

    func layout() -> SharedStateLayout {
        SharedStateLayout(
            dataRoot: "/scripted",
            stateDir: "/scripted/state",
            databaseFile: "/scripted/state/gramdrive.sqlite3",
            quarantineDir: "/scripted/state/quarantine",
            cacheDir: "/scripted/cache"
        )
    }

    func role() -> StateRole {
        .provider
    }

    func providerFetchHealth() throws -> ProviderFetchHealthCounters {
        ProviderFetchHealthCounters(
            callbackCount: 0,
            successCount: 0,
            engineFailureCount: 0,
            providerMappingCount: 0,
            noSuchItemCount: 0,
            retryableCount: 0)
    }

    func recordProviderFetchHealth(observation: ProviderFetchHealthObservation) throws {
        _ = observation
        throw DriveError.InvalidArgument(detail: "scripted provider is read-only")
    }

    func schemaVersion() throws -> Int64 {
        2
    }
}

// MARK: - Recording observers

/// Records everything an enumeration delivers. The enumerator answers
/// synchronously, so tests assert immediately after the call.
final class RecordingEnumerationObserver: NSObject, NSFileProviderEnumerationObserver,
    @unchecked Sendable
{
    private(set) var batches: [[any NSFileProviderItem]] = []
    private(set) var finishedPages: [NSFileProviderPage?] = []
    private(set) var finishError: Error?
    /// What `suggestedPageSize` reports; 0 means "no usable suggestion".
    var pageSizeSuggestion = 0

    var suggestedPageSize: Int { pageSizeSuggestion }

    /// Every delivered item identifier, in delivery order.
    var enumeratedIdentifiers: [String] {
        batches.flatMap { $0.map(\.itemIdentifier.rawValue) }
    }

    var finishCallCount: Int { finishedPages.count + (finishError == nil ? 0 : 1) }

    func didEnumerate(_ updatedItems: [any NSFileProviderItem]) {
        batches.append(updatedItems)
    }

    func finishEnumerating(upTo nextPage: NSFileProviderPage?) {
        finishedPages.append(nextPage)
    }

    func finishEnumeratingWithError(_ error: any Error) {
        finishError = error
    }
}

/// Records everything a change enumeration delivers.
final class RecordingChangeObserver: NSObject, NSFileProviderChangeObserver, @unchecked Sendable {
    private(set) var updatedBatches: [[any NSFileProviderItem]] = []
    private(set) var deletedBatches: [[NSFileProviderItemIdentifier]] = []
    private(set) var finishes: [(anchor: NSFileProviderSyncAnchor, moreComing: Bool)] = []
    private(set) var finishError: Error?
    /// What `suggestedBatchSize` reports; 0 means "no usable suggestion".
    var batchSizeSuggestion = 0

    var suggestedBatchSize: Int { batchSizeSuggestion }

    var updatedIdentifiers: [String] {
        updatedBatches.flatMap { $0.map(\.itemIdentifier.rawValue) }
    }

    var deletedIdentifiers: [String] {
        deletedBatches.flatMap { $0.map(\.rawValue) }
    }

    var finishCallCount: Int { finishes.count + (finishError == nil ? 0 : 1) }

    func didUpdate(_ updatedItems: [any NSFileProviderItem]) {
        updatedBatches.append(updatedItems)
    }

    func didDeleteItems(withIdentifiers deletedItemIdentifiers: [NSFileProviderItemIdentifier]) {
        deletedBatches.append(deletedItemIdentifiers)
    }

    func finishEnumeratingChanges(upTo anchor: NSFileProviderSyncAnchor, moreComing: Bool) {
        finishes.append((anchor: anchor, moreComing: moreComing))
    }

    func finishEnumeratingWithError(_ error: any Error) {
        finishError = error
    }
}
