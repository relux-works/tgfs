import Testing

@testable import GramDriveCompanion

@Suite @MainActor struct WindowPresentationRoutingTests {
    @Test func coldLaunchPresentsMainForAReturningUser() {
        let harness = WindowPresentationHarness(onboarding: false)

        harness.lifecycle.applicationDidFinishLaunching()
        let presented = harness.consumePendingRequest()

        #expect(presented)
        #expect(harness.events == ["window:gramdrive-main", "activate"])
        #expect(harness.visibleWindowIDs == ["gramdrive-main"])
    }

    @Test func coldLaunchPresentsOnboardingWhenSetupIsIncomplete() {
        let harness = WindowPresentationHarness(onboarding: true)

        harness.lifecycle.applicationDidFinishLaunching()
        harness.consumePendingRequest()

        #expect(harness.events == ["window:gramdrive-onboarding", "activate"])
        #expect(harness.visibleWindowIDs == ["gramdrive-onboarding"])
    }

    @Test func reopenFromZeroWindowsPresentsAndActivatesAgain() {
        let harness = WindowPresentationHarness(onboarding: false)
        harness.lifecycle.applicationDidFinishLaunching()
        harness.consumePendingRequest()
        harness.closeAllWindows()
        harness.events.removeAll()

        let handled = harness.lifecycle.applicationShouldHandleReopen()
        let presented = harness.consumePendingRequest()

        #expect(handled)
        #expect(presented)
        #expect(harness.events == ["window:gramdrive-main", "activate"])
        #expect(harness.visibleWindowIDs == ["gramdrive-main"])
    }

    @Test func reopenReusesAnExistingSingletonWindowWithoutDuplicates() {
        let harness = WindowPresentationHarness(onboarding: false)
        harness.lifecycle.applicationDidFinishLaunching()
        harness.consumePendingRequest()

        harness.lifecycle.applicationShouldHandleReopen()
        harness.consumePendingRequest()

        #expect(harness.windowOpenRequests == ["gramdrive-main", "gramdrive-main"])
        #expect(harness.visibleWindowIDs == ["gramdrive-main"])
        #expect(harness.activationCount == 2)
    }

    @Test func observingOneGenerationTwiceDoesNotDuplicatePresentation() {
        let harness = WindowPresentationHarness(onboarding: false)
        harness.lifecycle.applicationDidFinishLaunching()

        let first = harness.consumePendingRequest()
        let second = harness.consumePendingRequest()

        #expect(first)
        #expect(!second)
        #expect(harness.windowOpenRequests == ["gramdrive-main"])
        #expect(harness.visibleWindowIDs == ["gramdrive-main"])
    }

    @Test func activationRetriesStopAfterPresentationSucceedsAndRespectLaterFocus() {
        let harness = WindowActivationRetryHarness()

        harness.retrier.activate()

        #expect(harness.activationAttemptCount == 1)
        #expect(harness.scheduledActionCount == 1)

        harness.completePresentationOnNextAttempt = true
        harness.runNextScheduledAction()

        #expect(harness.activationAttemptCount == 2)
        #expect(harness.scheduledActionCount == 0)

        // Finder (or another app) becomes active after GramDrive has already
        // satisfied the explicit presentation request. No stale work remains
        // that could reactivate GramDrive and steal focus back.
        harness.isPresentationComplete = false

        #expect(harness.activationAttemptCount == 2)
        #expect(harness.scheduledActionCount == 0)
    }

    @Test func newerActivationRequestInvalidatesOlderScheduledRetry() {
        let harness = WindowActivationRetryHarness()

        harness.retrier.activate()
        harness.retrier.activate()

        #expect(harness.activationAttemptCount == 2)
        #expect(harness.scheduledActionCount == 2)

        harness.runNextScheduledAction()

        #expect(harness.activationAttemptCount == 2)
        #expect(harness.scheduledActionCount == 1)

        harness.completePresentationOnNextAttempt = true
        harness.runNextScheduledAction()

        #expect(harness.activationAttemptCount == 3)
        #expect(harness.scheduledActionCount == 0)
    }
}

@MainActor
private final class WindowPresentationHarness {
    let lifecycle = CompanionApplicationLifecycle()
    let consumer = CompanionWindowPresentationConsumer()
    var events: [String] = []
    var visibleWindowIDs: Set<String> = []
    var windowOpenRequests: [String] = []
    var activationCount = 0
    private let onboarding: Bool

    init(onboarding: Bool) {
        self.onboarding = onboarding
    }

    var router: CompanionActionRouter {
        CompanionActionRouter(
            mainWindowID: "gramdrive-main",
            onboardingWindowID: "gramdrive-onboarding",
            shouldPresentOnboarding: { self.onboarding },
            openWindow: {
                self.events.append("window:\($0)")
                self.windowOpenRequests.append($0)
                self.visibleWindowIDs.insert($0)
            },
            activateApplication: {
                self.events.append("activate")
                self.activationCount += 1
            })
    }

    @discardableResult
    func consumePendingRequest() -> Bool {
        consumer.presentPendingRequest(from: lifecycle, using: router)
    }

    func closeAllWindows() {
        visibleWindowIDs.removeAll()
    }
}

@MainActor
private final class WindowActivationRetryHarness {
    var isPresentationComplete = false
    var completePresentationOnNextAttempt = false
    private(set) var activationAttemptCount = 0
    private var scheduledActions: [@MainActor @Sendable () -> Void] = []

    lazy var retrier = CompanionWindowActivationRetrier(
        maximumAttemptCount: 20,
        isPresentationComplete: { self.isPresentationComplete },
        performActivationAttempt: {
            self.activationAttemptCount += 1
            if self.completePresentationOnNextAttempt {
                self.completePresentationOnNextAttempt = false
                self.isPresentationComplete = true
            }
        },
        scheduleRetry: { self.scheduledActions.append($0) })

    var scheduledActionCount: Int {
        scheduledActions.count
    }

    func runNextScheduledAction() {
        scheduledActions.removeFirst()()
    }
}
