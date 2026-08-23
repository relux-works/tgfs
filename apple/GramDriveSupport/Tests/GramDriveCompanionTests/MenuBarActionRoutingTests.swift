import Testing

@testable import GramDriveCompanion

@Suite @MainActor struct MenuBarActionRoutingTests {
    @Test func openGramDriveOpensTheMainWindowThenActivatesTheApp() {
        let recorder = MenuActionRecorder()
        let router = recorder.router

        router.openGramDrive()

        #expect(recorder.events == ["window:gramdrive-main", "activate"])
    }

    @Test func setUpGramDriveRestartsThenOpensAndActivatesWelcome() {
        let recorder = MenuActionRecorder()
        let router = recorder.router

        router.setUpGramDrive()

        #expect(
            recorder.events
                == ["restart-onboarding", "window:gramdrive-onboarding", "activate"])
    }

    @Test func openInFinderRoutesOnlyToTheDriveRevealer() {
        let recorder = MenuActionRecorder()
        let router = recorder.router

        router.openInFinder()

        #expect(recorder.events == ["open-in-finder"])
    }

    @Test func manualUpdateActionActivatesImmediatelyBeforeCheckingWhenEnabled() {
        let availability = UpdateAvailability()
        var events: [String] = []
        let action = ManualUpdateAction(
            availability: availability,
            activateApplication: { events.append("activate") },
            invokeUpdater: { events.append("check") })

        #expect(!action.isEnabled)
        action.invoke()
        #expect(events.isEmpty)

        availability.setCanCheckForUpdates(true)
        #expect(action.isEnabled)
        action.invoke()
        #expect(events == ["activate", "check"])
    }
}

@MainActor
private final class MenuActionRecorder {
    private(set) var events: [String] = []

    var router: CompanionActionRouter {
        CompanionActionRouter(
            mainWindowID: "gramdrive-main",
            onboardingWindowID: "gramdrive-onboarding",
            shouldPresentOnboarding: { false },
            openWindow: { self.events.append("window:\($0)") },
            activateApplication: { self.events.append("activate") },
            openDriveInFinder: { self.events.append("open-in-finder") },
            restartOnboarding: { self.events.append("restart-onboarding") })
    }
}
