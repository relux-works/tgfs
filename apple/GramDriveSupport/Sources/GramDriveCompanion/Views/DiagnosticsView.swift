import GramDriveAgentCore
import SwiftUI

/// The diagnostics screen: the agent's health snapshot, rendered field by
/// field with honest gaps where the engine has not wired a reading. Redacted
/// by construction — the snapshot carries codes and versions, never account
/// material.
public struct DiagnosticsView: View {
    private let model: CompanionStatusViewModel

    public init(model: CompanionStatusViewModel) {
        self.model = model
    }

    public var body: some View {
        Form {
            switch model.readout {
            case .running(let snapshot):
                report(DiagnosticsReport(snapshot: snapshot))
            case .notRunning:
                Section { Text("The agent is not running.") }
            case .timedOut:
                Section { Text("The agent did not respond in time.") }
            case .error(let detail):
                Section {
                    Label(detail, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.secondary)
                }
            }
            Section {
                Button("Refresh") { Task { await model.refresh() } }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Diagnostics")
    }

    @ViewBuilder
    private func report(_ report: DiagnosticsReport) -> some View {
        Section("Agent") {
            LabeledContent("State", value: report.runState.rawValue)
            LabeledContent("Agent version", value: report.agentVersion)
            LabeledContent("FFI contract", value: report.contractVersion)
            LabeledContent("PID", value: String(report.pid))
            LabeledContent("Started", value: report.startedAt.formatted())
            LabeledContent("Pending transfers", value: String(report.pendingTransferCount))
        }
        Section("Shared state") {
            optionalRow("Schema version", report.stateSchemaVersion.map(String.init))
            optionalRow("Data version", report.dataVersion.map(String.init))
            optionalRow("Cache pressure", report.cachePressure)
            optionalRow("Provider domain", report.providerRegistrationState)
        }
        Section("Power") {
            optionalRow("Last sleep", report.lastSleep?.formatted())
            optionalRow("Last wake", report.lastWake?.formatted())
        }
        Section("Recent events") {
            if report.recentEvents.isEmpty {
                Text("None").foregroundStyle(.secondary)
            } else {
                ForEach(Array(report.recentEvents.enumerated()), id: \.offset) { _, event in
                    Text(event).font(.callout.monospaced())
                }
            }
        }
    }

    @ViewBuilder
    private func optionalRow(_ label: String, _ value: String?) -> some View {
        LabeledContent(label, value: value ?? "Not reported yet")
    }
}

#if DEBUG
#Preview("Diagnostics") {
    let backend = InMemoryCompanionBackend(health: .running(previewSnapshot()))
    let model = CompanionStatusViewModel(backend: backend)
    return DiagnosticsView(model: model).task { await model.refresh() }
}
#endif
