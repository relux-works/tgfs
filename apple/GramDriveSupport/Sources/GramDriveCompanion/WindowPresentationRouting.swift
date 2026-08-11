import Observation

#if canImport(AppKit)
import AppKit
#endif

/// Converts application lifecycle callbacks into monotonic presentation
/// requests. The request survives view construction order, so a cold-launch
/// callback that arrives before SwiftUI creates the menu-bar label is not lost.
@MainActor
@Observable
public final class CompanionApplicationLifecycle {
    public private(set) var presentationGeneration: UInt64 = 0

    public init() {}

    public func applicationDidFinishLaunching() {
        requestWindowPresentation()
    }

    @discardableResult
    public func applicationShouldHandleReopen() -> Bool {
        requestWindowPresentation()
        return true
    }

    private func requestWindowPresentation() {
        presentationGeneration &+= 1
    }
}

/// Coalesces repeated observation of one lifecycle generation while still
/// allowing every explicit launch/reopen event to raise the singleton scene.
@MainActor
public final class CompanionWindowPresentationConsumer {
    public private(set) var handledGeneration: UInt64 = 0

    public init() {}

    @discardableResult
    public func presentPendingRequest(
        from lifecycle: CompanionApplicationLifecycle,
        using router: CompanionActionRouter
    ) -> Bool {
        guard lifecycle.presentationGeneration != handledGeneration else {
            return false
        }
        handledGeneration = lifecycle.presentationGeneration
        router.presentAppropriateWindow()
        return true
    }
}

/// The one product-level route for launch, reopen, and menu-bar window actions.
/// Both window ids refer to SwiftUI singleton `Window` scenes, so opening an id
/// raises/reuses its existing scene instead of creating another instance.
@MainActor
public struct CompanionActionRouter {
    private let mainWindowID: String
    private let onboardingWindowID: String
    private let shouldPresentOnboarding: () -> Bool
    private let openWindow: (String) -> Void
    private let activateApplication: () -> Void
    private let openDriveInFinder: () -> Void
    private let restartOnboarding: () -> Void

    public init(
        mainWindowID: String,
        onboardingWindowID: String,
        shouldPresentOnboarding: @escaping () -> Bool,
        openWindow: @escaping (String) -> Void,
        activateApplication: @escaping () -> Void,
        openDriveInFinder: @escaping () -> Void = {},
        restartOnboarding: @escaping () -> Void = {}
    ) {
        self.mainWindowID = mainWindowID
        self.onboardingWindowID = onboardingWindowID
        self.shouldPresentOnboarding = shouldPresentOnboarding
        self.openWindow = openWindow
        self.activateApplication = activateApplication
        self.openDriveInFinder = openDriveInFinder
        self.restartOnboarding = restartOnboarding
    }

    public func presentAppropriateWindow() {
        presentWindow(id: shouldPresentOnboarding() ? onboardingWindowID : mainWindowID)
    }

    public func openGramDrive() {
        presentWindow(id: mainWindowID)
    }

    public func setUpGramDrive() {
        restartOnboarding()
        presentWindow(id: onboardingWindowID)
    }

    public func openInFinder() {
        openDriveInFinder()
    }

    private func presentWindow(id: String) {
        openWindow(id)
        activateApplication()
    }
}

/// Activates an `LSUIElement` application after SwiftUI has been asked to open
/// its scene. Bounded retries cover both asynchronous scene ordering and the
/// activation-policy transition without blocking the main run loop.
@MainActor
public enum CompanionApplicationActivation {
    public static func activate() {
        #if canImport(AppKit)
        CompanionWindowActivationController.shared.activate()
        #endif
    }
}

/// Owns one bounded activation-retry generation. A newer explicit request
/// invalidates callbacks from the previous generation, and observable success
/// ends the generation immediately so a later user focus choice is respected.
@MainActor
final class CompanionWindowActivationRetrier {
    typealias ScheduledAction = @MainActor @Sendable () -> Void

    private let maximumAttemptCount: Int
    private let isPresentationComplete: @MainActor () -> Bool
    private let performActivationAttempt: @MainActor () -> Void
    private let scheduleRetry: @MainActor (@escaping ScheduledAction) -> Void
    private var requestGeneration: UInt64 = 0

    init(
        maximumAttemptCount: Int,
        isPresentationComplete: @escaping @MainActor () -> Bool,
        performActivationAttempt: @escaping @MainActor () -> Void,
        scheduleRetry: @escaping @MainActor (@escaping ScheduledAction) -> Void
    ) {
        precondition(maximumAttemptCount > 0)
        self.maximumAttemptCount = maximumAttemptCount
        self.isPresentationComplete = isPresentationComplete
        self.performActivationAttempt = performActivationAttempt
        self.scheduleRetry = scheduleRetry
    }

    func activate() {
        requestGeneration &+= 1
        attempt(
            generation: requestGeneration,
            attemptsRemaining: maximumAttemptCount)
    }

    private func attempt(generation: UInt64, attemptsRemaining: Int) {
        guard generation == requestGeneration, !isPresentationComplete() else {
            return
        }

        performActivationAttempt()

        guard generation == requestGeneration,
              !isPresentationComplete(),
              attemptsRemaining > 1
        else {
            return
        }

        scheduleRetry { [weak self] in
            self?.attempt(
                generation: generation,
                attemptsRemaining: attemptsRemaining - 1)
        }
    }
}

#if canImport(AppKit)
/// `LSUIElement` starts the process with accessory behavior. AppKit does not
/// make that process frontmost even after `activate()` and window ordering, so
/// the first explicit window request promotes this process to regular activation
/// policy. It stays foreground-capable until Quit; demoting after close makes a
/// later LaunchServices reopen impossible to front through supported AppKit APIs.
@MainActor
private final class CompanionWindowActivationController {
    static let shared = CompanionWindowActivationController()

    private lazy var activationRetrier = CompanionWindowActivationRetrier(
        maximumAttemptCount: 20,
        isPresentationComplete: { [weak self] in
            self?.isPresentationComplete() ?? true
        },
        performActivationAttempt: { [weak self] in
            self?.performActivationAttempt()
        },
        scheduleRetry: { action in
            DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(50)) {
                action()
            }
        })

    func activate() {
        activationRetrier.activate()
    }

    private func isPresentationComplete() -> Bool {
        guard NSApplication.shared.isActive,
              let keyWindow = NSApplication.shared.keyWindow
        else {
            return false
        }
        return isPresentationWindow(keyWindow)
    }

    private func performActivationAttempt() {
        NSApplication.shared.setActivationPolicy(.regular)
        let preferredWindow = [
            NSApplication.shared.keyWindow,
            NSApplication.shared.mainWindow,
        ]
            .compactMap { $0 }
            .first(where: isPresentationWindow)
        let keyCapableWindow = preferredWindow
            ?? NSApplication.shared.windows.last(where: isPresentationWindow)
        keyCapableWindow?.makeKeyAndOrderFront(nil)
        keyCapableWindow?.orderFrontRegardless()
        NSApplication.shared.activate(ignoringOtherApps: true)
    }

    private func isPresentationWindow(_ window: NSWindow) -> Bool {
        window.isVisible && window.canBecomeKey && window.level == .normal
    }
}
#endif
