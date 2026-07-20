import Foundation
import GramDriveAgentCore
import Testing

@testable import GramDriveCompanion

/// Builds an onboarding view model over an in-memory backend, with injectable
/// drive-location and completion stores.
@MainActor
private func makeOnboarding(
    backend: InMemoryCompanionBackend = InMemoryCompanionBackend(),
    driveLocation: any DriveLocationProviding = FixedDriveLocation(url: nil),
    store: InMemoryOnboardingCompletionStore = InMemoryOnboardingCompletionStore(),
    available: UInt64? = 500_000_000_000
) -> OnboardingViewModel {
    OnboardingViewModel(
        authorization: AuthorizationViewModel(backend: backend),
        settings: CompanionSettingsViewModel(
            backend: backend, diskProbe: FixedDiskSpaceProbe(available: available)),
        status: CompanionStatusViewModel(backend: backend),
        driveLocation: driveLocation,
        completionStore: store)
}

/// Drives the shared sign-in flow to `.ready` so gated steps unblock.
@MainActor
private func authorize(
    _ model: OnboardingViewModel, session: ScriptedAuthorizationSession
) async {
    await model.authorization.begin()
    session.emit(.ready)
    session.finish()
    await model.authorization.waitForCompletion()
}

@MainActor
@Suite struct OnboardingPresentationTests {
    @Test func firstLaunchPresentsOnCleanMachine() {
        let model = makeOnboarding(store: InMemoryOnboardingCompletionStore(completed: false))
        #expect(model.isPresented)
        #expect(model.step == .welcome)
    }

    @Test func aReturningUserIsNotOnboardedAgain() {
        let model = makeOnboarding(store: InMemoryOnboardingCompletionStore(completed: true))
        #expect(!model.isPresented)
    }

    @Test func finishRecordsCompletionAndDismisses() {
        let store = InMemoryOnboardingCompletionStore(completed: false)
        let model = makeOnboarding(store: store)
        model.finish()
        #expect(store.hasCompletedOnboarding())
        #expect(!model.isPresented)
    }

    @Test func skipRecordsCompletionAndDismisses() {
        let store = InMemoryOnboardingCompletionStore(completed: false)
        let model = makeOnboarding(store: store)
        model.skip()
        #expect(store.hasCompletedOnboarding())
        #expect(!model.isPresented)
    }

    @Test func restartReopensAtWelcome() {
        let store = InMemoryOnboardingCompletionStore(completed: true)
        let model = makeOnboarding(store: store)
        #expect(!model.isPresented)
        model.restart()
        #expect(model.isPresented)
        #expect(model.step == .welcome)
    }
}

@MainActor
@Suite struct OnboardingNavigationTests {
    @Test func welcomeAdvancesToSignIn() {
        let model = makeOnboarding()
        #expect(model.canAdvance)  // welcome never gates
        model.advance()
        #expect(model.step == .signIn)
    }

    @Test func signInGatesUntilAuthorized() async {
        let session = ScriptedAuthorizationSession()
        let backend = InMemoryCompanionBackend(session: { session })
        let model = makeOnboarding(backend: backend)
        model.advance()  // → signIn
        #expect(model.step == .signIn)
        #expect(!model.canAdvance)  // not authorized yet
        model.advance()  // gated no-op
        #expect(model.step == .signIn)

        await authorize(model, session: session)
        #expect(model.authorization.isAuthorized)
        #expect(model.canAdvance)
        model.advance()
        #expect(model.step == .defaults)
    }

    @Test func enteringDefaultsLoadsPersistedSettings() async {
        let session = ScriptedAuthorizationSession()
        let backend = InMemoryCompanionBackend(
            settings: AgentSettings(
                launchAtLogin: false, cacheQuotaBytes: 42_000_000_000, archiveModeEnabled: true),
            session: { session })
        let model = makeOnboarding(backend: backend)
        model.advance()  // welcome → signIn
        await authorize(model, session: session)
        model.advance()  // signIn → defaults
        #expect(model.step == .defaults)
        #expect(model.settings.cacheQuotaBytes == 42_000_000_000)
        #expect(model.settings.archiveModeEnabled == true)
    }

    @Test func advancingThroughDefaultsPersistsChosenValues() async {
        let session = ScriptedAuthorizationSession()
        let backend = InMemoryCompanionBackend(session: { session })
        let model = makeOnboarding(backend: backend)
        model.advance()  // → signIn
        await authorize(model, session: session)
        model.advance()  // → defaults
        model.settings.cacheQuotaBytes = 30_000_000_000
        model.settings.archiveModeEnabled = false
        model.advance()  // → success, saves
        #expect(model.step == .success)
        #expect(backend.storedSettings.cacheQuotaBytes == 30_000_000_000)
        #expect(backend.storedSettings.archiveModeEnabled == false)
    }

