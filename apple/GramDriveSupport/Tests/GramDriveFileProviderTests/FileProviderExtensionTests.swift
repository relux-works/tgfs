import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport
import Testing

@testable import GramDriveFileProvider

/// The extension under test, wired to a fresh substitute container. The
/// happy path over a *seeded* container is proven cross-process by the
/// shared-state smoke (`make smoke-shared-state`, mode `domains`) —
/// durable writes are the engine's, so Swift tests cannot seed accounts
/// (DEC-006's no-writes-over-FFI rule).
private func makeExtension(
    domainIdentifier: String,
    dataRoot: URL
) -> GramDriveFileProviderExtension {
    GramDriveFileProviderExtension(
        domain: NSFileProviderDomain(
            identifier: NSFileProviderDomainIdentifier(rawValue: domainIdentifier),
            displayName: "GramDrive"
        ),
        dataRoot: { dataRoot }
    )
}

private func withSubstituteDataRoot<T>(_ body: (URL) throws -> T) rethrows -> T {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("gramdrive-fpext-tests-\(UUID().uuidString)", isDirectory: true)
    defer { try? FileManager.default.removeItem(at: root) }
    return try body(root)
}

private func withSubstituteDataRootAsync<T>(_ body: (URL) async throws -> T) async rethrows -> T {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("gramdrive-fpext-tests-\(UUID().uuidString)", isDirectory: true)
    defer { try? FileManager.default.removeItem(at: root) }
    return try await body(root)
}

