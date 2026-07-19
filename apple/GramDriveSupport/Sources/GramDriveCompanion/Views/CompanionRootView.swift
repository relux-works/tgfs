import GramDriveAgentCore
import SwiftUI

/// The companion shell's root: a sidebar over the six required sections and a
/// detail pane that renders the selected one. Presented from the menu-bar
/// window and the settings window alike.
public struct CompanionRootView: View {
    @Bindable private var model: CompanionViewModel

    public init(model: CompanionViewModel) {
        self.model = model
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
            AccountStatusView(model: model.status)
        case .authorization:
            AuthorizationView(model: model.authorization)
        case .storage:
            StorageSettingsView(model: model.settings)
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

    public init(model: CompanionStatusViewModel) {
        self.model = model
    }

    public var body: some View {
        Form {
            Section("Agent") {
                LabeledContent("Engine host", value: model.agentPresence.label)
            }
            Section("Account") {
                LabeledContent("Authorization", value: model.accountStatus.label)
            }
            Section("File Provider") {
                LabeledContent("Domain", value: model.providerStatus.label)
            }
            Section {
                Button("Refresh") { Task { await model.refresh() } }
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
            accountLabel: "Preview account"))
}
#endif
