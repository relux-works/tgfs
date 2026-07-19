import CryptoKit
import FileProvider
import Foundation
import GramDriveCore
import UniformTypeIdentifiers
import Testing

@testable import GramDriveFileProvider

// MARK: - Fixtures

/// Every `ItemKind` the core projection defines. Kept as an explicit list so
/// a new kind forces this suite to acknowledge it (see also
/// ``expectedIsDirectory(_:)``, whose exhaustive switch is the compile-time
/// guard).
private let allItemKinds: [ItemKind] = [
    .account, .chatList, .folderCatalog, .chat, .yearDir, .mediaDir,
    .attachment, .generatedDoc, .orderDoc,
]

/// The directory/file split, restated from the core's `kind_is_directory`.
/// The switch is exhaustive, so adding an `ItemKind` breaks compilation here
/// until its directory-ness is declared.
private func expectedIsDirectory(_ kind: ItemKind) -> Bool {
    switch kind {
    case .account, .chatList, .folderCatalog, .chat, .yearDir, .mediaDir:
        return true
    case .attachment, .generatedDoc, .orderDoc:
        return false
    }
}

/// The mutating capabilities that must never appear on any item (DEC-007 /
/// SYNC-060). If any of these leaks, a native client could try to change
/// Telegram through the filesystem.
private let mutatingCapabilities: NSFileProviderItemCapabilities = [
    .allowsWriting,
    .allowsRenaming,
    .allowsReparenting,
    .allowsTrashing,
    .allowsDeleting,
    .allowsAddingSubItems,
]

private let allAvailabilities: [ItemAvailability] = [.fetchable, .restricted, .unavailable]

private func makeMetadata(
    id: String = "item-1",
    parent: String? = "root-1",
    kind: ItemKind = .attachment,
    isDirectory: Bool? = nil,
    displayName: String = "Display Name",
    safeName: String = "safe-name.bin",
    metadataVersion: String = "m1",
    mimeType: String? = nil,
    logicalSize: UInt64? = nil,
    contentVersion: String? = nil,
    availability: ItemAvailability = .fetchable,
    createdAtMs: Int64? = nil,
    modifiedAtMs: Int64? = nil,
    deletedAtMs: Int64? = nil
) -> ItemMetadata {
    ItemMetadata(
        id: id,
        parent: parent,
        kind: kind,
        isDirectory: isDirectory ?? expectedIsDirectory(kind),
        displayName: displayName,
        safeName: safeName,
        metadataVersion: metadataVersion,
        mimeType: mimeType,
        logicalSize: logicalSize,
        contentVersion: contentVersion,
        availability: availability,
        createdAtMs: createdAtMs,
        modifiedAtMs: modifiedAtMs,
        deletedAtMs: deletedAtMs
    )
}

private func makeItem(_ metadata: ItemMetadata, accountRootId: String = "root-1")
    -> GramDriveFileProviderItem
{
    GramDriveFileProviderItem(metadata: metadata, accountRootId: accountRootId)
}

// MARK: - Kind coverage

@Suite("File Provider item mapping — every kind")
struct FileProviderItemKindTests {
    @Test("Every kind maps to a folder-or-file content type matching its directory-ness")
    func everyKindMapsContentTypeByDirectoryness() {
        for kind in allItemKinds {
            let item = makeItem(makeMetadata(kind: kind, safeName: "node"))
            if expectedIsDirectory(kind) {
                #expect(item.contentType == .folder, "kind \(kind) should be a folder")
                #expect(item.documentSize == nil, "directory \(kind) has no document size")
            } else {
                #expect(item.contentType != .folder, "file kind \(kind) is not a folder")
            }
        }
    }

    @Test("Directory kinds advertise only content enumeration; no read, no writes")
    func directoryKindsEnumerateOnly() {
        for kind in allItemKinds where expectedIsDirectory(kind) {
            let item = makeItem(makeMetadata(kind: kind))
            #expect(item.capabilities == .allowsContentEnumerating)
            #expect(item.fileSystemFlags == [.userReadable, .userExecutable])
        }
    }

    @Test("No kind at any availability leaks a mutating capability")
    func noWriteOrDeleteCapabilityLeaks() {
        for kind in allItemKinds {
            for availability in allAvailabilities {
                let item = makeItem(makeMetadata(kind: kind, availability: availability))
                let leaked = item.capabilities.intersection(mutatingCapabilities)
                #expect(
                    leaked.isEmpty,
                    "kind \(kind)/\(availability) leaked capabilities \(leaked.rawValue)")
                #expect(!item.fileSystemFlags.contains(.userWritable))
            }
        }
    }
}

// MARK: - Availability → capability surface

@Suite("File Provider item mapping — availability surface (POL-4)")
struct FileProviderItemAvailabilityTests {
    @Test("A fetchable file is readable and nothing more")
    func fetchableFileIsReadable() {
        let item = makeItem(makeMetadata(kind: .attachment, availability: .fetchable))
        #expect(item.capabilities == .allowsReading)
        #expect(item.fileSystemFlags == .userReadable)
    }