@Suite("File Provider extension skeleton")
struct FileProviderExtensionTests {
    @Test("Opening a chat routes requested priority without source work")
    func openingChatRoutesRequestedPriority() {
        withSubstituteDataRoot { dataRoot in
            let signaler = RecordingHistoryPrioritySignaler()
            let ext = GramDriveFileProviderExtension(
                domain: NSFileProviderDomain(
                    identifier: NSFileProviderDomainIdentifier(rawValue: "account-7"),
                    displayName: "GramDrive"),
                dataRoot: { dataRoot },
                historyPriority: signaler)
            let chat = ItemMetadata(
                contractVersion: 2,
                id: "opaque-chat", parent: "root", kind: .chat, isDirectory: true,
                displayName: "Chat", safeName: "Chat", metadataVersion: "m1",
                mimeType: nil, logicalSize: nil, attachmentLogicalKind: nil,
                attachmentRepresentation: nil, attachmentFidelity: nil,
                attachmentSourceName: nil, attachmentExactSize: nil, contentVersion: nil,
                availability: .fetchable, createdAtMs: nil, modifiedAtMs: nil,
                deletedAtMs: nil, pin: nil, chatId: 900)

            ext.signalHistoryPriority(for: chat, accountId: 7, .requested)
            #expect(
                signaler.snapshot()
                    == [HistoryPriorityRequest(
                        accountId: 7, chatId: 900, priority: .requested)])
        }
    }

    /// Folder enumeration cannot carry the foreground signal on its own —
    /// macOS answers a read of an already-materialized directory from its own
    /// replica without calling this extension — so the fetch path is the one
    /// that has to raise it (BUG-260728-2qfzbd). Without this the extension
    /// would build a fetcher that silently signals nothing, and only an
    /// installed run would notice.
    @Test("The fetch path is wired to the same history-priority seam as enumeration")
    func fetchPathIsWiredToTheHistoryPrioritySeam() {
        withSubstituteDataRoot { dataRoot in
            let signaler = RecordingHistoryPrioritySignaler()
            let ext = GramDriveFileProviderExtension(
                domain: NSFileProviderDomain(
                    identifier: NSFileProviderDomainIdentifier(rawValue: "account-7"),
                    displayName: "GramDrive"),
                dataRoot: { dataRoot },
                historyPriority: signaler)
            #expect(ext.contentFetcher.historyPriority as? AnyObject === signaler)
        }
    }

    @Test("A foreign domain identifier is refused typed, never aliased")
    func foreignIdentifierIsRefused() {
        withSubstituteDataRoot { dataRoot in
            let ext = makeExtension(domainIdentifier: "not-an-account", dataRoot: dataRoot)
            #expect(
                throws: FileProviderExtensionError.unrecognizedDomainIdentifier("not-an-account")
            ) {
                _ = try ext.accountContext()
            }
        }
    }

    @Test("A parseable domain with no configured account reports accountNotConfigured")
    func missingAccountIsReported() {
        withSubstituteDataRoot { dataRoot in
            let ext = makeExtension(domainIdentifier: "account-7", dataRoot: dataRoot)
            #expect(throws: FileProviderExtensionError.accountNotConfigured(7)) {
                _ = try ext.accountContext()
            }
        }
    }

    @Test("The provider-role store opens and is reused until invalidate, then reopens")
    func storeLifecycle() throws {
        try withSubstituteDataRoot { dataRoot in
            let ext = makeExtension(domainIdentifier: "account-7", dataRoot: dataRoot)
            // Two resolutions fail on the missing *account*, not on the
            // store — proving open succeeded and the container exists.
            #expect(throws: FileProviderExtensionError.accountNotConfigured(7)) {
                _ = try ext.accountContext()
            }
            let layout = try SharedState.layout(dataRoot: dataRoot)
            #expect(FileManager.default.fileExists(atPath: layout.databaseFile))

            ext.invalidate()
            #expect(throws: FileProviderExtensionError.accountNotConfigured(7)) {
                _ = try ext.accountContext()
            }
        }
    }

    @Test("An unresolvable domain answers the fetch surface with noSuchItem")
    func fetchSurfaceAnswersNoSuchItem() async {
        await withSubstituteDataRootAsync { dataRoot in
            let ext = makeExtension(domainIdentifier: "account-7", dataRoot: dataRoot)
            let future = TestFuture<FetchOutcome>()
            _ = ext.fetchContentsCore(
                itemIdentifier: .rootContainer,
                requestedVersion: nil
            ) { url, item, error in
                future.fulfill(FetchOutcome(url: url, item: item, error: error))
            }
            let outcome = await future.settled
            outcome.expectProviderError(.noSuchItem)
        }
    }

    @Test("A broken data root surfaces the storage failure as-is, not as a fake noSuchItem")
    func storageFailurePassesThrough() async throws {
        try await withSubstituteDataRootAsync { dataRoot in
            // A file where the data root should be makes the open fail.
            try Data("not a directory".utf8).write(to: dataRoot)
            let ext = makeExtension(domainIdentifier: "account-7", dataRoot: dataRoot)
            let future = TestFuture<FetchOutcome>()
            _ = ext.fetchContentsCore(
                itemIdentifier: .rootContainer,
                requestedVersion: nil
            ) { url, item, error in
                future.fulfill(FetchOutcome(url: url, item: item, error: error))
            }
            let outcome = await future.settled
            #expect(outcome.url == nil)
            #expect(outcome.nsError?.domain != NSFileProviderError.errorDomain)
        }
    }

    @Test("resolveItem over a domain with no configured account throws noSuchItem")
    func resolveItemUnresolvableDomain() {
        withSubstituteDataRoot { dataRoot in
            let ext = makeExtension(domainIdentifier: "account-7", dataRoot: dataRoot)
            do {
                _ = try ext.resolveItem(for: .rootContainer)
                Issue.record("expected resolveItem to throw for a missing account")
            } catch {
                let nsError = error as NSError
                #expect(nsError.domain == NSFileProviderError.errorDomain)
                #expect(nsError.code == NSFileProviderError.Code.noSuchItem.rawValue)
            }
        }
    }

    @Test("resolveItem over a foreign domain throws noSuchItem, never an item")
    func resolveItemForeignDomain() {
        withSubstituteDataRoot { dataRoot in
            let ext = makeExtension(domainIdentifier: "not-an-account", dataRoot: dataRoot)
            do {
                _ = try ext.resolveItem(for: .rootContainer)
                Issue.record("expected resolveItem to throw for a foreign domain")
            } catch {
                let nsError = error as NSError
                #expect(nsError.domain == NSFileProviderError.errorDomain)
                #expect(nsError.code == NSFileProviderError.Code.noSuchItem.rawValue)
            }
        }
    }

    @Test("resolveItem passes a storage failure through, not a fake noSuchItem")
    func resolveItemStorageFailurePassesThrough() throws {
        try withSubstituteDataRoot { dataRoot in
            try Data("not a directory".utf8).write(to: dataRoot)
            let ext = makeExtension(domainIdentifier: "account-7", dataRoot: dataRoot)
            do {
                _ = try ext.resolveItem(for: .rootContainer)
                Issue.record("expected resolveItem to throw on a broken data root")
            } catch {
                let nsError = error as NSError
                #expect(nsError.domain != NSFileProviderError.errorDomain)
            }
        }
    }
}

