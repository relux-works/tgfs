import Foundation
import GramDriveCore
import Testing

@testable import GramDriveSupport

/// A unique substitute container directory, removed after the test.
private func withTemporaryContainer<T>(_ body: (URL) throws -> T) throws -> T {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent("gramdrive-support-tests-\(ProcessInfo.processInfo.processIdentifier)-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: url) }
    return try body(url)
}

@Suite struct AppGroupTests {
    @Test func identifierIsTheTeamPrefixedGroupFromTheIdentifierPlan() {
        // DEC-019 / platform-requirements: the entitlement form v1 ships.
        #expect(AppGroup.identifier == "262RZ595FP.com.reluxworks.gramdrive")
    }

    @Test func dataRootIsTheFixedSubdirectoryOfTheContainer() {
        let container = URL(fileURLWithPath: "/tmp/container")
        let dataRoot = AppGroup.dataRootURL(containerURL: container)
        #expect(dataRoot.path == "/tmp/container/Library/Application Support/GramDrive")
    }

    @Test func layoutBelowTheDataRootComesFromTheCore() throws {
        let dataRoot = URL(fileURLWithPath: "/tmp/container/data")
        let layout = try SharedState.layout(dataRoot: dataRoot)
        #expect(layout.stateDir == "/tmp/container/data/state")
        #expect(layout.databaseFile == "/tmp/container/data/state/gramdrive.sqlite3")
        #expect(layout.quarantineDir == "/tmp/container/data/state/quarantine")
        #expect(layout.cacheDir == "/tmp/container/data/cache")
    }
}

@Suite struct SharedStateTests {
    @Test func openCreatesTheLayoutAndAnswersReadsWithAbsence() throws {
        try withTemporaryContainer { container in
            let dataRoot = AppGroup.dataRootURL(containerURL: container)
            let store = try SharedState.open(dataRoot: dataRoot, role: .provider)
            #expect(store.role() == .provider)
            let layout = store.layout()
            #expect(FileManager.default.fileExists(atPath: layout.databaseFile))
            #expect(FileManager.default.fileExists(atPath: layout.cacheDir))
            #expect(try store.schemaVersion() > 0)
            // An empty database answers with absence, not errors; a bogus
            // identifier is an InvalidArgument error.
            #expect(throws: DriveError.self) {
                _ = try store.item(id: "not-an-item-id")
            }
        }
    }

    @Test func twoHandlesOverOneContainerAgree() throws {
        try withTemporaryContainer { container in
            let dataRoot = AppGroup.dataRootURL(containerURL: container)
            let first = try SharedState.open(dataRoot: dataRoot, role: .coordinator)
            let second = try SharedState.open(dataRoot: dataRoot, role: .provider)
            #expect(try first.schemaVersion() == (try second.schemaVersion()))
            #expect(first.layout() == second.layout())
        }
    }

    @Test func providerRoleMayNotQuarantine() throws {
        try withTemporaryContainer { container in
            let dataRoot = AppGroup.dataRootURL(containerURL: container)
            #expect(throws: DriveError.self) {
                _ = try quarantineCorruptState(dataRoot: dataRoot.path, role: .provider)
            }
        }
    }

    @Test func coordinatorRecoversACorruptDatabase() throws {
        try withTemporaryContainer { container in
            let dataRoot = AppGroup.dataRootURL(containerURL: container)
            let layout = try SharedState.layout(dataRoot: dataRoot)
            try FileManager.default.createDirectory(
                atPath: layout.stateDir, withIntermediateDirectories: true)
            try Data("garbage, not a database".utf8)
                .write(to: URL(fileURLWithPath: layout.databaseFile))

            // A corrupt file refuses to open...
            #expect(throws: DriveError.self) {
                _ = try SharedState.open(dataRoot: dataRoot, role: .provider)
            }
            // ...a healthy-or-missing quarantine is a no-op by contract,
            // and a corrupt one moves the file and reports where.
            let quarantined = try quarantineCorruptState(
                dataRoot: dataRoot.path, role: .coordinator)
            let quarantineDir = try #require(quarantined)
            #expect(quarantineDir.hasPrefix(layout.quarantineDir))
            #expect(!FileManager.default.fileExists(atPath: layout.databaseFile))

            // The cleared path opens fresh.
            let store = try SharedState.open(dataRoot: dataRoot, role: .coordinator)
            #expect(try store.schemaVersion() > 0)
        }
    }

    @Test func dataVersionIsStableWithoutForeignCommits() throws {
        try withTemporaryContainer { container in
            let dataRoot = AppGroup.dataRootURL(containerURL: container)
            let store = try SharedState.open(dataRoot: dataRoot, role: .provider)
            let first = try store.dataVersion()
            #expect(try store.dataVersion() == first)
        }
    }
}

@Suite struct ChangeSignalTests {
    @Test func nameCarriesTheAppGroupPrefix() {
        #expect(ChangeSignal.name.hasPrefix(AppGroup.identifier))
    }

    // Darwin notification names are host-global, so these tests ring
    // uniquely named doorbells through the internal seam — observing the
    // product name would hear other tests (and other processes).

    @Test func aPostedSignalReachesAnObserver() async throws {
        let name = "\(ChangeSignal.name).test-\(UUID().uuidString)"
        try await confirmation("doorbell rings") { rang in
            let observation = try ChangeSignal.observe(name: name) { rang() }
            defer { observation.cancel() }
            ChangeSignal.post(name: name)
            // Darwin notifications are asynchronous; give delivery a
            // moment without polling the handler.
            try await Task.sleep(for: .milliseconds(500))
        }
    }

    @Test func aCancelledObservationStopsDelivery() async throws {
        let name = "\(ChangeSignal.name).test-\(UUID().uuidString)"
        try await confirmation("no ring after cancel", expectedCount: 0) { rang in
            let observation = try ChangeSignal.observe(name: name) { rang() }
            observation.cancel()
            ChangeSignal.post(name: name)
            try await Task.sleep(for: .milliseconds(300))
        }
    }
}
