import SwiftUI

/// The repair screen: one button to ask the agent to reconcile, and a
/// rendering of the outcome — including the honest "not available yet" state
/// while the agent has no control channel.
public struct RepairView: View {
    private let model: RepairViewModel

    public init(model: RepairViewModel) {
        self.model = model
    }

    public var body: some View {
        Form {
            Section {
                Text("Repair re-opens the local state and reconciles it with Telegram. It changes no messages.")
                    .foregroundStyle(.secondary)
            }
            Section("Result") {
                switch model.phase {
                case .idle:
                    Text("Not run yet.").foregroundStyle(.secondary)
                case .running:
                    ProgressView("Repairing…")
                case .succeeded:
                    Label("Repair completed.", systemImage: "checkmark.seal.fill")
                        .foregroundStyle(.green)
                case .unavailable(let reason):
                    Label(reason.message, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.secondary)
                case .failed(let failure):
                    Label(failure.message, systemImage: "xmark.octagon")
                        .foregroundStyle(.red)
                }
            }
            Section {
                Button("Run Repair") { Task { await model.repair() } }
                    .disabled(!model.canRepair)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Repair")
    }
}

/// The account-removal screen: an explicit, typed confirmation gate in front
/// of an irreversible wipe (SEC-004), and a rendering of the outcome. The
/// wipe itself runs in the agent; the shell only gates and reports.
public struct AccountRemovalView: View {
    @Bindable private var model: AccountRemovalViewModel

    public init(model: AccountRemovalViewModel) {
        self.model = model
    }

    public var body: some View {
        Form {
            Section {
                Label(
                    "Removing “\(model.accountLabel)” logs it out and erases its local archive. "
                        + "This cannot be undone.",
                    systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            }
            Section("Confirm") {
                Toggle("I understand this is irreversible", isOn: $model.acknowledgedIrreversible)
                TextField("Type “\(model.accountLabel)” to confirm", text: $model.typedConfirmation)
            }
            Section("Result") {
                switch model.phase {
                case .idle, .confirming:
                    Text("Awaiting confirmation.").foregroundStyle(.secondary)
                case .removing:
                    ProgressView("Removing…")
                case .removed:
                    Label("Account removed.", systemImage: "checkmark.seal.fill")
                        .foregroundStyle(.green)
                case .invalidConfirmation:
                    Label("Confirmation doesn't match — nothing was removed.", systemImage: "hand.raised")
                        .foregroundStyle(.secondary)
                case .unavailable(let reason):
                    Label(reason.message, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.secondary)
                case .failed(let failure):
                    Label(failure.message, systemImage: "xmark.octagon")
                        .foregroundStyle(.red)
                }
            }
            Section {
                Button("Remove Account", role: .destructive) { Task { await model.remove() } }
                    .disabled(!model.canRemove)
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Remove Account")
    }
}

#if DEBUG
#Preview("Repair — unavailable") {
    RepairView(model: RepairViewModel(backend: InMemoryCompanionBackend()))
}

#Preview("Removal") {
    AccountRemovalView(
        model: AccountRemovalViewModel(
            backend: InMemoryCompanionBackend(), accountLabel: "Preview account"))
}
#endif
