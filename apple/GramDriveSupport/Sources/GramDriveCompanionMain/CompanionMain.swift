import Foundation
import GramDriveAgentCore
import GramDriveCompanion
import GramDriveFileProvider
import GramDriveSupport
import Sparkle
import SwiftUI
import os

@MainActor
private final class GramDriveApplicationDelegate: NSObject, NSApplicationDelegate {
  static let lifecycle = CompanionApplicationLifecycle()
  private var terminationProgressAlert: NSAlert?
  private var pendingUpdateBuild: String?
  let updateAvailability = UpdateAvailability()
  private var updaterAvailabilityObservation: NSKeyValueObservation?

  func applicationWillFinishLaunching(_ notification: Notification) {
    guard
      InstalledPlaceholderResolutionCommand.isRequested(
        arguments: CommandLine.arguments)
    else { return }
    NSApplication.shared.setActivationPolicy(.prohibited)
    Task {
      let exitCode = await InstalledPlaceholderResolutionCommand.runSystem()
      fflush(stdout)
      exit(exitCode)
    }
  }

  lazy var updaterController = SPUStandardUpdaterController(
    startingUpdater: true,
    updaterDelegate: self,
    userDriverDelegate: nil)

  private lazy var terminationCoordinator = CompanionTerminationCoordinator.live(
    layout: GramDriveCompanionApp.layout())

  private lazy var terminationDriver = ApplicationTerminationRequestDriver(
    requestTermination: { [weak self] intent in
      await self?.terminationCoordinator.requestTermination(intent) ?? false
    },
    cancelTermination: { [weak self] in
      await self?.terminationCoordinator.cancelTermination() ?? false
    })

  func applicationDidFinishLaunching(_ notification: Notification) {
    guard
      !InstalledPlaceholderResolutionCommand.isRequested(
        arguments: CommandLine.arguments)
    else { return }
    Self.lifecycle.applicationDidFinishLaunching()
    updaterAvailabilityObservation = updaterController.updater.observe(
      \.canCheckForUpdates,
      options: [.initial, .new]
    ) { [weak self] updater, _ in
      Task { @MainActor [weak self] in
        self?.updateAvailability.setCanCheckForUpdates(updater.canCheckForUpdates)
      }
    }
  }

  func applicationShouldHandleReopen(
        _ sender: NSApplication,
        hasVisibleWindows flag: Bool
    ) -> Bool {
    Self.lifecycle.applicationShouldHandleReopen()
  }

  func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
    let intent = CompanionTerminationCoordinator.Intent.fromPendingUpdateBuild(pendingUpdateBuild)
    guard terminationDriver.applicationShouldTerminate(intent: intent, reply: { [weak self] allowed in
      self?.replyToTerminationOnce(allowed)
    }) else { return .terminateLater }
    presentTerminationProgress()
    return .terminateLater
  }

  func checkForUpdates() {
    ManualUpdateAction(
      availability: updateAvailability,
      activateApplication: { CompanionApplicationActivation.activate() },
      invokeUpdater: { [updaterController] in updaterController.checkForUpdates(nil) }
    ).invoke()
  }

  /// An in-flight AppKit termination has an explicit recovery route for a
  /// user who chooses to keep GramDrive open. The shared reply helper keeps
  /// the original request's reply exactly-once even if the normal drain task
  /// reaches a terminal result concurrently.
  func cancelPendingTermination() {
    terminationDriver.cancelPendingTermination { [weak self] allowed in
      self?.replyToTerminationOnce(allowed)
    }
  }

  private func replyToTerminationOnce(_ allowed: Bool) {
    dismissTerminationProgress()
    NSApplication.shared.reply(toApplicationShouldTerminate: allowed)
    if !allowed { presentTerminationCancelledExplanation() }
  }

  private func presentTerminationProgress() {
    guard let window = NSApplication.shared.keyWindow else { return }
    let alert = NSAlert()
    alert.messageText = "Preparing GramDrive to quit"
    alert.informativeText = "Waiting for File Provider transfers to reach a safe boundary."
    alert.addButton(withTitle: "Keep GramDrive Open")
    alert.addButton(withTitle: "Continue Quitting")
    terminationProgressAlert = alert
    alert.beginSheetModal(for: window) { [weak self] response in
      guard response == .alertFirstButtonReturn else { return }
      self?.cancelPendingTermination()
    }
  }

  private func dismissTerminationProgress() {
    guard let alert = terminationProgressAlert else { return }
    terminationProgressAlert = nil
    if let parent = alert.window.sheetParent {
      parent.endSheet(alert.window, returnCode: .abort)
    }
  }

  private func presentTerminationCancelledExplanation() {
    let alert = NSAlert()
    alert.messageText = "GramDrive is still running"
    alert.informativeText = terminationCoordinator.lastFailureMessage
      ?? "The agent could not safely stop. Try again, or use Force Quit if you need to stop immediately."
    alert.addButton(withTitle: "Try Again")
    alert.addButton(withTitle: "Keep GramDrive Open")
    if alert.runModal() == .alertFirstButtonReturn {
      NSApplication.shared.terminate(nil)
    }
  }
}

