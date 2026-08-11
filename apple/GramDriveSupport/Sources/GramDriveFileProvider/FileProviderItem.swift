import CryptoKit
import FileProvider
import Foundation
import GramDriveCore
import UniformTypeIdentifiers

/// A read-only `NSFileProviderItem` over one core `ItemMetadata`
/// (TASK-260715-i3mp9x; DEC-007, SYNC-060).
///
/// Pure and total: every field is a deterministic function of the item's
/// durable metadata plus the account root id it lives under. That is what
/// makes the whole mapping exercisable from hand-built fixtures — DEC-006
/// keeps durable writes off the FFI, so tests cannot seed a real store, and
/// the mapping is where every provider-visible attribute is decided anyway.
///
/// Read-only is *enforced* here, not merely left out: no create / write /
/// rename / reparent / trash / delete / add-subitem capability is ever
/// advertised for any item kind (DEC-007 makes V1 read-only with respect to
/// Telegram; SYNC-060 forbids surfacing those capabilities through native
/// providers). Content Telegram protects or has dropped (POL-4) carries no
/// read capability either — the byte fetch it would gate is one the engine
/// never performs. A file with no exact projected extent is likewise kept
/// non-readable until whole-content verification is possible; an estimate is
/// never promoted into a logical-size claim.
public final class GramDriveFileProviderItem: NSObject, NSFileProviderItem {
    /// The durable metadata this item projects (DOM-001).
    public let metadata: ItemMetadata

    /// The account root's item id, needed to fold the root — and any direct
    /// child's parent — onto `NSFileProviderItemIdentifier.rootContainer`.
    private let accountRootId: String

    public init(metadata: ItemMetadata, accountRootId: String) {
        self.metadata = metadata
        self.accountRootId = accountRootId
    }

    // MARK: - Identity & hierarchy

    public var itemIdentifier: NSFileProviderItemIdentifier {
        ItemIdentifierMapping.providerIdentifier(
            forCoreItemId: metadata.id, accountRootId: accountRootId)
    }

    public var parentItemIdentifier: NSFileProviderItemIdentifier {
        ItemIdentifierMapping.parentIdentifier(
            forParentCoreItemId: metadata.parent, accountRootId: accountRootId)
    }

    /// The on-disk name is the collision-free `safeName` (SYNC-012), never
    /// the display name: siblings must stay unique on a real filesystem, and
    /// only `safeName` carries that guarantee.
    public var filename: String {
        metadata.safeName
    }

    // MARK: - Type & size

    public var contentType: UTType {
        Self.contentType(for: metadata)
    }

    /// Logical size in bytes: a file's own content extent, or a directory's
    /// exact indexed-descendant rollup (BUG-260728-2qfzbd). `nil` when no
    /// size is claimed — a file before its extent is projected, a directory
    /// before a reconciliation pass has summed it, and a directory that owns
    /// no rollup at all (a chat list or the folder catalog, whose children
    /// are chats rather than correspondence). That last case is why `nil`
    /// and zero stay distinct here: zero is "this subtree is indexed and
    /// holds no bytes", which is a claim, and `nil` is the absence of one.
    ///
    /// Publishing the rollup is what lets the system answer "how big is this
    /// chat?" from durable metadata, before the folder is enumerated and
    /// without fetching one content byte: the value is a sum of sizes the
    /// index already holds, never a download and never an estimate.
    public var documentSize: NSNumber? {
        guard let size = metadata.isDirectory ? metadata.aggregateSize : metadata.logicalSize
        else { return nil }
        return NSNumber(value: size)
    }

    // MARK: - Capabilities (the read-only surface)

    public var capabilities: NSFileProviderItemCapabilities {
        Self.capabilities(for: metadata)
    }

    public var fileSystemFlags: NSFileProviderFileSystemFlags {
        Self.fileSystemFlags(for: metadata)
    }

    // MARK: - Content policy (offline pin / eviction)

    /// How the system keeps this item's content (TASK-260715-3s461k; POL-2,
    /// SYNC-051..053): eager and eviction-proof when the item is pinned
    /// "available offline", the evictable placeholder default otherwise.
    public var contentPolicy: NSFileProviderContentPolicy {
        Self.contentPolicy(for: metadata)
    }

    // MARK: - Versioning

    public var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(
            contentVersion: Self.contentVersionData(for: metadata),
            metadataVersion: Self.metadataVersionData(for: metadata))
    }

    // MARK: - Timestamps

    public var creationDate: Date? {
        Self.date(fromEpochMs: metadata.createdAtMs)
    }

    public var contentModificationDate: Date? {
        Self.date(fromEpochMs: metadata.modifiedAtMs)
    }

    /// The item's initial "last used" date: the most recent correspondence
    /// instant the index holds for it (BUG-260728-2qfzbd).
    ///
    /// A chat the user has never opened locally still has a truthful answer
    /// to "when was this last used" — the last message in it — and that is
    /// far better than the epoch the absent property renders as. This is a
    /// *floor*, not an override: `lastUsedDate` is one of the three
    /// locally-owned presentation properties (with `tagData` and
    /// `favoriteRank`), so when the system records a genuine newer local
    /// access it pushes it back through `modifyItem`, which accepts it
    /// rather than refusing and reverting it.
    public var lastUsedDate: Date? {
        Self.date(fromEpochMs: metadata.modifiedAtMs)
    }
}

