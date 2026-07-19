import FileProvider
import Foundation
import GramDriveCore

/// The one enumerator type of the provider (TASK-260715-rhcnhc;
/// PLAT-MAC-004, SYNC-003, NFR-021): paged item listing for directory
/// containers, and journal-anchored change enumeration for every container
/// including the working set.
///
/// # Listing (SYNC-003)
///
/// Items are served in the stable core identifier order the state's
/// `children` pages by, one keyset page per callback: the continuation
/// page records the last identifier delivered, so no item is ever
/// duplicated across pages and every item that exists throughout the
/// listing appears exactly once. Memory is bounded by the page size, never
/// by the directory (NFR-021). A commit landing *between* pages can add or
/// retire items around the anchor; whatever a page-composed listing missed
/// is exactly what change enumeration replays next — the anchor the system
/// holds was minted before the listing began, so nothing is lost, only
/// delivered on the change path (over-delivery is idempotent, loss would
/// not be).
///
/// # Changes
///
/// Change enumeration pages the durable item change journal
/// (`gramdrive-ffi::shared_state`): each batch delivers items' current
/// state — a POL-3 tombstone as a deletion, everything else as an update —
/// and finishes at the last delivered journal sequence. Anchors are
/// validated against the account, its namespace epoch, and the journal
/// instance ([`EnumerationSyncAnchor`]); a foreign or overtaken anchor
/// answers `.syncAnchorExpired`, the platform's explicit full-resync
/// recovery.
///
/// # Working set
///
/// On macOS the system enumerates only *changes* on the working set — it
/// is the domain-wide change feed `signalEnumerator` points at — so item
/// enumeration answers an empty listing rather than faking a tree walk,
/// and change enumeration serves the same journal as every container.
///
/// # Deadlines and cancellation
///
/// Every callback answers synchronously from short snapshot reads — no
/// waiting on the network, the engine, or another process — so completion
/// is prompt by construction and `invalidate` has nothing in flight to
/// cancel.
public final class GramDriveEnumerator: NSObject, NSFileProviderEnumerator {
    /// The default listing/change page size. Bounds callback memory
    /// (NFR-021); the system's `suggestedPageSize`/`suggestedBatchSize`
    /// caps it further when present.
    public static let defaultPageSize: UInt32 = 256

    private let store: any SharedStateStoreProtocol
    private let accountId: Int64
    private let container: NSFileProviderItemIdentifier
    private let pageSize: UInt32

    /// An enumerator over one container of one account's tree. `container`
    /// is either a directory's identifier (`.rootContainer` included) or
    /// `.workingSet`; the extension's `enumerator(for:request:)` owns
    /// refusing everything else.
    public init(
        store: any SharedStateStoreProtocol,
        accountId: Int64,
        container: NSFileProviderItemIdentifier,
        pageSize: UInt32 = GramDriveEnumerator.defaultPageSize
    ) {
        self.store = store
        self.accountId = accountId
        self.container = container
        self.pageSize = pageSize
    }

    /// Nothing to cancel: every callback completed synchronously before
    /// this can be called.
    public func invalidate() {}

    // MARK: - Item listing

    public func enumerateItems(
        for observer: NSFileProviderEnumerationObserver,
        startingAt page: NSFileProviderPage
    ) {
        if container == .workingSet {
            // macOS enumerates only changes on the working set (type docs);
            // an empty listing is the honest answer, not a claim that the
            // domain is empty — containers are where structure comes from.
            observer.finishEnumerating(upTo: nil)
            return
        }
        do {
            let account = try resolveAccount()
            let parent = ItemIdentifierMapping.coreItemId(
                for: container, accountRootId: account.rootItemId)
            // A container that is gone — or was never an item — answers
            // noSuchItem. The account root is exempt: a configured account
            // whose tree has not been written yet lists as empty, it is not
            // absent.
            if container != .rootContainer {
                guard
                    let metadata = try liveItem(id: parent),
                    metadata.deletedAtMs == nil
                else {
                    observer.finishEnumeratingWithError(NSFileProviderError(.noSuchItem))
                    return
                }
            }
            let after = try EnumerationPageCursor.startAnchor(of: page, parent: parent)
            let limit = effectiveLimit(suggested: observer.suggestedPageSize)
            let children = try store.children(parent: parent, after: after, limit: limit)
            if !children.isEmpty {
                observer.didEnumerate(
                    children.map {
                        GramDriveFileProviderItem(metadata: $0, accountRootId: account.rootItemId)
                    })
            }
            if children.count == Int(limit), let last = children.last {
                observer.finishEnumerating(
                    upTo: EnumerationPageCursor.page(parent: parent, after: last.id))
            } else {
                observer.finishEnumerating(upTo: nil)
            }
        } catch is EnumerationPageCursorError {
            // The platform's explicit recovery for an unusable page: the
            // system restarts from the initial page. Guessing a position
            // instead could duplicate or skip items (SYNC-003).
            observer.finishEnumeratingWithError(NSFileProviderError(.pageExpired))
        } catch {
            observer.finishEnumeratingWithError(error)
        }
    }

