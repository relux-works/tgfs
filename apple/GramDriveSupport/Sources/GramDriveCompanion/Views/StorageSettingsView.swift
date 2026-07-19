import SwiftUI

/// The storage & offline settings screen: the managed-cache quota and global
/// Archive Mode (POL-2), plus launch-at-login (PLAT-MAC-005). Archive Mode is
/// gated behind the POL-2 pre-enable check — projected disk usage and a
/// low-disk warning — before it is switched on.
public struct StorageSettingsView: View {
    @Bindable private var model: CompanionSettingsViewModel
    /// The engine will estimate the account's mirror size; until that field
    /// is wired, the preflight runs against this explicit, labeled estimate.
    @State private var estimatedArchiveGigabytes: Double = 25

    public init(model: CompanionSettingsViewModel) {
        self.model = model
    }

    private var estimatedArchiveBytes: UInt64 {
        UInt64((max(0, estimatedArchiveGigabytes) * 1_000_000_000).rounded())
    }

    public var body: some View {
        Form {
            Section("Managed cache") {
                LabeledContent("Quota") {
                    Text(formattedBytes(model.cacheQuotaBytes))
                }
                Stepper(
                    "\(Int(model.cacheQuotaGigabytes.rounded())) GB",
                    value: $model.cacheQuotaGigabytes, in: 1...1000, step: 1)
                Text("Unpinned content is evicted to stay under the quota. Pinned content is kept and counted.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }

            Section("Archive Mode") {
                Toggle("Download everything (Archive Mode)", isOn: $model.archiveModeEnabled)
                Text("Mirrors this account eagerly and keeps it offline, exempt from the cache quota.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                LabeledContent("Estimated size") {
                    Text(formattedBytes(estimatedArchiveBytes))
                }
                Stepper(
                    "\(Int(estimatedArchiveGigabytes.rounded())) GB estimate",
                    value: $estimatedArchiveGigabytes, in: 0...5000, step: 5)
                preflightRow
            }

            Section("Startup") {
                Toggle(
                    "Launch GramDrive at login",
                    isOn: Binding(
                        get: { model.launchAtLogin },
                        set: { model.applyLaunchAtLogin($0) }))
                if let action = model.lastLaunchAction, action == .awaitingApproval {
                    Label(
                        "Approve GramDrive in System Settings › General › Login Items.",
                        systemImage: "hand.raised")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                }
            }

            if let error = model.lastError {
                Section {
                    Label(error, systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.red)
                }
            }

            Section {
                Button("Save") { model.save() }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Storage & Offline")
        .onAppear { model.load() }
    }

    @ViewBuilder
    private var preflightRow: some View {
        let preflight = model.archiveModePreflight(estimatedArchiveBytes: estimatedArchiveBytes)
        switch preflight {
        case .ok(let projected, let available):
            let free = available.map { " · \(formattedBytes($0)) free" } ?? ""
            Label(
                "Projected: \(formattedBytes(projected))\(free)",
                systemImage: "internaldrive")
            .font(.callout)
            .foregroundStyle(.secondary)
        case .lowDisk(let projected, let available):
            Label(
                "Low disk: \(formattedBytes(projected)) needed, \(formattedBytes(available)) free.",
                systemImage: "exclamationmark.triangle.fill")
            .font(.callout)
            .foregroundStyle(.orange)
        }
    }
}

#if DEBUG
#Preview("Storage settings") {
    let backend = InMemoryCompanionBackend()
    let model = CompanionSettingsViewModel(
        backend: backend, diskProbe: FixedDiskSpaceProbe(available: 40_000_000_000))
    return StorageSettingsView(model: model)
}
#endif
