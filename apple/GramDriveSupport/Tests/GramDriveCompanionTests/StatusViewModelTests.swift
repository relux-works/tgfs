import Foundation
import GramDriveAgentCore
import GramDriveCore
import GramDriveSupport
import SQLite3
import Testing

@testable import GramDriveCompanion

private struct AuthRequiredNamespaceBootstrapper: AgentNamespaceBootstrapping {
    func start(
        accountId _: Int64,
        onProgress _: @escaping @Sendable (AgentNamespaceProgress) -> Void
    ) throws -> any AgentNamespaceSessionHosting {
        throw DriveError.AuthRequired(detail: "private diagnostic")
    }
}

private func seedAuthorizedCompanionAccount(dataRoot: URL, accountId: Int64) throws {
    let layout = try sharedStateLayout(dataRoot: dataRoot.path)
    var database: OpaquePointer?
    guard sqlite3_open_v2(
        layout.databaseFile, &database, SQLITE_OPEN_READWRITE, nil) == SQLITE_OK,
        let database
    else {
        throw CocoaError(.fileReadUnknown)
    }
    defer { sqlite3_close(database) }

    let sql = """
        INSERT INTO accounts (
            account_id, source_kind, display_name, auth_state, namespace_version,
            retention_mode, archive_mode, created_at_ms, updated_at_ms, display_timezone
        ) VALUES (?, 'local_tdlib', 'Private', 'authorized', 0, 'mirror', 0, 1, 1, 'UTC')
        """
    var statement: OpaquePointer?
    guard sqlite3_prepare_v2(database, sql, -1, &statement, nil) == SQLITE_OK,
          let statement
    else {
        throw CocoaError(.fileReadCorruptFile)
    }
    defer { sqlite3_finalize(statement) }
    guard sqlite3_bind_int64(statement, 1, accountId) == SQLITE_OK,
          sqlite3_step(statement) == SQLITE_DONE
    else {
        throw CocoaError(.fileWriteUnknown)
    }
}

