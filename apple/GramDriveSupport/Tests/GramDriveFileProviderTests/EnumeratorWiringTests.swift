import FileProvider
import Foundation
import GramDriveCore
import Testing

@testable import GramDriveFileProvider

/// `enumerator(for:request:)` wiring over a real (fresh, unseeded) store.
/// The happy path over a *seeded* container is the shared-state smoke's to
/// prove (`make smoke-shared-state`) — durable writes are the engine's, so
/// Swift tests cannot seed accounts (DEC-006); the enumerator's own
/// behavior over a populated tree is pinned against the scripted store in
/// `GramDriveEnumeratorTests`.
@Suite("Extension enumerator wiring")
struct EnumeratorWiringTests {
    private func withExtension<T>(
        domainIdentifier: String,
        _ body: (GramDriveFileProviderExtension) throws -> T
    ) rethrows -> T {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent(
                "gramdrive-enumerator-wiring-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let ext = GramDriveFileProviderExtension(
            domain: NSFileProviderDomain(
                identifier: NSFileProviderDomainIdentifier(rawValue: domainIdentifier),
                displayName: "GramDrive"
            ),
            dataRoot: { root }
        )
        return try body(ext)
    }

    @Test("A foreign domain answers noSuchItem for any container")
    func foreignDomain() {
        withExtension(domainIdentifier: "not-an-account") { ext in
            for container in [NSFileProviderItemIdentifier.workingSet, .rootContainer] {
                #expect(throws: NSFileProviderError(.noSuchItem)) {
                    _ = try ext.makeEnumerator(for: container)
                }
            }
        }
    }

    @Test("A parseable domain with no configured account answers noSuchItem")
    func unconfiguredAccount() {
        _ = withExtension(domainIdentifier: "account-7") { ext in
            #expect(throws: NSFileProviderError(.noSuchItem)) {
                _ = try ext.makeEnumerator(for: .workingSet)
            }
        }
    }
}
