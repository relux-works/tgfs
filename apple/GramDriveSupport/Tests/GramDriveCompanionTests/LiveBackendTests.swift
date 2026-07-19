import Foundation
import GramDriveAgentCore
import Testing

@testable import GramDriveCompanion

/// A unique scratch directory, removed after the test.
private func withTempRoot<T>(_ body: (URL) throws -> T) throws -> T {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent("gramdrive-companion-tests-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: url) }
    return try body(url)
}

/// Async variant of ``withTempRoot(_:)``.
private func withTempRootAsync<T>(_ body: (URL) async throws -> T) async throws -> T {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent("gramdrive-companion-tests-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: url) }
    return try await body(url)
}

@Suite struct LiveCompanionBackendTests {
    @Test func healthReadsNotRunningWhenNoAgentListens() async throws {
        try await withTempRootAsync { root in
            let backend = LiveCompanionBackend(
                layout: AgentRuntimeLayout(dataRoot: root), healthTimeout: .seconds(1))
            let result = await backend.fetchHealth()
            #expect(result == .notRunning)
        }
    }

    @Test func settingsMissingFileIsDefaults() throws {
        try withTempRoot { root in
            let backend = LiveCompanionBackend(layout: AgentRuntimeLayout(dataRoot: root))
            let loaded = try backend.loadSettings()
            #expect(loaded == AgentSettings())
            #expect(loaded.cacheQuotaBytes == AgentSettings.defaultCacheQuotaBytes)
        }
    }

    @Test func settingsRoundTripThroughTheDurableDocument() throws {
        try withTempRoot { root in
            let backend = LiveCompanionBackend(layout: AgentRuntimeLayout(dataRoot: root))
            let saved = AgentSettings(
                launchAtLogin: true, cacheQuotaBytes: 42_000_000_000, archiveModeEnabled: true)
            try backend.saveSettings(saved)
            #expect(try backend.loadSettings() == saved)
        }
    }

    @Test func commandsReportControlChannelNotWired() async {
        let backend = LiveCompanionBackend(layout: AgentRuntimeLayout(dataRoot: URL(fileURLWithPath: "/tmp/x")))
        #expect(await backend.requestRepair() == .unavailable(.notWired))
        let confirmation = RemovalConfirmation(
            accountLabel: "A", typedConfirmation: "A", acknowledgedIrreversible: true)
        #expect(await backend.removeAccount(confirmation) == .unavailable(.notWired))
        let start = await backend.makeAuthorizationSession().start()
        #expect(start == .unavailable(.notWired))
    }
}

@Suite struct AgentSettingsCompatibilityTests {
    private func decode(_ json: String) throws -> AgentSettings {
        try JSONDecoder().decode(AgentSettings.self, from: Data(json.utf8))
    }

    @Test func anEmptyObjectDecodesToAllDefaults() throws {
        let settings = try decode("{}")
        #expect(settings == AgentSettings())
        #expect(settings.cacheQuotaBytes == AgentSettings.defaultCacheQuotaBytes)
        #expect(settings.archiveModeEnabled == false)
    }

    @Test func anOlderDocumentWithOnlyLaunchAtLoginStillDecodes() throws {
        // A settings.json written before the POL-2 fields existed.
        let settings = try decode(#"{"launchAtLogin": true}"#)
        #expect(settings.launchAtLogin == true)
        #expect(settings.cacheQuotaBytes == AgentSettings.defaultCacheQuotaBytes)
        #expect(settings.archiveModeEnabled == false)
    }

    @Test func aFullDocumentDecodesEveryField() throws {
        let settings = try decode(
            #"{"launchAtLogin": true, "cacheQuotaBytes": 12345, "archiveModeEnabled": true}"#)
        #expect(settings == AgentSettings(
            launchAtLogin: true, cacheQuotaBytes: 12345, archiveModeEnabled: true))
    }

    @Test func encodeThenDecodeIsStable() throws {
        let original = AgentSettings(
            launchAtLogin: true, cacheQuotaBytes: 7_000_000_000, archiveModeEnabled: true)
        let data = try JSONEncoder().encode(original)
        #expect(try JSONDecoder().decode(AgentSettings.self, from: data) == original)
    }
}
