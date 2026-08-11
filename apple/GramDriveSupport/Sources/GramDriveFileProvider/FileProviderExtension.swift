import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport

/// Why the extension could not resolve its domain to a configured
/// account. Typed so tests pin the exact failure, and so the
/// `NSFileProviderItem` surface can map each case honestly.
public enum FileProviderExtensionError: Error, Equatable {
    /// The domain identifier is not one ``DomainIdentity`` produces — a
    /// foreign or corrupted domain that must never alias a real account.
    case unrecognizedDomainIdentifier(String)
    /// The identifier parses but no such account is configured in shared
    /// state (for example, mid-removal).
    case accountNotConfigured(Int64)
}

/// The thin `NSFileProviderReplicatedExtension` skeleton
/// (TASK-260715-3s44pc; PLAT-MAC-001/-002).
///
/// Thin is the design, not a stage (DEC-006): this process never hosts
/// TDLib or the engine — it opens the shared container read-only as
/// `StateRole.provider` and serves what the durable state already holds.
/// The target's only dependencies are the support package and the core
/// bindings; there is no TDLib to link. Everything Telegram happens in
/// the companion agent, on the other side of the database.
///
/// This type owns exactly the domain→account wiring: parse the domain
/// identifier, open shared state, resolve the account and its root item
/// identifier (``accountContext()``). Item mapping (TASK-260715-i3mp9x)
/// and enumeration (TASK-260715-rhcnhc) sit on top of that context, and
/// content fetch (TASK-260715-kkglhx) bridges to the companion agent's
/// hydration endpoint through ``ContentFetcher`` — the extension asks the
/// engine's host process for bytes, never a source directly.
public final class GramDriveFileProviderExtension: NSObject, NSFileProviderReplicatedExtension,
    NSFileProviderThumbnailing, @unchecked Sendable
{
    /// The account context every provider callback starts from: the
    /// configured account (including `rootItemId`, the durable identifier
    /// behind `NSFileProviderItemIdentifier.rootContainer`) and the open
    /// shared-state handle to read the tree through.
    public struct AccountContext {
        public let account: AccountInfo
        public let store: SharedStateStore
    }

    public let domain: NSFileProviderDomain

    private let resolveDataRoot: @Sendable () throws -> URL
    private let lock = NSLock()
    private var cachedStore: SharedStateStore?
    let contentFetcher: ContentFetcher
    let thumbnailFetcher: ThumbnailFetcher
    private let historyPriority: any HistoryPrioritySignaling
    private let providerFetchHealth: any ProviderFetchHealthSignaling

    /// The system's entry point: resolve shared state inside the App
    /// Group container the signed extension is entitled to.
    public required convenience init(domain: NSFileProviderDomain) {
        self.init(
            domain: domain,
            dataRoot: {
                AppGroup.dataRootURL(containerURL: try AppGroup.containerURL())
            }
        )
    }

    /// The testable entry point: same wiring over a substitute container
    /// (the shared-state layer's substitute-container rule). `hydration`
    /// and `fetchScratchDirectory` default to the real agent channel and
    /// the domain's provider-managed scratch location; tests substitute
    /// both.
    public init(
        domain: NSFileProviderDomain,
        dataRoot: @escaping @Sendable () throws -> URL,
        hydration: (any HydrationRequesting)? = nil,
        fetchScratchDirectory: (@Sendable () throws -> URL)? = nil,
        historyPriority: (any HistoryPrioritySignaling)? = nil,
        providerFetchHealth: (any ProviderFetchHealthSignaling)? = nil
    ) {
        self.domain = domain
        self.resolveDataRoot = dataRoot
        let prioritySignaler = historyPriority
            ?? AgentHistoryPriorityClient(socketURL: {
                AgentControlEndpoint.socketURL(dataRoot: try dataRoot())
            })
        self.historyPriority = prioritySignaler
        let healthSignaler = providerFetchHealth
            ?? AgentProviderFetchHealthClient(socketURL: {
                AgentControlEndpoint.socketURL(dataRoot: try dataRoot())
            })
        self.providerFetchHealth = healthSignaler
        let hydrationClient = hydration
            ?? AgentHydrationClient(socketURL: {
                HydrationContract.socketURL(dataRoot: try dataRoot())
            })
        self.contentFetcher = ContentFetcher(
            hydration: hydrationClient,
            scratchDirectory: fetchScratchDirectory
                ?? Self.providerScratchDirectory(domain: domain),
            historyPriority: prioritySignaler,
            telemetry: ProviderFetchTelemetry(health: healthSignaler))
        self.thumbnailFetcher = ThumbnailFetcher(
            hydration: hydrationClient,
            telemetry: ProviderFetchTelemetry(health: healthSignaler))
        super.init()
    }

    /// The default scratch location for fetched content: the domain's
    /// provider-managed temporary directory (same volume as the system's
    /// store, so the returned file moves without a byte copy). Outside a
    /// registered domain (tests, smoke harnesses) the extension-local
    /// temporary directory stands in.
    private static func providerScratchDirectory(
        domain: NSFileProviderDomain
    ) -> @Sendable () throws -> URL {
        let boxed = UncheckedSendable(domain)
        return {
            if let manager = NSFileProviderManager(for: boxed.value) {
                return try manager.temporaryDirectoryURL()
            }
            return FileManager.default.temporaryDirectory
                .appendingPathComponent("gramdrive-fetch-scratch", isDirectory: true)
        }
    }

    /// Resolves the domain to its configured account and an open
    /// shared-state handle. Fails typed when the domain identifier is
    /// foreign or the account is gone; each call re-reads the account
    /// row (a fresh snapshot), while the store handle is cached until
    /// ``invalidate()``.
    public func accountContext() throws -> AccountContext {
        let identifier = domain.identifier.rawValue
        guard let accountId = DomainIdentity.accountId(fromIdentifier: identifier) else {
            throw FileProviderExtensionError.unrecognizedDomainIdentifier(identifier)
        }
        let store = try openedStore()
        guard let account = try store.account(accountId: accountId) else {
            throw FileProviderExtensionError.accountNotConfigured(accountId)
        }
        return AccountContext(account: account, store: store)
    }

    public func invalidate() {
        contentFetcher.cancelAll()
        thumbnailFetcher.cancelAll()
        lock.lock()
        cachedStore = nil
        lock.unlock()
    }

    // MARK: - NSFileProviderReplicatedExtension callbacks

    /// Resolves one identifier to its mapped item (TASK-260715-i3mp9x).
    ///
    /// `.rootContainer` folds back to the account root; every other
    /// identifier is a durable id. An unknown id, or a POL-3 tombstone that
    /// is no longer live, answers `noSuchItem` — there is genuinely nothing
    /// to serve. A restricted or unavailable item (POL-4) is *not* absent:
    /// it resolves to a real item whose capability surface withholds the
    /// bytes, so the tree stays whole while the content stays ungettable.
    public func item(
        for identifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        do {
            completionHandler(try resolveItem(for: identifier), nil)
        } catch {
            completionHandler(nil, error)
        }
        return completedProgress()
    }

    /// The resolution `item(for:)` performs, minus the completion-handler
    /// and `NSFileProviderRequest` plumbing (the request has no
    /// test-constructible form). Returns the mapped item, or throws the
    /// exact error the callback reports: `noSuchItem` for an unknown id, a
    /// POL-3 tombstone, a foreign domain, or a gone account; a transient
    /// storage failure as-is, so the system retries rather than caching a
    /// false "no such item".
    func resolveItem(for identifier: NSFileProviderItemIdentifier) throws -> NSFileProviderItem {
        let context: AccountContext
        do {
            context = try accountContext()
        } catch let error as FileProviderExtensionError {
            switch error {
            case .unrecognizedDomainIdentifier, .accountNotConfigured:
                throw NSFileProviderError(.noSuchItem)
            }
        }
        let coreItemId = ItemIdentifierMapping.coreItemId(
            for: identifier, accountRootId: context.account.rootItemId)
        guard
            let metadata = try context.store.item(id: coreItemId),
            metadata.deletedAtMs == nil
        else {
            throw NSFileProviderError(.noSuchItem)
        }
        signalHistoryPriority(for: metadata, accountId: context.account.accountId, .requested)
        return GramDriveFileProviderItem(
            metadata: metadata, accountRootId: context.account.rootItemId)
    }

    /// Content fetch (TASK-260715-kkglhx): Finder opens and Quick Look both
    /// materialize through this same on-demand callback. The whole behavior lives in
    /// ``ContentFetcher`` — this callback only binds it to the domain's
    /// account context (minus the `NSFileProviderRequest` plumbing, which
    /// has no test-constructible form). A domain that does not resolve to
    /// a configured account answers `noSuchItem`, exactly like the item
    /// surface.
    ///
    /// It is also the drive's reliable "the user is in this chat" signal
    /// (BUG-260728-2qfzbd): the fetcher raises `requested` history demand for
    /// the enclosing chat while the read runs. Directory enumeration cannot
    /// carry that signal on its own, because macOS answers a read of an
    /// already-materialized folder from its own replica without ever calling
    /// this extension.
    public func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        let handler = UncheckedSendable(completionHandler)
        return fetchContentsCore(
            itemIdentifier: itemIdentifier,
            requestedVersion: requestedVersion
        ) { url, item, error in
            handler.value(url, item, error)
        }
    }

    /// The testable form of `fetchContents`.
    func fetchContentsCore(
        itemIdentifier: NSFileProviderItemIdentifier,
        requestedVersion: NSFileProviderItemVersion?,
        completionHandler: @escaping ContentFetcher.Completion
    ) -> Progress {
        contentFetcher.fetchContents(
            itemIdentifier: itemIdentifier,
            requestedVersion: requestedVersion,
            context: { [self] in
                do {
                    let context = try accountContext()
                    return (account: context.account, store: context.store)
                } catch let error as FileProviderExtensionError {
                    switch error {
                    case .unrecognizedDomainIdentifier, .accountNotConfigured:
                        throw NSFileProviderError(.noSuchItem)
                    }
                }
            },
            completionHandler: completionHandler)
    }

    /// Finder thumbnailing is a separate bounded agent operation. It never
    /// calls `fetchContents`, so asking for a preview cannot hydrate full media.
    public func fetchThumbnails(
        for itemIdentifiers: [NSFileProviderItemIdentifier],
        requestedSize size: CGSize,
        perThumbnailCompletionHandler: @escaping (
            NSFileProviderItemIdentifier, Data?, (any Error)?
        ) -> Void,
        completionHandler: @escaping ((any Error)?) -> Void
    ) -> Progress {
        let perItem = UncheckedSendable(perThumbnailCompletionHandler)
        let completion = UncheckedSendable(completionHandler)
        return fetchThumbnailsCore(
            itemIdentifiers: itemIdentifiers,
            requestedSize: size,
            perItemCompletion: { identifier, data, error in
                perItem.value(identifier, data, error)
            },
            completion: { error in completion.value(error) })
    }

    func fetchThumbnailsCore(
        itemIdentifiers: [NSFileProviderItemIdentifier],
        requestedSize: CGSize,
        perItemCompletion: @escaping ThumbnailFetcher.PerItemCompletion,
        completion: @escaping ThumbnailFetcher.Completion
    ) -> Progress {
        thumbnailFetcher.fetchThumbnails(
            itemIdentifiers: itemIdentifiers,
            requestedSize: requestedSize,
            context: { [self] in
                do {
                    let context = try accountContext()
                    return (account: context.account, store: context.store)
                } catch let error as FileProviderExtensionError {
                    switch error {
                    case .unrecognizedDomainIdentifier, .accountNotConfigured:
                        throw NSFileProviderError(.noSuchItem)
                    }
                }
            },
            perItemCompletion: perItemCompletion,
            completion: completion)
    }

    public func createItem(
        basedOn itemTemplate: NSFileProviderItem,
        fields: NSFileProviderItemFields,
        contents url: URL?,
        options: NSFileProviderCreateItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?)
            -> Void
    ) -> Progress {
        // V1 is read-only (PLAT-MAC-004 read scope); the capability
        // surface that refuses writes up front is the item-mapping task's.
        completionHandler(nil, [], false, CocoaError(.featureUnsupported))
        return completedProgress()
    }

    /// V1 is read-only with respect to Telegram (DEC-007), so every change
    /// that would have to travel *to* Telegram is refused.
    ///
    /// The three locally-owned presentation properties are the exception,
    /// and refusing them would be wrong rather than strict: `lastUsedDate`,
    /// `tagData` and `favoriteRank` never leave this Mac, and the system
    /// pushes them here so a provider can keep them. Failing the
    /// modification makes the system revert the user's own local state — a
    /// chat they just opened would snap back to the date this extension
    /// reports (BUG-260728-2qfzbd). Accepting them with no remaining pending
    /// fields lets the genuine, newer local access date win over the
    /// index-derived floor `GramDriveFileProviderItem.lastUsedDate`
    /// publishes.
    public func modifyItem(
        _ item: NSFileProviderItem,
        baseVersion version: NSFileProviderItemVersion,
        changedFields: NSFileProviderItemFields,
        contents newContents: URL?,
        options: NSFileProviderModifyItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?)
            -> Void
    ) -> Progress {
        if Self.isLocallyOwnedModification(changedFields) {
            completionHandler(item, [], false, nil)
            return completedProgress()
        }
        completionHandler(nil, [], false, CocoaError(.featureUnsupported))
        return completedProgress()
    }

    /// Whether every changed field is a presentation property this Mac owns
    /// on its own and that never maps to a Telegram write. An empty change
    /// set is trivially local: there is nothing to send anywhere.
    static func isLocallyOwnedModification(_ changedFields: NSFileProviderItemFields) -> Bool {
        let locallyOwned: NSFileProviderItemFields = [.lastUsedDate, .tagData, .favoriteRank]
        return changedFields.subtracting(locallyOwned).isEmpty
    }

    public func deleteItem(
        identifier: NSFileProviderItemIdentifier,
        baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions = [],
        request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void
    ) -> Progress {
        completionHandler(CocoaError(.featureUnsupported))
        return completedProgress()
    }

    /// An enumerator over one container (TASK-260715-rhcnhc): the working
    /// set (the domain-wide change feed on macOS), the root, or a live
    /// directory. A directory that is unknown, tombstoned, or not even a
    /// parseable identifier answers `noSuchItem`; a *file* identifier
    /// answers `featureUnsupported` (the item exists, it just has no
    /// children to enumerate), as does the trash of this read-only domain
    /// (nothing can ever be trashed — DEC-007).
    public func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        try makeEnumerator(for: containerItemIdentifier)
    }

    /// The construction `enumerator(for:request:)` performs, minus the
    /// `NSFileProviderRequest` plumbing (the request has no
    /// test-constructible form).
    func makeEnumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier
    ) throws -> NSFileProviderEnumerator {
        let context: AccountContext
        do {
            context = try accountContext()
        } catch let error as FileProviderExtensionError {
            switch error {
            case .unrecognizedDomainIdentifier, .accountNotConfigured:
                throw NSFileProviderError(.noSuchItem)
            }
        }
        if containerItemIdentifier == .trashContainer {
            throw CocoaError(.featureUnsupported)
        }
        var chatPriorityRequest: HistoryPriorityRequest?
        if containerItemIdentifier != .workingSet, containerItemIdentifier != .rootContainer {
            let coreId = ItemIdentifierMapping.coreItemId(
                for: containerItemIdentifier, accountRootId: context.account.rootItemId)
            let metadata: ItemMetadata?
            do {
                metadata = try context.store.item(id: coreId)
            } catch let error as DriveError {
                // A system-held identifier that does not even parse as a
                // core id names nothing — it is not a caller bug to retry.
                if case .InvalidArgument = error {
                    throw NSFileProviderError(.noSuchItem)
                }
                throw error
            }
            guard let metadata, metadata.deletedAtMs == nil else {
                throw NSFileProviderError(.noSuchItem)
            }
            guard metadata.isDirectory else {
                throw CocoaError(.featureUnsupported)
            }
            if metadata.kind == .chat, let chatId = metadata.chatId {
                chatPriorityRequest = HistoryPriorityRequest(
                    accountId: context.account.accountId,
                    chatId: chatId,
                    priority: .visible)
            }
        }
        return GramDriveEnumerator(
            store: context.store,
            accountId: context.account.accountId,
            container: containerItemIdentifier,
            historyPriority: historyPriority,
            chatPriorityRequest: chatPriorityRequest)
    }

    func signalHistoryPriority(
        for metadata: ItemMetadata,
        accountId: Int64,
        _ priority: HistoryPriorityHint
    ) {
        guard metadata.kind == .chat, let chatId = metadata.chatId else { return }
        historyPriority.signal(
            HistoryPriorityRequest(accountId: accountId, chatId: chatId, priority: priority))
    }

    // MARK: - Internals

    private func openedStore() throws -> SharedStateStore {
        lock.lock()
        defer { lock.unlock() }
        if let cachedStore {
            return cachedStore
        }
        let store = try SharedState.open(dataRoot: resolveDataRoot(), role: .provider)
        cachedStore = store
        return store
    }

    private func completedProgress() -> Progress {
        let progress = Progress(totalUnitCount: 1)
        progress.completedUnitCount = 1
        return progress
    }
}

/// Carries a value the SDK has not annotated `Sendable` (the domain object,
/// the system's completion handlers) across an isolation boundary the
/// platform contract already makes safe.
struct UncheckedSendable<Value>: @unchecked Sendable {
    let value: Value

    init(_ value: Value) {
        self.value = value
    }
}