    @Test("Restricted content advertises no capability and no readable flag")
    func restrictedFileIsFullyWithheld() {
        let item = makeItem(makeMetadata(kind: .attachment, availability: .restricted))
        #expect(item.capabilities == [])
        #expect(item.fileSystemFlags == [])
    }

    @Test("Unavailable content advertises no capability and no readable flag")
    func unavailableFileIsFullyWithheld() {
        let item = makeItem(makeMetadata(kind: .attachment, availability: .unavailable))
        #expect(item.capabilities == [])
        #expect(item.fileSystemFlags == [])
    }

    @Test("A restricted item still carries its size and type — only the bytes are withheld")
    func restrictedItemKeepsMetadata() {
        let item = makeItem(
            makeMetadata(
                kind: .attachment,
                safeName: "photo.jpg",
                mimeType: "image/jpeg",
                logicalSize: 4096,
                availability: .restricted))
        #expect(item.contentType == .jpeg)
        #expect(item.documentSize == NSNumber(value: 4096))
    }
}

// MARK: - Identifier & hierarchy mapping

@Suite("File Provider item mapping — identifiers")
struct FileProviderItemIdentifierTests {
    @Test("The account root folds onto rootContainer, and is its own parent")
    func accountRootFoldsToRootContainer() {
        let item = makeItem(
            makeMetadata(id: "root-1", parent: nil, kind: .account),
            accountRootId: "root-1")
        #expect(item.itemIdentifier == .rootContainer)
        #expect(item.parentItemIdentifier == .rootContainer)
    }

    @Test("A direct child of the root reparents onto rootContainer, keeps its own id")
    func directChildReparentsOntoRoot() {
        let item = makeItem(
            makeMetadata(id: "chat-9", parent: "root-1", kind: .chat),
            accountRootId: "root-1")
        #expect(item.itemIdentifier == NSFileProviderItemIdentifier(rawValue: "chat-9"))
        #expect(item.parentItemIdentifier == .rootContainer)
    }

    @Test("A deep item passes both its id and its parent through verbatim")
    func deepItemPassesThrough() {
        let item = makeItem(
            makeMetadata(id: "att-3", parent: "year-2", kind: .attachment),
            accountRootId: "root-1")
        #expect(item.itemIdentifier == NSFileProviderItemIdentifier(rawValue: "att-3"))
        #expect(item.parentItemIdentifier == NSFileProviderItemIdentifier(rawValue: "year-2"))
    }

    @Test("The mapping helpers round-trip the reserved root value both ways")
    func mappingHelpersRoundTrip() {
        #expect(
            ItemIdentifierMapping.providerIdentifier(
                forCoreItemId: "root-1", accountRootId: "root-1") == .rootContainer)
        #expect(
            ItemIdentifierMapping.providerIdentifier(
                forCoreItemId: "other", accountRootId: "root-1")
                == NSFileProviderItemIdentifier(rawValue: "other"))
        #expect(
            ItemIdentifierMapping.coreItemId(for: .rootContainer, accountRootId: "root-1")
                == "root-1")
        #expect(
            ItemIdentifierMapping.coreItemId(
                for: NSFileProviderItemIdentifier(rawValue: "other"), accountRootId: "root-1")
                == "other")
    }

    @Test("The filesystem name is the safe name, never the display name")
    func filenameUsesSafeName() {
        let item = makeItem(
            makeMetadata(displayName: "My Chat!/weird", safeName: "My Chat — @handle"))
        #expect(item.filename == "My Chat — @handle")
    }
}

// MARK: - Content type resolution

@Suite("File Provider item mapping — content type")
struct FileProviderItemContentTypeTests {
    @Test("A known MIME type resolves directly")
    func mimeTypeResolves() {
        let item = makeItem(
            makeMetadata(kind: .attachment, safeName: "clip", mimeType: "image/png"))
        #expect(item.contentType == .png)
    }

    @Test("An unresolvable MIME type falls back to the filename extension")
    func fallsBackToExtension() {
        let item = makeItem(
            makeMetadata(
                kind: .generatedDoc,
                safeName: "order.json",
                mimeType: "application/x-not-a-real-mime-xyz"))
        #expect(item.contentType == .json)
    }

    @Test("A missing MIME type and no extension falls back to plain data")
    func fallsBackToData() {
        let item = makeItem(
            makeMetadata(kind: .attachment, safeName: "blob", mimeType: nil))
        #expect(item.contentType == .data)
    }

    @Test("A generated JSON document with no MIME resolves by extension")
    func generatedJsonResolvesByExtension() {
        let item = makeItem(
            makeMetadata(kind: .orderDoc, safeName: "order.json", mimeType: nil))
        #expect(item.contentType == .json)
    }
}

// MARK: - Size & timestamps

