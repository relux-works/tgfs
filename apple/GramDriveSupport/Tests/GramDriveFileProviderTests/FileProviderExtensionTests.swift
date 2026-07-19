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