@Suite("File Provider extension — the read-only write surface (DEC-007)")
struct FileProviderExtensionModifyTests {
    @Test("Purely local presentation changes are accepted, not refused")
    func locallyOwnedFieldsAreAccepted() {
        // `lastUsedDate`, `tagData` and `favoriteRank` never leave this Mac.
        // Refusing them makes the system revert the user's own local state —
        // a chat they just opened would snap back to the index-derived date
        // this extension publishes (BUG-260728-2qfzbd).
        #expect(GramDriveFileProviderExtension.isLocallyOwnedModification([.lastUsedDate]))
        #expect(GramDriveFileProviderExtension.isLocallyOwnedModification([.tagData]))
        #expect(GramDriveFileProviderExtension.isLocallyOwnedModification([.favoriteRank]))
        #expect(
            GramDriveFileProviderExtension.isLocallyOwnedModification([
                .lastUsedDate, .tagData, .favoriteRank,
            ]))
        #expect(
            GramDriveFileProviderExtension.isLocallyOwnedModification([]),
            "an empty change set has nothing to send anywhere")
    }

    @Test("Anything that would have to reach Telegram is still refused")
    func telegramBoundChangesStayRefused() {
        for field: NSFileProviderItemFields in [
            .contents, .filename, .parentItemIdentifier, .creationDate, .contentModificationDate,
            .fileSystemFlags, .extendedAttributes, .typeAndCreator,
        ] {
            #expect(
                !GramDriveFileProviderExtension.isLocallyOwnedModification(field),
                "V1 is read-only with respect to Telegram: \(field) must not be accepted")
        }
        #expect(
            !GramDriveFileProviderExtension.isLocallyOwnedModification([.lastUsedDate, .filename]),
            "one Telegram-bound field poisons an otherwise local change set")
    }

    @Test("A last-used modification completes without an error and leaves nothing pending")
    func lastUsedModificationCompletesCleanly() {
        withSubstituteDataRoot { dataRoot in
            let ext = makeExtension(domainIdentifier: "account-9", dataRoot: dataRoot)
            let item = GramDriveFileProviderItem(
                metadata: ItemMetadata(
                    contractVersion: 1,
                    id: "item-1",
                    parent: "root-9",
                    kind: .chat,
                    isDirectory: true,
                    displayName: "Chat",
                    safeName: "Chat",
                    metadataVersion: "m1",
                    mimeType: nil,
                    logicalSize: nil,
                    aggregateSize: 4096,
                    attachmentLogicalKind: nil,
                    attachmentRepresentation: nil,
                    attachmentFidelity: nil,
                    attachmentSourceName: nil,
                    attachmentExactSize: nil,
                    contentVersion: nil,
                    availability: .fetchable,
                    createdAtMs: 1_600_000_000_000,
                    modifiedAtMs: 1_600_000_500_000,
                    deletedAtMs: nil,
                    pin: nil,
                    chatId: 9),
                accountRootId: "root-9")

            // The handler runs inline: the extension decides locally and
            // performs no I/O for a purely local presentation change.
            var completions = 0
            var returnedItem: NSFileProviderItem?
            var pending: NSFileProviderItemFields = [.contents]
            var failure: Error?
            _ = ext.modifyItem(
                item,
                baseVersion: item.itemVersion,
                changedFields: [.lastUsedDate],
                contents: nil,
                request: NSFileProviderRequest()
            ) { item, fields, _, error in
                completions += 1
                returnedItem = item
                pending = fields
                failure = error
            }

            #expect(completions == 1, "the decision is made inline, with no I/O")
            #expect(failure == nil, "accepting a local-only change is not an error")
            #expect(returnedItem != nil, "the accepted item is handed back")
            #expect(pending.isEmpty, "nothing stays pending")
        }
    }
}