    // MARK: - Change enumeration

    public func enumerateChanges(
        for observer: NSFileProviderChangeObserver,
        from syncAnchor: NSFileProviderSyncAnchor
    ) {
        do {
            let account = try resolveAccount()
            let journal = try store.changeJournalState()
            guard
                let anchor = EnumerationSyncAnchor.decode(syncAnchor),
                anchor.isCurrent(account: account, journal: journal)
            else {
                observer.finishEnumeratingWithError(NSFileProviderError(.syncAnchorExpired))
                return
            }
            let limit = effectiveLimit(suggested: observer.suggestedBatchSize)
            let changes = try store.itemChangesSince(
                accountId: accountId, afterSequence: anchor.sequence, limit: limit)

            var updated: [any NSFileProviderItem] = []
            var deleted: [NSFileProviderItemIdentifier] = []
            for change in changes {
                if change.metadata.deletedAtMs != nil {
                    deleted.append(
                        ItemIdentifierMapping.providerIdentifier(
                            forCoreItemId: change.metadata.id,
                            accountRootId: account.rootItemId))
                } else {
                    updated.append(
                        GramDriveFileProviderItem(
                            metadata: change.metadata, accountRootId: account.rootItemId))
                }
            }
            if !updated.isEmpty {
                observer.didUpdate(updated)
            }
            if !deleted.isEmpty {
                observer.didDeleteItems(withIdentifiers: deleted)
            }
            let next = EnumerationSyncAnchor(
                accountId: account.accountId,
                namespaceVersion: account.namespaceVersion,
                journalInstance: journal.instanceId,
                sequence: changes.last?.sequence ?? anchor.sequence)
            observer.finishEnumeratingChanges(
                upTo: next.rawAnchor(), moreComing: changes.count == Int(limit))
        } catch {
            observer.finishEnumeratingWithError(error)
        }
    }

    /// The current anchor: the journal's high-water mark under today's
    /// account epoch and journal instance. Stateless, so minting is safe at
    /// any moment — the system requests it before listing, and a change
    /// committed while pages stream is *behind* the minted anchor, replayed
    /// by the next change enumeration rather than lost.
    public func currentSyncAnchor(
        completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void
    ) {
        do {
            let account = try resolveAccount()
            let journal = try store.changeJournalState()
            completionHandler(
                EnumerationSyncAnchor(
                    accountId: account.accountId,
                    namespaceVersion: account.namespaceVersion,
                    journalInstance: journal.instanceId,
                    sequence: journal.latestSequence
                ).rawAnchor())
        } catch {
            completionHandler(nil)
        }
    }

    // MARK: - Internals

    /// The account, freshly resolved — each callback is its own snapshot,
    /// like every other extension callback. A mid-removal account answers
    /// `noSuchItem`.
    private func resolveAccount() throws -> AccountInfo {
        guard let account = try store.account(accountId: accountId) else {
            throw NSFileProviderError(.noSuchItem)
        }
        return account
    }

    /// One item read where a malformed identifier means "no such item":
    /// container identifiers come from the system, and one that cannot even
    /// parse as a core id names nothing rather than being a caller bug.
    private func liveItem(id: String) throws -> ItemMetadata? {
        do {
            return try store.item(id: id)
        } catch let error as DriveError {
            if case .InvalidArgument = error {
                return nil
            }
            throw error
        }
    }

    /// The configured page size, capped by the observer's suggestion when
    /// it offers a usable one.
    private func effectiveLimit(suggested: Int?) -> UInt32 {
        guard let suggested, suggested > 0 else { return pageSize }
        return min(pageSize, UInt32(suggested))
    }
}