    @Test func backStepsBackwards() {
        let model = makeOnboarding()
        model.advance()  // → signIn
        #expect(!model.isFirstStep)
        model.back()
        #expect(model.step == .welcome)
        #expect(model.isFirstStep)
        model.back()  // no-op at the first step
        #expect(model.step == .welcome)
    }

    @Test func successIsTheLastStep() async {
        let session = ScriptedAuthorizationSession()
        let backend = InMemoryCompanionBackend(session: { session })
        let store = InMemoryOnboardingCompletionStore(completed: false)
        let model = makeOnboarding(backend: backend, store: store)
        model.advance()  // → signIn
        await authorize(model, session: session)
        model.advance()  // → defaults
        model.advance()  // → success
        #expect(model.isLastStep)
        #expect(model.primaryActionTitle == "Done")
        model.advance()  // Done → finish
        #expect(store.hasCompletedOnboarding())
        #expect(!model.isPresented)
    }
}

@MainActor
@Suite struct OnboardingSignInStartTests {
    @Test func beginSignInStartsFromIdle() async {
        let session = ScriptedAuthorizationSession()
        let backend = InMemoryCompanionBackend(session: { session })
        let model = makeOnboarding(backend: backend)
        session.emit(.waitPhoneNumber)
        await model.beginSignInIfNeeded()
        session.finish()
        await model.authorization.waitForCompletion()
        #expect(model.authorization.state == .waitPhoneNumber)
    }

    @Test func beginSignInDoesNotRestartAnUnavailableChannel() async {
        let backend = InMemoryCompanionBackend(
            session: { UnavailableAuthorizationSession(reason: .notWired) })
        let model = makeOnboarding(backend: backend)
        await model.authorization.begin()
        #expect(model.authorization.unavailable == .notWired)
        // A second call must not re-begin over the honest unavailable state.
        await model.beginSignInIfNeeded()
        #expect(model.authorization.unavailable == .notWired)
    }
}

@Suite struct InitialSyncDerivationTests {
    @Test func waitingForAgentWhenNotRunning() {
        #expect(OnboardingViewModel.initialSync(from: .notRunning) == .waitingForAgent)
        #expect(OnboardingViewModel.initialSync(from: .timedOut) == .waitingForAgent)
        #expect(OnboardingViewModel.initialSync(from: .error("x")) == .waitingForAgent)
    }

    @Test func preparingWhenRunningButNoSourceUpdateYet() {
        let snapshot = previewSnapshot()  // lastSourceUpdateMs nil, pending 0
        #expect(OnboardingViewModel.initialSync(from: .running(snapshot)) == .preparing)
    }

    @Test func syncingWhenTransfersPending() {
        let snapshot = AgentHealthSnapshot(
            payloadVersion: 1, agentVersion: AgentVersion.current, contractVersion: "0.2.0",
            pid: 1, state: .running, startedAtMs: 0, launchAtLogin: nil, stateSchemaVersion: nil,
            dataVersion: nil, pendingTransferCount: 3, lastSourceUpdateMs: 10, changeCursor: nil,
            cachePressure: nil, providerRegistrationState: nil, lastSleepMs: nil, lastWakeMs: nil,
            recentEvents: [])
        #expect(OnboardingViewModel.initialSync(from: .running(snapshot)) == .syncing(pending: 3))
    }

    @Test func upToDateWhenSyncedAndNothingPending() {
        let snapshot = AgentHealthSnapshot(
            payloadVersion: 1, agentVersion: AgentVersion.current, contractVersion: "0.2.0",
            pid: 1, state: .running, startedAtMs: 0, launchAtLogin: nil, stateSchemaVersion: nil,
            dataVersion: nil, pendingTransferCount: 0, lastSourceUpdateMs: 10, changeCursor: nil,
            cachePressure: nil, providerRegistrationState: nil, lastSleepMs: nil, lastWakeMs: nil,
            recentEvents: [])
        #expect(OnboardingViewModel.initialSync(from: .running(snapshot)) == .upToDate)
    }

    @Test func onlyActiveStatesSpin() {
        #expect(InitialSyncStatus.waitingForAgent.isActive)
        #expect(InitialSyncStatus.preparing.isActive)
        #expect(InitialSyncStatus.syncing(pending: 1).isActive)
        #expect(!InitialSyncStatus.upToDate.isActive)
    }
}

@MainActor
@Suite struct OnboardingDriveLocationTests {
    @Test func openInFinderRevealsThroughTheSeam() {
        let url = URL(fileURLWithPath: "/Users/x/Library/CloudStorage/GramDrive")
        let location = FixedDriveLocation(url: url)
        let model = makeOnboarding(driveLocation: location)
        #expect(model.driveURL == url)
        #expect(model.openDriveInFinder())
        #expect(location.revealCount == 1)
    }

    @Test func openInFinderReportsWhenNoLocationYet() {
        let location = FixedDriveLocation(url: nil)
        let model = makeOnboarding(driveLocation: location)
        #expect(model.driveURL == nil)
        #expect(!model.openDriveInFinder())
        #expect(location.revealCount == 1)
    }
}