extension GramDriveApplicationDelegate: SPUUpdaterDelegate {
  func updater(_ updater: SPUUpdater, didDownloadUpdate item: SUAppcastItem) {
    pendingUpdateBuild = item.versionString
  }

  func updater(
    _ updater: SPUUpdater,
    willInstallUpdateOnQuit item: SUAppcastItem,
    immediateInstallationBlock: @escaping () -> Void
  ) -> Bool {
    // AppKit's termination gate owns the drain and its one reply. Returning
    // false keeps Sparkle from taking the immediate-install shortcut before
    // the agent has stopped admitting File Provider work.
    pendingUpdateBuild = item.versionString
    return false
  }
}

/// Retains one state-to-File-Provider change relay per registered domain for
/// the companion process lifetime. The lock permits replacement from the
/// detached setup operation without making the SwiftUI app global actor-bound.
private final class LiveChangeRelayRegistry: @unchecked Sendable {
    private let lock = NSLock()
    private var relays: [ChangeSignalRelay] = []

    func replace(with relays: [ChangeSignalRelay]) {
        lock.lock()
        let previous = self.relays
        self.relays = relays
        lock.unlock()
        for relay in previous { relay.stop() }
    }

    func signalAll() {
        lock.lock()
        let current = relays
        lock.unlock()
        for relay in current { relay.signalEnumeratorsAfterAgentReplacement() }
    }
}

/// The GramDrive companion shell (PLAT-MAC-005): a menu-bar app over the
/// engine-hosting agent. It hosts no engine and performs no Telegram
/// operation itself — it renders the agent's status and drives it through the
/// ``CompanionBackend`` seam, exactly the "menu-bar/settings shell" the
/// platform requirement calls for.
///
/// Not a File Provider extension and holds no filesystem callbacks: the AC's
/// "no Telegram operations from filesystem callbacks" holds because this
/// process has none.
@main
struct GramDriveCompanionApp: App {
    @NSApplicationDelegateAdaptor(GramDriveApplicationDelegate.self)
    private var applicationDelegate
    nonisolated private static let changeRelays = LiveChangeRelayRegistry()

    private static let domainSetup = CoalescingFileProviderDomainSetup {
        let registrar = SystemDomainRegistrar()
        switch await DomainStartupReconcile.run(registrar: registrar) {
        case .skipped:
            throw LiveDomainSetupError.sharedStorageUnavailable
        case .failed:
            throw LiveDomainSetupError.registrationFailed
        case .reconciled(let outcome):
            guard let domain = outcome.desired.first else {
                throw LiveDomainSetupError.authorizedAccountUnavailable
            }
            let platformURL = try await registrar.userVisibleRootURL(for: domain)
            try GramDriveCompanionApp.installChangeRelays(
                desired: outcome.desired, registrar: registrar)
            let rootURL = try await GramDriveCompanionApp.resolveDriveRoot(
                platformURL: platformURL)
            return FileProviderDomainSetupResult(rootURL: rootURL)
        }
    }

    nonisolated private static func installChangeRelays(
        desired: [DesiredDomain],
        registrar: SystemDomainRegistrar
    ) throws {
        let container = try AppGroup.containerURL()
        let dataRoot = AppGroup.dataRootURL(containerURL: container)
        var relays: [ChangeSignalRelay] = []
        for domain in desired {
            guard let signaling = registrar.changeSignaler(for: domain) else { continue }
            let store = try SharedState.open(dataRoot: dataRoot, role: .provider)
            guard let account = try store.account(accountId: domain.accountId) else {
                continue
            }
            let relay = ChangeSignalRelay(
                probe: { try store.dataVersion() },
                containerProbe: {
                    try ProviderContainerChangeResolver.changes(
                        store: store, account: account, after: $0)
                },
                signaling: signaling)
            try relay.start()
            relays.append(relay)
        }
        changeRelays.replace(with: relays)
    }

