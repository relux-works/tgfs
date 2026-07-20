import Foundation
import GramDriveAgentCore
import GramDriveCompanion
import GramDriveFileProvider
import GramDriveSupport
import SwiftUI
import os

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
    @State private var model = CompanionViewModel.live(
        layout: GramDriveCompanionApp.layout(),
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
        })

    init() {
        // Launch-time File Provider domain reconcile (TASK-260715-3s44pc
        // registration): the shell is the app that embeds the extension, so
        // domain management runs here — off the main thread, never blocking or
        // failing startup.
        //
        // This is the SYNC-070 startup recovery, and it is deliberately the
        // *add-only* reconcile, not the full repair: it re-registers every
        // account's domain (recovering Finder state under the stable
        // identifier) but never removes anything. Automatically tearing down
        // Finder state on every launch is exactly the failure mode the
        // reconcile/repair split exists to prevent — an empty or partial
        // canonical read at startup would otherwise make every domain look
        // like a stray and wipe it. Stray cleanup is the user-triggered repair
        // (SYNC-071), wired behind the explicit "Repair File Provider Domains"
        // command below. Idempotent: a healthy install logs a settled pass.
        Task.detached(priority: .utility) {
            let logger = Logger(
                subsystem: "com.reluxworks.gramdrive",
                category: "file-provider-domains"
            )
            switch await DomainStartupReconcile.run() {
            case .skipped(let reason):
                logger.info("domain reconcile skipped: \(reason, privacy: .public)")
            case .failed(let reason):
                logger.error("domain reconcile failed: \(reason, privacy: .public)")
            case .reconciled(let outcome):
                let plan = outcome.plan
                logger.info(
                    """
                    domain reconcile: adds=\(plan.adds.count) \
                    renames=\(plan.renames.count) keeps=\(plan.keeps.count) \
                    strays=\(plan.strays.count)
                    """
                )
            }
        }
    }

    /// The full-shell window id.
    static let mainWindowID = "gramdrive-main"
    /// The first-launch Welcome window id.
    static let onboardingWindowID = "gramdrive-onboarding"

    var body: some Scene {
        // The menu-bar item, present from first launch. Its click surface is
        // the compact status panel; the always-present label additionally
        // presents onboarding once on a clean machine (TASK-260720-31nw0w).
        MenuBarExtra {
            MenuBarStatusView(
                model: model,
                mainWindowID: GramDriveCompanionApp.mainWindowID,
                onboardingWindowID: GramDriveCompanionApp.onboardingWindowID)
        } label: {
            MenuBarLaunchLabel(
                systemImage: "externaldrive.badge.person.crop",
                shouldPresentOnboarding: { model.onboarding.isPresented },
                onboardingWindowID: GramDriveCompanionApp.onboardingWindowID)
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
            CompanionRootView(model: model)
        }
        .commands {
            // SYNC-071: domain repair can *remove* domains, so it never runs
            // automatically — only from this explicit user action.
            CommandGroup(after: .appInfo) {
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
    static func layout() -> AgentRuntimeLayout {
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

/// The always-present menu-bar label. Beyond drawing the status glyph, it is
/// the app's one reliably-instantiated view at launch in a menu-bar-only
/// (`LSUIElement`) shell, so it hosts the first-launch onboarding trigger:
/// once per process, if onboarding has not been completed, it opens the
/// Welcome window and brings the app forward. Manual entry points (the
/// compact panel's "Setup Guide…" and Help ▸ GramDrive Setup Guide) cover the
/// re-run and any case where the launch trigger does not fire.
private struct MenuBarLaunchLabel: View {
    let systemImage: String
    let shouldPresentOnboarding: () -> Bool
    let onboardingWindowID: String
    @Environment(\.openWindow) private var openWindow
    @State private var didAttemptLaunchPresentation = false

    var body: some View {
        Image(systemName: systemImage)
            .task {
                guard !didAttemptLaunchPresentation else { return }
                didAttemptLaunchPresentation = true
                guard shouldPresentOnboarding() else { return }
                openWindow(id: onboardingWindowID)
                NSApplication.shared.activate(ignoringOtherApps: true)
            }
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
            NSApplication.shared.activate(ignoringOtherApps: true)
        }
    }
}
