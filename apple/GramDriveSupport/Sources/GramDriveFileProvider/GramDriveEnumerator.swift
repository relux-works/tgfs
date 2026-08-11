import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport

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
/// Every callback reads only one bounded local snapshot page. A watchdog
/// still guards that read because a damaged or locked SQLite connection must
/// not become Finder's indefinite spinner. Timeout and invalidation race
/// through one completion gate, so the observer completes exactly once; a
/// later request gets a fresh read and is the retry path.
public final class GramDriveEnumerator: NSObject, NSFileProviderEnumerator, @unchecked Sendable {
    /// The default listing/change page size. Bounds callback memory
    /// (NFR-021); the system's `suggestedPageSize`/`suggestedBatchSize`
    /// caps it further when present.
    public static let defaultPageSize: UInt32 = 256
    /// Maximum wall-clock time before an item-listing or change observer
    /// receives a retryable error. The page read may unwind later, but cannot
    /// complete the observer a second time.
    public static let defaultEnumerationTimeout: TimeInterval = 8
    private static let watchdogQueue = DispatchQueue(
        label: "com.reluxworks.gramdrive.fileprovider.enumeration-watchdog",
        qos: .userInitiated,
        attributes: .concurrent)

    private let store: any SharedStateStoreProtocol
    private let accountId: Int64
    private let container: NSFileProviderItemIdentifier
    private let pageSize: UInt32
    private let enumerationTimeout: TimeInterval
    private let historyPriority: (any HistoryPrioritySignaling)?
    private let chatPriorityRequest: HistoryPriorityRequest?
    private let requestLock = NSLock()
    private var listingRequests: [UUID: ListingRequest] = [:]
    private var changeRequests: [UUID: ChangeRequest] = [:]
    private var isInvalidated = false

    /// An enumerator over one container of one account's tree. `container`
    /// is either a directory's identifier (`.rootContainer` included) or
    /// `.workingSet`; the extension's `enumerator(for:request:)` owns
    /// refusing everything else.
    public init(
        store: any SharedStateStoreProtocol,
        accountId: Int64,
        container: NSFileProviderItemIdentifier,
        pageSize: UInt32 = GramDriveEnumerator.defaultPageSize,
        enumerationTimeout: TimeInterval = GramDriveEnumerator.defaultEnumerationTimeout,
        historyPriority: (any HistoryPrioritySignaling)? = nil,
        chatPriorityRequest: HistoryPriorityRequest? = nil
    ) {
        self.store = store
        self.accountId = accountId
        self.container = container
        self.pageSize = min(max(pageSize, 1), Self.defaultPageSize)
        self.enumerationTimeout = max(0.001, enumerationTimeout)
        self.historyPriority = historyPriority
        self.chatPriorityRequest = chatPriorityRequest
    }

    /// Cancels every observer that has not already completed. The underlying
    /// local read is allowed to unwind, but its result is discarded.
    public func invalidate() {
        requestLock.lock()
        let wasInvalidated = isInvalidated
        isInvalidated = true
        let listingRequests = Array(self.listingRequests.values)
        let changeRequests = Array(self.changeRequests.values)
        self.listingRequests.removeAll()
        self.changeRequests.removeAll()
        requestLock.unlock()
        if !wasInvalidated, var request = chatPriorityRequest {
            request.priority = .background
            historyPriority?.signal(request)
        }
        for request in listingRequests {
            request.resolve(.failure(CocoaError(.userCancelled)))
        }
        for request in changeRequests {
            request.resolve(.failure(CocoaError(.userCancelled)))
        }
    }

    // MARK: - Item listing

    public func enumerateItems(
        for observer: NSFileProviderEnumerationObserver,
        startingAt page: NSFileProviderPage
    ) {
        if var request = chatPriorityRequest {
            request.priority = .visible
            historyPriority?.signal(request)
        }
        let id = UUID()
        let request = ListingRequest(observer: observer) { [weak self] in
            self?.forgetListingRequest(id)
        }
        guard registerListingRequest(request, id: id) else {
            request.resolve(.failure(CocoaError(.userCancelled)))
            return
        }

        let watchdog = DispatchSource.makeTimerSource(queue: Self.watchdogQueue)
        watchdog.schedule(deadline: .now() + enumerationTimeout)
        watchdog.setEventHandler { [request] in
            request.resolve(.failure(NSFileProviderError(.cannotSynchronize)))
        }
        watchdog.resume()
        defer { watchdog.cancel() }

        request.resolve(
            listingOutcome(
                startingAt: page,
                suggestedPageSize: observer.suggestedPageSize))
    }

