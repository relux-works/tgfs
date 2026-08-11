import Foundation
import GramDriveAgentCore
import Testing

@testable import GramDriveCompanion

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

    @Test func accountStatusProjectsDurableAuthorizationWithoutIdentityData() {
        let authorized = AccountHealthSummary(
            accountId: 7, displayName: "Private", authState: "authorized")
        #expect(
            CompanionStatusViewModel.accountStatus(
                from: .running(previewSnapshot(accounts: [authorized]))) == .authorized)
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
