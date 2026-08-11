import Foundation
import Testing

@testable import GramDriveCompanion

@Suite struct OnboardingCompletionStoreTests {
    @Test func inMemoryStoreRoundTrips() {
        let store = InMemoryOnboardingCompletionStore(completed: false)
        #expect(!store.hasCompletedOnboarding())
        store.setCompletedOnboarding(true)
        #expect(store.hasCompletedOnboarding())
        store.setCompletedOnboarding(false)
        #expect(!store.hasCompletedOnboarding())
    }

    @Test func userDefaultsStoreDefaultsToNotCompleted() {
        let suite = "gramdrive.onboarding.tests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        defer { defaults.removePersistentDomain(forName: suite) }
        let store = UserDefaultsOnboardingCompletionStore(defaults: defaults, key: "completed")
        #expect(!store.hasCompletedOnboarding())
        store.setCompletedOnboarding(true)
        #expect(store.hasCompletedOnboarding())
        #expect(defaults.bool(forKey: "completed"))
    }
}

@Suite struct DriveLocationTests {
    /// A throwaway directory tree for one test; removed on teardown.
    private final class TempTree {
        let root: URL
        init() {
            root = FileManager.default.temporaryDirectory
                .appendingPathComponent("gramdrive-drive-\(UUID().uuidString)", isDirectory: true)
            try? FileManager.default.createDirectory(
                at: root, withIntermediateDirectories: true)
        }
        func makeChild(_ name: String) {
            try? FileManager.default.createDirectory(
                at: root.appendingPathComponent(name, isDirectory: true),
                withIntermediateDirectories: true)
        }
        func cleanup() { try? FileManager.default.removeItem(at: root) }
    }

    @Test func resolvesTheGramDriveProviderFolderWhenPresent() {
        let tree = TempTree()
        defer { tree.cleanup() }
        tree.makeChild("Dropbox")
        tree.makeChild("GramDrive")
        let location = CloudStorageDriveLocation(
            baseDirectory: tree.root, revealer: { _ in true })
        #expect(location.resolveDriveURL()?.lastPathComponent == "GramDrive")
    }

    @Test func picksTheFirstGramDriveFolderDeterministically() {
        let tree = TempTree()
        defer { tree.cleanup() }
        tree.makeChild("GramDrive — Bob")
        tree.makeChild("GramDrive — Alice")
        let location = CloudStorageDriveLocation(
            baseDirectory: tree.root, revealer: { _ in true })
        #expect(location.resolveDriveURL()?.lastPathComponent == "GramDrive — Alice")
    }

    @Test func doesNotTreatTheContainerAsARegisteredDrive() {
        let tree = TempTree()
        defer { tree.cleanup() }
        let location = CloudStorageDriveLocation(
            baseDirectory: tree.root, revealer: { _ in true })
        #expect(location.resolveDriveURL() == nil)
    }

    @Test func resolvesNilWhenTheContainerIsMissing() {
        let missing = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-absent-\(UUID().uuidString)", isDirectory: true)
        let location = CloudStorageDriveLocation(
            baseDirectory: missing, revealer: { _ in true })
        #expect(location.resolveDriveURL() == nil)
    }

    @Test func revealSendsTheResolvedURLToTheRevealer() {
        let tree = TempTree()
        defer { tree.cleanup() }
        tree.makeChild("GramDrive")
        let box = RevealBox()
        let location = CloudStorageDriveLocation(
            baseDirectory: tree.root, revealer: { url in box.record(url) })
        #expect(location.reveal())
        #expect(box.revealed?.lastPathComponent == "GramDrive")
    }

    @Test func revealReportsFailureWhenNothingResolves() {
        let missing = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-absent-\(UUID().uuidString)", isDirectory: true)
        let box = RevealBox()
        let location = CloudStorageDriveLocation(
            baseDirectory: missing, revealer: { url in box.record(url) })
        #expect(!location.reveal())
        #expect(box.revealed == nil)  // never called when there is nothing to show
    }

    @Test func fixedLocationCountsReveals() {
        let url = URL(fileURLWithPath: "/tmp/GramDrive")
        let location = FixedDriveLocation(url: url)
        #expect(location.resolveDriveURL() == url)
        #expect(location.reveal())
        #expect(location.reveal())
        #expect(location.revealCount == 2)
    }
}

/// Captures the URL passed to an injected revealer, across the `@Sendable`
/// boundary the closure requires.
private final class RevealBox: @unchecked Sendable {
    private let lock = NSLock()
    private var _revealed: URL?

    var revealed: URL? {
        lock.lock()
        defer { lock.unlock() }
        return _revealed
    }

    @discardableResult
    func record(_ url: URL) -> Bool {
        lock.lock()
        _revealed = url
        lock.unlock()
        return true
    }
}