@Suite struct StatusDerivationTests {
    @Test func agentPresenceMapsEveryReadout() {
        #expect(
            CompanionStatusViewModel.agentPresence(from: .running(previewSnapshot(state: .draining)))
                == .running(.draining))
        #expect(CompanionStatusViewModel.agentPresence(from: .notRunning) == .notRunning)
        #expect(
            CompanionStatusViewModel.agentPresence(from: .timedOut) == .unreachable("timed out"))
        #expect(
            CompanionStatusViewModel.agentPresence(from: .error("boom"))
                == .unreachable("boom"))
    }

    @Test func accountStatusIsUnknownWhileRunningAndUnavailableOtherwise() {
        #expect(CompanionStatusViewModel.accountStatus(from: .running(previewSnapshot())) == .unknown)
        #expect(CompanionStatusViewModel.accountStatus(from: .notRunning) == .agentUnavailable)
        #expect(CompanionStatusViewModel.accountStatus(from: .timedOut) == .agentUnavailable)
    }

    @Test func accountStatusProjectsDurableAndLiveAuthorizationWithoutIdentityData() {
        let authorized = AccountHealthSummary(
            accountId: 7,
            displayName: "Private",
            authState: "authorized",
            observedAuthorization: .authorized)
        #expect(
            CompanionStatusViewModel.accountStatus(
                from: .running(previewSnapshot(accounts: [authorized]))) == .authorized)
        let terminallySignedOut = AccountHealthSummary(
            accountId: 7,
            displayName: "Private",
            authState: "authorized",
            observedAuthorization: .authorizationRequired)
        #expect(
            CompanionStatusViewModel.accountStatus(
                from: .running(previewSnapshot(accounts: [terminallySignedOut])))
                == .authorizationRequired)
        #expect(AccountStatus.authorizationRequired.label == "Authorization Required")
        let liveProbeUnavailable = AccountHealthSummary(
            accountId: 7,
            displayName: "Private",
            authState: "authorized",
            observedAuthorization: .unavailable)
        #expect(
            CompanionStatusViewModel.accountStatus(
                from: .running(previewSnapshot(accounts: [liveProbeUnavailable]))) == .authorized)
        #expect(
            CompanionStatusViewModel.accountStatus(
                from: .running(previewSnapshot(accounts: []))) == .notConfigured)
        let signedOut = AccountHealthSummary(
            accountId: 7, displayName: "Private", authState: "signed_out")
        #expect(
            CompanionStatusViewModel.accountStatus(
                from: .running(previewSnapshot(accounts: [signedOut])))
                == .authorizationRequired)
    }

    @Test func retryableSynchronousAuthRequiredOverridesDurableAuthorizedStatus() async throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(
            "gramdrive-companion-auth-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        let lifecycle = AgentLifecycle(
            configuration: AgentConfiguration(
                dataRoot: root,
                namespaceBootstrapper: AuthRequiredNamespaceBootstrapper()))
        try lifecycle.start()
        try seedAuthorizedCompanionAccount(dataRoot: root, accountId: 9)
        lifecycle.restartNamespaces()

        let snapshot = try AgentHealthClient.fetch(
            socketURL: lifecycle.runtimeLayout.healthSocket)
        let account = try #require(snapshot.accounts?.first)
        #expect(account.authState == "authorized")
        #expect(account.observedAuthorization == .authorizationRequired)
        #expect(
            CompanionStatusViewModel.accountStatus(from: .running(snapshot))
                == .authorizationRequired)

        await lifecycle.shutdown(reason: .terminate)
    }

    @Test func providerStatusProjectsTheRegistrationField() {
        #expect(
            CompanionStatusViewModel.providerStatus(
                from: .running(previewSnapshot(providerRegistrationState: "registered")))
                == .registered)
        #expect(
            CompanionStatusViewModel.providerStatus(
                from: .running(previewSnapshot(providerRegistrationState: "notRegistered")))
                == .notRegistered)
        #expect(
            CompanionStatusViewModel.providerStatus(
                from: .running(previewSnapshot(providerRegistrationState: "weird")))
                == .other("weird"))
        // nil today — honest unknown, not a fabricated registration.
        #expect(
            CompanionStatusViewModel.providerStatus(from: .running(previewSnapshot())) == .unknown)
        #expect(CompanionStatusViewModel.providerStatus(from: .notRunning) == .unknown)
    }

    @Test func diagnosticsExistOnlyWhenRunning() {
        #expect(CompanionStatusViewModel.diagnostics(from: .running(previewSnapshot())) != nil)
        #expect(CompanionStatusViewModel.diagnostics(from: .notRunning) == nil)
        #expect(CompanionStatusViewModel.diagnostics(from: .timedOut) == nil)
    }

    @Test func diagnosticsReportProjectsSnapshotFieldsHonestly() {
        let report = DiagnosticsReport(snapshot: previewSnapshot())
        #expect(report.contractVersion == "0.2.0")
        #expect(report.pid == 4242)
        #expect(report.runState == .running)
        #expect(report.launchAtLogin == true)
        #expect(report.dataVersion == 17)
        #expect(report.pendingTransferCount == 0)
        // Unwired engine fields stay nil — "not reported yet", not zero.
        #expect(report.lastSourceUpdate == nil)
        #expect(report.changeCursor == nil)
        #expect(report.cachePressure == nil)
        #expect(report.lastWake != nil)
    }
}

@MainActor
@Suite struct StatusRefreshTests {
    @Test func refreshPullsFromTheBackend() async {
        let backend = InMemoryCompanionBackend(health: .running(previewSnapshot()))
        let model = CompanionStatusViewModel(backend: backend)
        #expect(model.agentPresence == .notRunning)  // default before refresh
        await model.refresh()
        #expect(model.agentPresence == .running(.running))
        #expect(model.diagnostics?.pid == 4242)
    }

    @Test func appOwnedProviderResultOverridesAnUnwiredHealthField() {
        let backend = InMemoryCompanionBackend(health: .running(previewSnapshot()))
        let model = CompanionStatusViewModel(backend: backend)
        model.reportProviderStatus(.registered)
        #expect(model.providerStatus == .registered)
    }
}
