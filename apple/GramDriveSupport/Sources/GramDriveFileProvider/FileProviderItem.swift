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
/// never performs.
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

    /// Logical size in bytes for files whose size is known; `nil` for
    /// directories and for files before their size is known.
    public var documentSize: NSNumber? {
        guard !metadata.isDirectory, let size = metadata.logicalSize else { return nil }
        return NSNumber(value: size)
    }

    // MARK: - Capabilities (the read-only surface)

    public var capabilities: NSFileProviderItemCapabilities {
        Self.capabilities(for: metadata)
    }

    public var fileSystemFlags: NSFileProviderFileSystemFlags {
        Self.fileSystemFlags(for: metadata)
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
    /// be enumerated; fetchable files may be read; content Telegram protects
    /// or has dropped (POL-4) advertises nothing at all. No mutating
    /// capability — write, rename, reparent, trash, delete, add-subitem —
    /// appears for any kind, which is the invariant SYNC-061 depends on to
    /// return a stable read-only error to clients that ignore capabilities.
    static func capabilities(for metadata: ItemMetadata) -> NSFileProviderItemCapabilities {
        if metadata.isDirectory {
            return .allowsContentEnumerating
        }
        switch metadata.availability {
        case .fetchable:
            return .allowsReading
        case .restricted, .unavailable:
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
        case .fetchable:
            return .userReadable
        case .restricted, .unavailable:
            return []
        }
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
