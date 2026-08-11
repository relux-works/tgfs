import GramDriveAgentCore
import SwiftUI

/// The companion shell's root: a sidebar over the six required sections and a
/// detail pane that renders the selected one. Presented from the menu-bar
/// window and the settings window alike.
@MainActor
public struct CompanionRootView: View {
  @Bindable private var model: CompanionViewModel
  private let checkForUpdates: () -> Void
  private let updateAvailability: UpdateAvailability

  public init(
    model: CompanionViewModel,
    checkForUpdates: @escaping () -> Void = {},
    updateAvailability: UpdateAvailability = .unavailable
  ) {
    self.model = model
    self.checkForUpdates = checkForUpdates
    self.updateAvailability = updateAvailability
  }

  public var body: some View {
    NavigationSplitView {
      List(CompanionSection.allCases, selection: sectionSelection) { section in
        Label(section.title, systemImage: section.systemImage)
          .tag(section)
      }
      .navigationTitle("GramDrive")
      .navigationSplitViewColumnWidth(min: 180, ideal: 200)
    } detail: {
      detail(for: model.selectedSection)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .padding(20)
    }
    .frame(minWidth: 680, minHeight: 460)
    .task { await model.refresh() }
    .toolbar {
      Button("Check for Updates…") {
        ManualUpdateAction(
          availability: updateAvailability, invokeUpdater: checkForUpdates).invoke()
      }
      .disabled(!updateAvailability.canCheckForUpdates)
        .accessibilityLabel("Check for Updates")
    }
  }

  private var sectionSelection: Binding<CompanionSection?> {
    Binding(
      get: { model.selectedSection },
      set: { if let value = $0 { model.selectedSection = value } })
  }

  @ViewBuilder
  private func detail(for section: CompanionSection) -> some View {
    switch section {
    case .account:
      AccountStatusView(model: model.status, contentPolicies: model.contentPolicies)
    case .authorization:
      AuthorizationView(model: model.authorization)
    case .storage:
      StorageSettingsView(
        model: model.settings,
        contentPolicies: model.contentPolicies)
    case .diagnostics:
      DiagnosticsView(model: model.status)
    case .repair:
      RepairView(model: model.repair)
    case .removal:
      AccountRemovalView(model: model.removal)
    }
  }
}

/// The account/provider status screen. Everything here is a pure projection
/// of the last health reading — honest "not reported yet" where the engine
/// has not wired a field.
public struct AccountStatusView: View {
  private let model: CompanionStatusViewModel
  private let contentPolicies: ContentPolicySettingsViewModel

  public init(
    model: CompanionStatusViewModel,
    contentPolicies: ContentPolicySettingsViewModel
  ) {
    self.model = model
    self.contentPolicies = contentPolicies
  }

  public var body: some View {
    Form {
      Section("Agent") {
        LabeledContent("Engine host", value: model.agentPresence.label)
      }
      Section("Account") {
        LabeledContent("Authorization", value: model.accountStatus.label)
        ForEach(contentPolicies.accounts) { account in
          if let status = account.status {
            LabeledContent(account.displayName, value: status.retention.label)
            LabeledContent(
              "\(account.displayName) Archive Mode",
              value: status.archiveModeEnabled ? "On" : "Off")
          } else if let message = account.availability.message {
            LabeledContent(account.displayName, value: message)
          }
        }
      }
      Section("File Provider") {
        LabeledContent("Domain", value: model.providerStatus.label)
      }
      Section {
        Button("Refresh") {
          Task {
            await model.refresh()
            await contentPolicies.refresh(from: model.readout)
          }
        }
      }
    }
    .formStyle(.grouped)
    .navigationTitle("Account")
  }
}

/// Formats a byte count for display (base-10, matching the quota's unit).
func formattedBytes(_ bytes: UInt64) -> String {
  let formatter = ByteCountFormatter()
  formatter.countStyle = .decimal
  return formatter.string(fromByteCount: Int64(clamping: bytes))
}

#if DEBUG
  #Preview("Account — running") {
    let backend = InMemoryCompanionBackend(
      health: .running(previewSnapshot(state: .running)))
    return CompanionRootView(
      model: CompanionViewModel(
        backend: backend,
        diskProbe: FixedDiskSpaceProbe(available: 500_000_000_000),
        accountLabel: "Preview account",
        domainSetup: FixedFileProviderDomainSetup(
          rootURL: URL(fileURLWithPath: "/Users/preview/Library/CloudStorage/GramDrive"))))
  }
#endif
