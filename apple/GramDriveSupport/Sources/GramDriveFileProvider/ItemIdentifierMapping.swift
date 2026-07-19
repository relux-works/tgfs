import FileProvider
import Foundation

/// Translation between core item identifiers and File Provider ones — the
/// one place the two namespaces differ (TASK-260715-i3mp9x).
///
/// Core identifies every node, including the account root, by the durable
/// text form of its `ItemId` (DOM-024). File Provider reserves
/// `NSFileProviderItemIdentifier.rootContainer` for a domain's top-level
/// directory and passes it to `item(for:)` / `enumerator(for:)`; it has no
/// notion of the account root's real id. Every crossing of that boundary
/// goes through here so the reserved value and the durable id never leak
/// into each other's namespace — the root is folded onto `.rootContainer`
/// on the way out and resolved back to `AccountInfo.rootItemId` on the way
/// in, and no other identifier is ever rewritten.
public enum ItemIdentifierMapping {
    /// The File Provider identifier for a core item id under an account
    /// whose root is `accountRootId`: the account root becomes
    /// `.rootContainer`; every other id passes through verbatim.
    public static func providerIdentifier(
        forCoreItemId coreItemId: String,
        accountRootId: String
    ) -> NSFileProviderItemIdentifier {
        coreItemId == accountRootId
            ? .rootContainer
            : NSFileProviderItemIdentifier(rawValue: coreItemId)
    }

    /// The File Provider identifier for an item's parent id. `parentCoreItemId`
    /// is `nil` only for the account root (DOM-001), which is its own parent
    /// by File Provider convention; a direct child of the root reparents onto
    /// `.rootContainer`; everything deeper passes through verbatim.
    public static func parentIdentifier(
        forParentCoreItemId parentCoreItemId: String?,
        accountRootId: String
    ) -> NSFileProviderItemIdentifier {
        guard let parentCoreItemId else { return .rootContainer }
        return providerIdentifier(forCoreItemId: parentCoreItemId, accountRootId: accountRootId)
    }

    /// The core item id a File Provider identifier resolves to:
    /// `.rootContainer` resolves to the account root; every other identifier
    /// is a durable id verbatim. This only undoes the reserved-value
    /// substitution — the caller still confirms the id exists in the store.
    public static func coreItemId(
        for identifier: NSFileProviderItemIdentifier,
        accountRootId: String
    ) -> String {
        identifier == .rootContainer ? accountRootId : identifier.rawValue
    }
}
