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
/// This skeleton owns exactly the domain→account wiring: parse the
/// domain identifier, open shared state, resolve the account and its
/// root item identifier (``accountContext()``). Item mapping, working-set
/// enumeration, and content fetch land on top of that context in their
/// own tasks (STORY-260715-14k4l9, STORY-260715-14n7wp); until then the
/// callbacks answer `CocoaError.featureUnsupported` rather than faking a
/// tree.
public final class GramDriveFileProviderExtension: NSObject, NSFileProviderReplicatedExtension {
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
    /// (the shared-state layer's substitute-container rule).
    public init(domain: NSFileProviderDomain, dataRoot: @escaping @Sendable () throws -> URL) {
        self.domain = domain
        self.resolveDataRoot = dataRoot
        super.init()
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
        return GramDriveFileProviderItem(
            metadata: metadata, accountRootId: context.account.rootItemId)
    }

    public func fetchContents(
        for itemIdentifier: NSFileProviderItemIdentifier,
        version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void
    ) -> Progress {
        completionHandler(nil, nil, itemError(for: itemIdentifier))
        return completedProgress()
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
        completionHandler(nil, [], false, CocoaError(.featureUnsupported))
        return completedProgress()
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

    public func enumerator(
        for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest
    ) throws -> NSFileProviderEnumerator {
        throw itemError(for: containerItemIdentifier)
    }

    // MARK: - Internals

    /// The one error rule of the skeleton: a domain that does not resolve
    /// to a configured account answers `noSuchItem` (there is genuinely
    /// nothing to serve); a resolvable domain answers
    /// `featureUnsupported` until the enumeration and content tasks land.
    /// Shared-state failures pass through as-is (the system retries).
    func itemError(for identifier: NSFileProviderItemIdentifier) -> Error {
        do {
            _ = try accountContext()
            return CocoaError(.featureUnsupported)
        } catch let error as FileProviderExtensionError {
            switch error {
            case .unrecognizedDomainIdentifier, .accountNotConfigured:
                return NSFileProviderError(.noSuchItem)
            }
        } catch {
            return error
        }
    }

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