@Suite("File Provider item mapping — size and timestamps")
struct FileProviderItemSizeTimeTests {
    @Test("A file's known logical size surfaces as its document size")
    func fileSizeSurfaces() {
        let item = makeItem(makeMetadata(kind: .attachment, logicalSize: 2048))
        #expect(item.documentSize == NSNumber(value: 2048))
    }

    @Test("A file without a known size has no document size")
    func fileWithoutSizeHasNone() {
        let item = makeItem(makeMetadata(kind: .attachment, logicalSize: nil))
        #expect(item.documentSize == nil)
    }

    @Test("A directory never reports a document size, even if one is present")
    func directoryHasNoSize() {
        let item = makeItem(makeMetadata(kind: .chat, logicalSize: 999))
        #expect(item.documentSize == nil)
    }

    @Test("Millisecond timestamps map to dates; absent ones map to nil")
    func timestampsMap() {
        let withTimes = makeItem(
            makeMetadata(createdAtMs: 1_600_000_000_000, modifiedAtMs: 1_600_000_500_000))
        #expect(withTimes.creationDate == Date(timeIntervalSince1970: 1_600_000_000))
        #expect(withTimes.contentModificationDate == Date(timeIntervalSince1970: 1_600_000_500))

        let withoutTimes = makeItem(makeMetadata(createdAtMs: nil, modifiedAtMs: nil))
        #expect(withoutTimes.creationDate == nil)
        #expect(withoutTimes.contentModificationDate == nil)
    }
}

// MARK: - Versioning

@Suite("File Provider item mapping — versions")
struct FileProviderItemVersionTests {
    @Test("The metadata version is the token's UTF-8 bytes")
    func metadataVersionIsTokenBytes() {
        let metadata = makeMetadata(metadataVersion: "etag:abc123")
        #expect(GramDriveFileProviderItem.metadataVersionData(for: metadata) == Data("etag:abc123".utf8))
        #expect(makeItem(metadata).itemVersion.metadataVersion == Data("etag:abc123".utf8))
    }

    @Test("A present content version is the token's UTF-8 bytes")
    func contentVersionIsTokenBytes() {
        let metadata = makeMetadata(kind: .attachment, contentVersion: "sha256:deadbeef")
        #expect(
            GramDriveFileProviderItem.contentVersionData(for: metadata)
                == Data("sha256:deadbeef".utf8))
        #expect(makeItem(metadata).itemVersion.contentVersion == Data("sha256:deadbeef".utf8))
    }

    @Test("An absent content version maps to the non-empty 0x00 sentinel")
    func absentContentVersionIsSentinel() {
        let metadata = makeMetadata(kind: .chat, contentVersion: nil)
        let component = GramDriveFileProviderItem.contentVersionData(for: metadata)
        #expect(component == Data([0x00]))
        #expect(!component.isEmpty)
    }

    @Test("The sentinel differs from the first real content version — a change the system sees")
    func sentinelDiffersFromFirstRealVersion() {
        let placeholder = GramDriveFileProviderItem.contentVersionData(
            for: makeMetadata(contentVersion: nil))
        let hydrated = GramDriveFileProviderItem.contentVersionData(
            for: makeMetadata(contentVersion: "v1"))
        #expect(placeholder != hydrated)
    }

    @Test("A 128-byte token is passed through; a longer one folds to its 32-byte SHA-256")
    func longTokenFoldsToDigest() {
        let atLimit = String(repeating: "a", count: 128)
        #expect(GramDriveFileProviderItem.versionComponent(atLimit) == Data(atLimit.utf8))
        #expect(GramDriveFileProviderItem.versionComponent(atLimit).count == 128)

        let overLimit = String(repeating: "a", count: 129)
        let folded = GramDriveFileProviderItem.versionComponent(overLimit)
        #expect(folded.count == 32)
        #expect(folded == Data(SHA256.hash(data: Data(overLimit.utf8))))
    }

    @Test("Folding preserves distinctness: distinct long tokens keep distinct components")
    func foldingPreservesDistinctness() {
        let a = GramDriveFileProviderItem.versionComponent(String(repeating: "a", count: 200))
        let b = GramDriveFileProviderItem.versionComponent(String(repeating: "b", count: 200))
        #expect(a != b)
    }

    @Test("Both version components are always non-empty and within the 128-byte ceiling")
    func versionComponentsAreValid() {
        for kind in allItemKinds {
            for contentVersion in [nil, "v1", String(repeating: "x", count: 256)] {
                let item = makeItem(
                    makeMetadata(
                        kind: kind,
                        metadataVersion: "m1",
                        contentVersion: expectedIsDirectory(kind) ? nil : contentVersion))
                let version = item.itemVersion
                #expect(!version.metadataVersion.isEmpty)
                #expect(!version.contentVersion.isEmpty)
                #expect(version.metadataVersion.count <= 128)
                #expect(version.contentVersion.count <= 128)
            }
        }
    }
}