extension GramDriveFileProviderItem {
    /// Content type: directories are `.folder`; files prefer a *declared*
    /// (system-registered) type, resolved from MIME first, then from the
    /// filename extension. `UTType(mimeType:)` synthesizes a dynamic
    /// (`dyn…`) type for an unrecognized MIME rather than failing, so a bare
    /// non-nil check would let a useless dynamic MIME type shadow a perfectly
    /// good extension match — hence the declared-first ordering. When neither
    /// source is registered, the extension's dynamic type is kept over the
    /// MIME's (it drives Finder's "Open With"), and `.data` is the last
    /// resort so an unknown file is still a concrete, openable node.
    static func contentType(for metadata: ItemMetadata) -> UTType {
        if metadata.isDirectory {
            return .folder
        }
        let fromMime = metadata.mimeType.flatMap { UTType(mimeType: $0) }
        let ext = (metadata.safeName as NSString).pathExtension
        let fromExtension = ext.isEmpty ? nil : UTType(filenameExtension: ext)

        if let fromMime, fromMime.isDeclared {
            return fromMime
        }
        if let fromExtension, fromExtension.isDeclared {
            return fromExtension
        }
        return fromExtension ?? fromMime ?? .data
    }

    /// The read-only capability surface (DEC-007 / SYNC-060). Directories may
    /// be enumerated; fetchable files with an exact extent may be read;
    /// content Telegram protects or has dropped (POL-4) advertises nothing at
    /// all. No mutating
    /// capability — write, rename, reparent, trash, delete, add-subitem —
    /// appears for any kind, which is the invariant SYNC-061 depends on to
    /// return a stable read-only error to clients that ignore capabilities.
    /// TDLib's `expected_size` remains nil in the projection and cannot
    /// justify a whole-content read capability.
    static func capabilities(for metadata: ItemMetadata) -> NSFileProviderItemCapabilities {
        if metadata.isDirectory {
            return .allowsContentEnumerating
        }
        switch metadata.availability {
        case .fetchable where metadata.logicalSize != nil:
            return .allowsReading
        case .fetchable, .restricted, .unavailable:
            return []
        }
    }

    /// A POSIX-flag echo of the capability surface: readable (and, for
    /// directories, traversable), never writable. Restricted or unavailable
    /// content is not even readable, matching its empty capability set.
    static func fileSystemFlags(for metadata: ItemMetadata) -> NSFileProviderFileSystemFlags {
        if metadata.isDirectory {
            return [.userReadable, .userExecutable]
        }
        switch metadata.availability {
        case .fetchable where metadata.logicalSize != nil:
            return .userReadable
        case .fetchable, .restricted, .unavailable:
            return []
        }
    }

    /// Maps durable offline-pin state onto the system's content policy — the
    /// provider half of pin/eviction reconciliation (SYNC-051..053, POL-2).
    ///
    /// A pin — an explicit user "available offline" or Archive-Mode coverage —
    /// means keep the bytes: the system downloads eagerly and never evicts
    /// under disk pressure (SYNC-051), which is what makes pinned content
    /// quota-exempt from the user's view. A directory pin propagates to
    /// inheriting children, so Archive-Mode coverage of a scope keeps its
    /// whole subtree materialized. Only content that can actually be fetched
    /// is ever marked eager: restricted or dropped content (POL-4) carries
    /// bytes the engine never fetches, so eager-pinning it would ask the
    /// system to retry a fetch that can never land — it takes the default.
    ///
    /// Without a pin, content is the dataless placeholder default (POL-2):
    /// files download lazily on open and stay independently evictable under
    /// pressure (SYNC-052) even beneath an eager ancestor; directories
    /// inherit, so the (lazy) root default flows down the tree while an eager
    /// ancestor pin still reaches its as-yet-unpinned descendants.
    static func contentPolicy(for metadata: ItemMetadata) -> NSFileProviderContentPolicy {
        if metadata.pin != nil, metadata.isDirectory || metadata.availability == .fetchable {
            return .downloadEagerlyAndKeepDownloaded
        }
        return metadata.isDirectory ? .inherited : .downloadLazily
    }

    static func metadataVersionData(for metadata: ItemMetadata) -> Data {
        versionComponent(metadata.metadataVersion)
    }

    /// Content version: the bytes' token when known; a fixed non-empty
    /// sentinel otherwise — directories, and files before any content is
    /// known (DOM-003 leaves the token absent there). The sentinel byte is
    /// `0x00`, an ASCII control byte core forbids in a real token, so it can
    /// never collide with one: the first real content version is always a
    /// change the system acts on.
    static func contentVersionData(for metadata: ItemMetadata) -> Data {
        guard let token = metadata.contentVersion else { return absentVersionSentinel }
        return versionComponent(token)
    }

    static func date(fromEpochMs milliseconds: Int64?) -> Date? {
        guard let milliseconds else { return nil }
        return Date(timeIntervalSince1970: Double(milliseconds) / 1000)
    }

    /// A non-empty version component of at most 128 bytes, as
    /// `NSFileProviderItemVersion` requires. Core caps tokens at 256 bytes
    /// (`MAX_VERSION_TOKEN_BYTES`) — twice the File Provider limit — so a
    /// token longer than 128 bytes folds to its SHA-256 digest. The fold is
    /// equality-preserving in the way versioning needs: equal tokens keep
    /// equal components, and distinct tokens keep (bar cryptographic
    /// collision) distinct ones, always in a fixed 32 bytes.
    static func versionComponent(_ token: String) -> Data {
        let bytes = Data(token.utf8)
        if bytes.isEmpty {
            return absentVersionSentinel
        }
        if bytes.count <= maxVersionComponentBytes {
            return bytes
        }
        return Data(SHA256.hash(data: bytes))
    }

    /// `NSFileProviderItemVersion`'s per-component byte ceiling.
    static let maxVersionComponentBytes = 128

    /// The stand-in for an absent (or, defensively, empty) version token.
    /// `0x00` is an ASCII control byte core rejects in any real token, so it
    /// is provably distinct from every version the store can produce.
    static let absentVersionSentinel = Data([0x00])
}