    @State private var model = CompanionViewModel.live(
        layout: GramDriveCompanionApp.layout(),
        domainSetup: GramDriveCompanionApp.domainSetup,
        // The app half of the SEC-004 removal: after the agent's engine
        // half completes, deregister the account's File Provider domain —
        // domain management can only run in the app embedding the
        // extension. The trace-free disposition matches the removal's
        // irreversible contract.
        accountDomainCleanup: { accountId in
            _ = try? await DomainRemoval.removeAccountDomain(
                accountId: accountId,
                disposition: .deleteLocalData,
                registrar: SystemDomainRegistrar(),
                remover: SystemDomainRemover())
        },
        matchingAgentReady: {
            GramDriveCompanionApp.changeRelays.signalAll()
        })

    /// The full-shell window id.
    static let mainWindowID = "gramdrive-main"
    /// The first-launch Welcome window id.
    static let onboardingWindowID = "gramdrive-onboarding"

    /// `getUserVisibleURL(for: .rootContainer)` may transiently resolve to the
    /// CloudStorage container while Finder is publishing the registered
    /// domain child. Accept an exact GramDrive URL immediately; otherwise
    /// wait a bounded interval for the child and fail into onboarding Retry.
    private static func resolveDriveRoot(platformURL: URL) async throws -> URL {
        if platformURL.lastPathComponent.hasPrefix(DomainIdentity.displayNameBase) {
            return platformURL
        }

        let location = CloudStorageDriveLocation()
        for _ in 0..<20 {
            if let rootURL = location.resolveDriveURL() {
                return rootURL
            }
            try await Task.sleep(for: .milliseconds(250))
        }
        throw LiveDomainSetupError.rootURLUnavailable
    }

    var body: some Scene {
        // The menu-bar item, present from first launch. Its click surface is
        // the compact status panel; the always-present label additionally
        // presents onboarding once on a clean machine (TASK-260720-31nw0w).
        MenuBarExtra {
            MenuBarStatusView(
                model: model,
                mainWindowID: GramDriveCompanionApp.mainWindowID,
                onboardingWindowID: GramDriveCompanionApp.onboardingWindowID,
                checkForUpdates: { applicationDelegate.checkForUpdates() },
                updateAvailability: applicationDelegate.updateAvailability)
        } label: {
            MenuBarLaunchLabel(
                systemImage: "externaldrive.badge.person.crop",
                startAgentSession: { await model.startAgentSession() },
                shouldPresentOnboarding: { model.onboarding.isPresented },
                mainWindowID: GramDriveCompanionApp.mainWindowID,
                onboardingWindowID: GramDriveCompanionApp.onboardingWindowID,
                lifecycle: GramDriveApplicationDelegate.lifecycle)
        }
        .menuBarExtraStyle(.window)

        // The first-launch Welcome window (shown once; re-runnable from Help).
        Window("Welcome to GramDrive", id: GramDriveCompanionApp.onboardingWindowID) {
            OnboardingView(model: model.onboarding)
        }
        .windowResizability(.contentSize)
        .defaultPosition(.center)

        // The same surface in a resizable window, for the full settings view.
        Window("GramDrive", id: GramDriveCompanionApp.mainWindowID) {
            CompanionRootView(
                model: model,
                checkForUpdates: { applicationDelegate.checkForUpdates() },
                updateAvailability: applicationDelegate.updateAvailability)
        }
        .commands {
            // SYNC-071: domain repair can *remove* domains, so it never runs
            // automatically — only from this explicit user action.
            CommandGroup(after: .appInfo) {
                Button("Check for Updates…") {
                    applicationDelegate.checkForUpdates()
                }
                .disabled(!applicationDelegate.updateAvailability.canCheckForUpdates)
                Button("Repair File Provider Domains…") {
                    GramDriveCompanionApp.repairFileProviderDomains()
                }
            }
            // Onboarding is re-runnable on demand from Help.
            CommandGroup(replacing: .help) {
                SetupGuideCommand(
                    windowID: GramDriveCompanionApp.onboardingWindowID,
                    onSelect: { model.onboarding.restart() })
            }
        }
    }

