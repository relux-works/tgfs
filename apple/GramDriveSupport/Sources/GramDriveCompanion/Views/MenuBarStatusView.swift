import GramDriveAgentCore
import SwiftUI

/// The compact status surface shown when the menu-bar item is clicked: a
/// small at-a-glance panel over the same live status, with the actions that
/// open the full window, reveal the drive in Finder, and re-run setup.
///
/// Present from first launch — the menu-bar item exists whether or not the
/// user has onboarded — and deliberately small: the full six-section shell
/// lives behind "Open GramDrive…".
public struct MenuBarStatusView: View {
    @Bindable private var model: CompanionViewModel
    private let mainWindowID: String
    private let onboardingWindowID: String
    @Environment(\.openWindow) private var openWindow

    public init(
        model: CompanionViewModel,
        mainWindowID: String,
        onboardingWindowID: String
    ) {
        self.model = model
        self.mainWindowID = mainWindowID
        self.onboardingWindowID = onboardingWindowID
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Image(systemName: "externaldrive.badge.person.crop")
                    .foregroundStyle(.tint)
                Text("GramDrive").font(.headline)
                Spacer()
            }

            statusRow(
                title: "Agent", value: model.status.agentPresence.label,
                systemImage: agentGlyph)
            statusRow(
                title: "Account", value: model.status.accountStatus.label,
                systemImage: "person.crop.circle")
            statusRow(
                title: "Sync", value: model.onboarding.initialSync.label,
                systemImage: model.onboarding.initialSync.isActive
                    ? "arrow.triangle.2.circlepath" : "checkmark.circle")

            Divider()

            Button("Open GramDrive…") { openWindow(id: mainWindowID) }
            Button("Open in Finder") { model.onboarding.openDriveInFinder() }
            Button("Setup Guide…") {
                model.onboarding.restart()
                openWindow(id: onboardingWindowID)
            }

            Divider()

            Button("Quit GramDrive") {
                #if canImport(AppKit)
                NSApplication.shared.terminate(nil)
                #endif
            }
        }
        .buttonStyle(.plain)
        .padding(12)
        .frame(width: 280)
        .task { await model.status.refresh() }
    }

    private var agentGlyph: String {
        switch model.status.agentPresence {
        case .running: return "checkmark.circle.fill"
        case .notRunning: return "pause.circle"
        case .unreachable: return "exclamationmark.triangle.fill"
        }
    }

    private func statusRow(title: String, value: String, systemImage: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: systemImage)
                .foregroundStyle(.secondary)
                .frame(width: 18)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 1) {
                Text(title).font(.caption).foregroundStyle(.secondary)
                Text(value).font(.callout)
            }
            Spacer(minLength: 0)
        }
    }
}

#if DEBUG
#Preview("Menu bar — running") {
    let backend = InMemoryCompanionBackend(health: .running(previewSnapshot(state: .running)))
    let model = CompanionViewModel(
        backend: backend,
        diskProbe: FixedDiskSpaceProbe(available: 500_000_000_000),
        accountLabel: "Preview account",
        driveLocation: FixedDriveLocation(url: nil),
        onboardingStore: InMemoryOnboardingCompletionStore(completed: true))
    return MenuBarStatusView(
        model: model, mainWindowID: "main", onboardingWindowID: "onboarding")
}
#endif
