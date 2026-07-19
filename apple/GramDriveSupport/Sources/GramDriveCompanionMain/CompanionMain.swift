import Foundation
import GramDriveAgentCore
import GramDriveCompanion
import GramDriveSupport
import SwiftUI

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
    @State private var model = CompanionViewModel.live(layout: GramDriveCompanionApp.layout())

    var body: some Scene {
        MenuBarExtra("GramDrive", systemImage: "externaldrive.badge.person.crop") {
            CompanionRootView(model: model)
        }
        .menuBarExtraStyle(.window)

        // The same surface in a resizable window, for the full settings view.
        Window("GramDrive", id: "gramdrive-main") {
            CompanionRootView(model: model)
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
