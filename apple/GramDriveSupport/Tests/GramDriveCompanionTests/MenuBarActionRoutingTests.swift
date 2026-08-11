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

    @Test func manualUpdateActionTracksAvailabilityAndRoutesOnlyWhenEnabled() {
        let availability = UpdateAvailability()
        var calls = 0
        let action = ManualUpdateAction(availability: availability) { calls += 1 }

        #expect(!action.isEnabled)
        action.invoke()
        #expect(calls == 0)

        availability.setCanCheckForUpdates(true)
        #expect(action.isEnabled)
        action.invoke()
        #expect(calls == 1)
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
