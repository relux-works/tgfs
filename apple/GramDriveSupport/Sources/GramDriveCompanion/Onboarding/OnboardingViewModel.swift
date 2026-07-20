import Foundation
import GramDriveAgentCore

/// The initial-sync standing the success step surfaces, projected purely from
/// the agent's health so it never fabricates progress the engine did not
/// report (the same honesty the status screens keep).
public enum InitialSyncStatus: Equatable, Sendable {
    /// No agent is answering yet — nothing can be syncing.
    case waitingForAgent
    /// The agent is up but has not reported a first source update: connecting
    /// to Telegram and preparing the chat list.
    case preparing
    /// Content transfers are in flight (`count` hydrations pending).
    case syncing(pending: Int)
    /// The agent is up, has synced, and nothing is pending.
    case upToDate

    /// A short, user-facing line for the success step.
    public var label: String {
        switch self {
        case .waitingForAgent:
            return "Waiting for the GramDrive agent to start…"
        case .preparing:
            return "Connecting to Telegram and preparing your chats…"
        case .syncing(let pending):
            return "Syncing your chats… (\(pending) \(pending == 1 ? "item" : "items") in progress)"
        case .upToDate:
            return "Your chats are ready in Finder."
        }
    }

    /// Whether the UI should show an active (indeterminate) progress spinner.
    public var isActive: Bool {
        switch self {
        case .waitingForAgent, .preparing, .syncing: return true
        case .upToDate: return false
        }
    }
}

/// The first-launch onboarding flow (TASK-260720-31nw0w): a guided
/// Welcome → Sign In → Choose defaults → Success wizard.
///
/// It drives the *same* live view models the companion shell uses — the one
/// ``AuthorizationViewModel`` so sign-in genuinely authorizes over the agent
/// control channel, the one ``CompanionSettingsViewModel`` so the defaults the
/// user picks persist to the settings document the agent reads, and the one
/// ``CompanionStatusViewModel`` so the success step's progress reflects the
/// real agent. Navigation, gating, and completion are the testable surface;
/// each step's screen is a thin view over these models.
@MainActor
@Observable
public final class OnboardingViewModel {
    /// The steps of the guided flow, in order.
    public enum Step: Int, CaseIterable, Identifiable, Sendable {
        case welcome
        case signIn
        case defaults
        case success

        public var id: Int { rawValue }

        public var title: String {
            switch self {
            case .welcome: return "Welcome"
            case .signIn: return "Sign In"
            case .defaults: return "Choose Defaults"
            case .success: return "You're All Set"
            }
        }
    }

    /// The current step.
    public private(set) var step: Step = .welcome
    /// Whether the Welcome window should be on screen. Seeded from the
    /// persisted completion flag so a clean machine opens onboarding on first
    /// launch and a returning user does not.
    public var isPresented: Bool

    /// The shared sign-in flow (embedded on the Sign In step).
    public let authorization: AuthorizationViewModel
    /// The shared settings editor (embedded on the Choose Defaults step).
    public let settings: CompanionSettingsViewModel
    /// The shared status reader (drives the success step's sync progress).
    public let status: CompanionStatusViewModel

    private let driveLocation: any DriveLocationProviding
    private let completionStore: any OnboardingCompletionStore

    public init(
        authorization: AuthorizationViewModel,
        settings: CompanionSettingsViewModel,
        status: CompanionStatusViewModel,
        driveLocation: any DriveLocationProviding,
        completionStore: any OnboardingCompletionStore
    ) {
        self.authorization = authorization
        self.settings = settings
        self.status = status
        self.driveLocation = driveLocation
        self.completionStore = completionStore
        self.isPresented = !completionStore.hasCompletedOnboarding()
    }

    // MARK: - Navigation

    /// Whether the current step's primary action can advance. Only sign-in
    /// gates: the flow does not leave it until the account is authorized.
    public var canAdvance: Bool {
        switch step {
        case .welcome, .defaults, .success:
            return true
        case .signIn:
            return authorization.isAuthorized
        }
    }

    public var isFirstStep: Bool { step == Step.allCases.first }
    public var isLastStep: Bool { step == Step.allCases.last }

    /// The primary-button title for the current step.
    public var primaryActionTitle: String {
        switch step {
        case .welcome: return "Get Started"
        case .signIn: return "Continue"
        case .defaults: return "Continue"
        case .success: return "Done"
        }
    }

    /// Advances to the next step (or finishes on the last one). A no-op when
    /// the current step cannot advance yet.
    public func advance() {
        guard canAdvance else { return }
        switch step {
        case .welcome:
            step = .signIn
        case .signIn:
            settings.load()
            step = .defaults
        case .defaults:
            // Persist the chosen defaults before leaving the step.
            settings.save()
            step = .success
        case .success:
            finish()
        }
    }

    /// Steps back one screen. A no-op on the first step.
    public func back() {
        guard let previous = Step(rawValue: step.rawValue - 1) else { return }
        step = previous
    }

    /// Finishes onboarding: persists whatever defaults are set, records
    /// completion so it never auto-shows again, and dismisses the window.
    public func finish() {
        settings.save()
        completionStore.setCompletedOnboarding(true)
        isPresented = false
    }

    /// Dismisses onboarding without walking to the end (the explicit "Skip
    /// setup" affordance). Records completion so it is genuinely shown once.
    public func skip() {
        completionStore.setCompletedOnboarding(true)
        isPresented = false
    }

    /// Re-runs onboarding from the start — the Help ▸ Setup Guide entry.
    public func restart() {
        step = .welcome
        isPresented = true
    }

    // MARK: - Sign In

    /// Starts the sign-in flow if it has not begun. Idempotent for a step the
    /// user may revisit: it only (re)begins from a fresh `idle` state, so it
    /// never interrupts a flow already in progress or already authorized.
    public func beginSignInIfNeeded() async {
        guard authorization.state == .idle, authorization.unavailable == nil else { return }
        await authorization.begin()
    }

    // MARK: - Success

    /// The drive's user-visible Finder URL, when resolvable.
    public var driveURL: URL? { driveLocation.resolveDriveURL() }

    /// Reveals the drive in Finder. Returns whether a location could be shown.
    @discardableResult
    public func openDriveInFinder() -> Bool {
        driveLocation.reveal()
    }

    /// Refreshes the agent health backing the sync indicator.
    public func refreshStatus() async {
        await status.refresh()
    }

    /// The initial-sync status for the success step.
    public var initialSync: InitialSyncStatus { Self.initialSync(from: status.readout) }

    /// Pure projection of one health reading to a sync status — so every
    /// success-step state is a snapshot away in a test.
    public nonisolated static func initialSync(from readout: HealthReadout) -> InitialSyncStatus {
        switch readout {
        case .notRunning, .timedOut, .error:
            return .waitingForAgent
        case .running(let snapshot):
            if snapshot.pendingTransferCount > 0 {
                return .syncing(pending: snapshot.pendingTransferCount)
            }
            // No source update reported yet → still preparing the chat list.
            if snapshot.lastSourceUpdateMs == nil {
                return .preparing
            }
            return .upToDate
        }
    }
}