    private func listingOutcome(
        startingAt page: NSFileProviderPage,
        suggestedPageSize: Int?
    ) -> ListingOutcome {
        if container == .workingSet {
            // macOS enumerates only changes on the working set (type docs);
            // an empty listing is the honest answer, not a claim that the
            // domain is empty — containers are where structure comes from.
            return .success(items: [], nextPage: nil)
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
                    return .failure(NSFileProviderError(.noSuchItem))
                }
            }
            let journal = try store.changeJournalState()
            let after = try EnumerationPageCursor.startAnchor(
                of: page, parent: parent, account: account, journal: journal)
            let limit = effectiveLimit(suggested: suggestedPageSize)
            let result: ItemPage
            do {
                result = try store.childrenPage(parent: parent, after: after, limit: limit)
            } catch let error as DriveError {
                if case .NotFound = error {
                    return .failure(NSFileProviderError(.pageExpired))
                }
                throw error
            }
            let children = result.items
            let items: [any NSFileProviderItem] = children.map {
                GramDriveFileProviderItem(metadata: $0, accountRootId: account.rootItemId)
            }
            if let nextAfter = result.nextAfter {
                return .success(
                    items: items,
                    nextPage: EnumerationPageCursor.page(
                        parent: parent,
                        after: nextAfter,
                        account: account,
                        journal: journal))
            } else {
                return .success(items: items, nextPage: nil)
            }
        } catch is EnumerationPageCursorError {
            // The platform's explicit recovery for an unusable page: the
            // system restarts from the initial page. Guessing a position
            // instead could duplicate or skip items (SYNC-003).
            return .failure(NSFileProviderError(.pageExpired))
        } catch let error as DriveError {
            return .failure(Self.providerError(for: error))
        } catch {
            return .failure(error)
        }
    }

    // MARK: - Change enumeration

    public func enumerateChanges(
        for observer: NSFileProviderChangeObserver,
        from syncAnchor: NSFileProviderSyncAnchor
    ) {
        // Reopening a folder the system has already listed answers from the
        // change feed, not `enumerateItems`. It is the same user gesture and
        // must raise the same hint — otherwise the second open of a chat only
        // ever produced `invalidate()`'s release, actively *removing* demand
        // for a chat the user just opened (BUG-260728-2qfzbd).
        if var request = chatPriorityRequest {
            request.priority = .visible
            historyPriority?.signal(request)
        }
        let id = UUID()
        let request = ChangeRequest(observer: observer) { [weak self] in
            self?.forgetChangeRequest(id)
        }
        guard registerChangeRequest(request, id: id) else {
            request.resolve(.failure(CocoaError(.userCancelled)))
            return
        }

        let watchdog = DispatchSource.makeTimerSource(queue: Self.watchdogQueue)
        watchdog.schedule(deadline: .now() + enumerationTimeout)
        watchdog.setEventHandler { [request] in
            request.resolve(.failure(NSFileProviderError(.cannotSynchronize)))
        }
        watchdog.resume()
        defer { watchdog.cancel() }

        request.resolve(
            changeOutcome(from: syncAnchor, suggestedBatchSize: observer.suggestedBatchSize))
    }

    private func changeOutcome(
        from syncAnchor: NSFileProviderSyncAnchor,
        suggestedBatchSize: Int?
    ) -> ChangeOutcome {
        do {
            let account = try resolveAccount()
            let journal = try store.changeJournalState()
            guard
                let anchor = EnumerationSyncAnchor.decode(syncAnchor),
                anchor.isCurrent(account: account, journal: journal)
            else {
                return .failure(NSFileProviderError(.syncAnchorExpired))
            }
            let limit = effectiveLimit(suggested: suggestedBatchSize)
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
            let next = EnumerationSyncAnchor(
                accountId: account.accountId,
                namespaceVersion: account.namespaceVersion,
                journalInstance: journal.instanceId,
                sequence: changes.last?.sequence ?? anchor.sequence)
            return .success(
                updated: updated,
                deleted: deleted,
                anchor: next.rawAnchor(),
                moreComing: changes.count == Int(limit))
        } catch let error as DriveError {
            return .failure(Self.providerError(for: error))
        } catch {
            return .failure(error)
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
        guard account.authState == "authorized" else {
            throw DriveError.AuthRequired(detail: "account authorization is not usable")
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
        return min(pageSize, UInt32(clamping: suggested))
    }

    private func registerListingRequest(_ request: ListingRequest, id: UUID) -> Bool {
        requestLock.lock()
        defer { requestLock.unlock() }
        guard !isInvalidated else { return false }
        listingRequests[id] = request
        return true
    }

    private func forgetListingRequest(_ id: UUID) {
        requestLock.lock()
        listingRequests[id] = nil
        requestLock.unlock()
    }

    private func registerChangeRequest(_ request: ChangeRequest, id: UUID) -> Bool {
        requestLock.lock()
        defer { requestLock.unlock() }
        guard !isInvalidated else { return false }
        changeRequests[id] = request
        return true
    }

    private func forgetChangeRequest(_ id: UUID) {
        requestLock.lock()
        changeRequests[id] = nil
        requestLock.unlock()
    }

    /// Maps durable/source failures to errors File Provider can recover from
    /// without parsing diagnostic strings.
    private static func providerError(for error: DriveError) -> any Error {
        switch error {
        case .NotFound:
            return NSFileProviderError(.noSuchItem)
        case .AuthRequired:
            return NSFileProviderError(.notAuthenticated)
        case .RateLimited, .SourceUnavailable:
            return NSFileProviderError(.serverUnreachable)
        case .Cancelled:
            return CocoaError(.userCancelled)
        case .InvalidArgument, .Storage, .Integrity, .Restricted, .VersionConflict, .Internal:
            return NSFileProviderError(.cannotSynchronize)
        }
    }
}

private enum ListingOutcome {
    case success(items: [any NSFileProviderItem], nextPage: NSFileProviderPage?)
    case failure(any Error)
}

private enum ChangeOutcome {
    case success(
        updated: [any NSFileProviderItem],
        deleted: [NSFileProviderItemIdentifier],
        anchor: NSFileProviderSyncAnchor,
        moreComing: Bool)
    case failure(any Error)
}

/// One exactly-once gate around an NSFileProviderEnumerationObserver.
private final class ListingRequest: @unchecked Sendable {
    private let lock = NSLock()
    private let observer: NSFileProviderEnumerationObserver
    private let onFinish: @Sendable () -> Void
    private var completed = false

    init(
        observer: NSFileProviderEnumerationObserver,
        onFinish: @escaping @Sendable () -> Void
    ) {
        self.observer = observer
        self.onFinish = onFinish
    }

    func resolve(_ outcome: ListingOutcome) {
        lock.lock()
        guard !completed else {
            lock.unlock()
            return
        }
        completed = true
        lock.unlock()

        switch outcome {
        case .success(let items, let nextPage):
            if !items.isEmpty {
                observer.didEnumerate(items)
            }
            observer.finishEnumerating(upTo: nextPage)
        case .failure(let error):
            observer.finishEnumeratingWithError(error)
        }
        onFinish()
    }
}

/// One exactly-once gate around an NSFileProviderChangeObserver.
private final class ChangeRequest: @unchecked Sendable {
    private let lock = NSLock()
    private let observer: NSFileProviderChangeObserver
    private let onFinish: @Sendable () -> Void
    private var completed = false

    init(
        observer: NSFileProviderChangeObserver,
        onFinish: @escaping @Sendable () -> Void
    ) {
        self.observer = observer
        self.onFinish = onFinish
    }

    func resolve(_ outcome: ChangeOutcome) {
        lock.lock()
        guard !completed else {
            lock.unlock()
            return
        }
        completed = true
        lock.unlock()

        switch outcome {
        case .success(let updated, let deleted, let anchor, let moreComing):
            if !updated.isEmpty {
                observer.didUpdate(updated)
            }
            if !deleted.isEmpty {
                observer.didDeleteItems(withIdentifiers: deleted)
            }
            observer.finishEnumeratingChanges(upTo: anchor, moreComing: moreComing)
        case .failure(let error):
            observer.finishEnumeratingWithError(error)
        }
        onFinish()
    }
}
