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

    var body: some Scene {
        MenuBarExtra("GramDrive", systemImage: "externaldrive.badge.person.crop") {
            CompanionRootView(model: model)
        }
        .menuBarExtraStyle(.window)

        // The same surface in a resizable window, for the full settings view.
        Window("GramDrive", id: "gramdrive-main") {
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