    /// The SYNC-071 user-triggered File Provider domain repair: the explicit
    /// action that runs the full ``DomainRepair`` — re-register lost domains
    /// and clean strays — off the main thread. Unlike the launch reconcile it
    /// can remove domains, so it never runs on its own; and it refuses a total
    /// teardown (an empty desired set against still-registered domains) by
    /// default, so a spurious-empty canonical read cannot wipe every domain
    /// even when the user asks to repair.
    static func repairFileProviderDomains() {
        Task.detached(priority: .utility) {
            let logger = Logger(
                subsystem: "com.reluxworks.gramdrive",
                category: "file-provider-domains"
            )
            switch await DomainRepair.run() {
            case .skipped(let reason):
                logger.info("domain repair skipped: \(reason, privacy: .public)")
            case .failed(let reason):
                logger.error("domain repair failed: \(reason, privacy: .public)")
            case .repaired(let outcome) where outcome.withheldTotalTeardown:
                logger.error(
                    """
                    domain repair withheld total teardown: \
                    \(outcome.withheldStrays.count, privacy: .public) domains \
                    left in place (no configured accounts)
                    """
                )
            case .repaired(let outcome):
                let plan = outcome.plan
                logger.info(
                    """
                    domain repair: adds=\(plan.adds.count) \
                    renames=\(plan.renames.count) keeps=\(plan.keeps.count) \
                    strays-removed=\(outcome.removedStrays.count)
                    """
                )
            }
        }
    }

    /// Resolves the agent runtime layout. Prefers the signed App Group
    /// container; falls back to a local Application Support root for an
    /// unsigned dev run, so the shell is runnable without the entitlement.
    nonisolated static func layout() -> AgentRuntimeLayout {
        if let container = try? AppGroup.containerURL() {
            return AgentRuntimeLayout(dataRoot: AppGroup.dataRootURL(containerURL: container))
        }
        let fallback = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first?
            .appendingPathComponent("GramDrive", isDirectory: true)
            ?? URL(fileURLWithPath: NSTemporaryDirectory(), isDirectory: true)
                .appendingPathComponent("GramDrive", isDirectory: true)
        return AgentRuntimeLayout(dataRoot: fallback)
    }
}

/// Stable, non-sensitive failure categories for the live setup seam. Raw
/// platform errors never reach onboarding, logs, or task evidence.
private enum LiveDomainSetupError: Error {
    case sharedStorageUnavailable
    case registrationFailed
    case authorizedAccountUnavailable
    case rootURLUnavailable
}

/// The always-present menu-bar label. Beyond drawing the status glyph, it is
/// the app's one reliably-instantiated view at launch in a menu-bar-only
/// (`LSUIElement`) shell, so it consumes durable cold-launch and reopen
/// requests from the AppKit delegate and routes them through the same singleton
/// window action used by the menu bar.
private struct MenuBarLaunchLabel: View {
    let systemImage: String
    let startAgentSession: () async -> Void
    let shouldPresentOnboarding: () -> Bool
    let mainWindowID: String
    let onboardingWindowID: String
    let lifecycle: CompanionApplicationLifecycle
    @Environment(\.openWindow) private var openWindow
    @State private var presentationConsumer = CompanionWindowPresentationConsumer()
    @State private var didStartAgentSession = false

    var body: some View {
        Image(systemName: systemImage)
            .task {
                presentPendingApplicationRequest()
                guard !didStartAgentSession else { return }
                didStartAgentSession = true
                await startAgentSession()
            }
            .onChange(of: lifecycle.presentationGeneration) {
                presentPendingApplicationRequest()
            }
    }

    private var actionRouter: CompanionActionRouter {
        CompanionActionRouter(
            mainWindowID: mainWindowID,
            onboardingWindowID: onboardingWindowID,
            shouldPresentOnboarding: shouldPresentOnboarding,
            openWindow: { openWindow(id: $0) },
            activateApplication: { CompanionApplicationActivation.activate() })
    }

    private func presentPendingApplicationRequest() {
        presentationConsumer.presentPendingRequest(from: lifecycle, using: actionRouter)
    }
}

/// The Help ▸ Setup Guide command: restarts the onboarding flow and opens its
/// window. A small view so it can read the `openWindow` environment action.
private struct SetupGuideCommand: View {
    let windowID: String
    let onSelect: () -> Void
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("GramDrive Setup Guide…") {
            onSelect()
            openWindow(id: windowID)
            CompanionApplicationActivation.activate()
        }
    }
}
