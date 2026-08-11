import GramDriveAgentCore
import SwiftUI

/// The compact status surface shown when the menu-bar item is clicked: a
/// small at-a-glance panel over the same live status, with the actions that
/// open the full window, reveal the drive in Finder, and re-run setup.
///
/// Present from first launch — the menu-bar item exists whether or not the
/// user has onboarded — and deliberately small: the full six-section shell
/// lives behind "Open GramDrive…".
@MainActor
public struct MenuBarStatusView: View {
  @Bindable private var model: CompanionViewModel
  private let mainWindowID: String
  private let onboardingWindowID: String
  private let checkForUpdates: () -> Void
  private let updateAvailability: UpdateAvailability
  @Environment(\.openWindow) private var openWindow

  public init(
    model: CompanionViewModel,
    mainWindowID: String,
    onboardingWindowID: String,
    checkForUpdates: @escaping () -> Void = {},
    updateAvailability: UpdateAvailability = .unavailable
  ) {
    self.model = model
    self.mainWindowID = mainWindowID
    self.onboardingWindowID = onboardingWindowID
    self.checkForUpdates = checkForUpdates
    self.updateAvailability = updateAvailability
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
      if let policy = model.contentPolicies.accounts.first, let status = policy.status {
        statusRow(
          title: model.contentPolicies.accounts.count == 1
            ? "Retention" : "Retention (\(policy.displayName))",
          value: status.retention.label,
          systemImage: "clock.arrow.circlepath")
      }
      statusRow(
        title: "Sync", value: model.onboarding.initialSync.label,
        systemImage: model.onboarding.initialSync.isActive
          ? "arrow.triangle.2.circlepath" : "checkmark.circle")

      Divider()

      Button("Open GramDrive…") {
        actionRouter.openGramDrive()
      }
      Button("Open in Finder") { actionRouter.openInFinder() }
      Button("Set Up GramDrive…") {
        actionRouter.setUpGramDrive()
      }

      Button("Check for Updates…") {
        ManualUpdateAction(
          availability: updateAvailability, invokeUpdater: checkForUpdates).invoke()
      }
      .disabled(!updateAvailability.canCheckForUpdates)

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
    .task {
      await model.status.refresh()
      await model.contentPolicies.refresh(from: model.status.readout)
    }
  }

  private var agentGlyph: String {
    switch model.status.agentPresence {
    case .running: return "checkmark.circle.fill"
    case .notRunning: return "pause.circle"
    case .unreachable: return "exclamationmark.triangle.fill"
    }
  }

  private var actionRouter: CompanionActionRouter {
    CompanionActionRouter(
      mainWindowID: mainWindowID,
      onboardingWindowID: onboardingWindowID,
      shouldPresentOnboarding: { model.onboarding.isPresented },
      openWindow: { openWindow(id: $0) },
      activateApplication: { CompanionApplicationActivation.activate() },
      openDriveInFinder: { model.onboarding.openDriveInFinder() },
      restartOnboarding: { model.onboarding.restart() })
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
      domainSetup: FixedFileProviderDomainSetup(
        rootURL: URL(fileURLWithPath: "/Users/preview/Library/CloudStorage/GramDrive")),
      onboardingStore: InMemoryOnboardingCompletionStore(completed: true))
    return MenuBarStatusView(
      model: model, mainWindowID: "main", onboardingWindowID: "onboarding")
  }
#endif
