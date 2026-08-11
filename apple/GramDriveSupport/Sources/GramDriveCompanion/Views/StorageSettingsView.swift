import GramDriveAgentCore
import SwiftUI

/// The storage & offline settings screen: managed-cache quota, per-account
/// retention and Archive Mode, plus launch-at-login (PLAT-MAC-005). Each
/// Archive toggle is gated by the storage preflight before it is sent.
public struct StorageSettingsView: View {
  @Bindable private var model: CompanionSettingsViewModel
  @Bindable private var contentPolicies: ContentPolicySettingsViewModel
  /// The engine will estimate the account's mirror size; until that field
  /// is wired, the preflight runs against this explicit, labeled estimate.
  @State private var estimatedArchiveGigabytes: Double = 25

  public init(
    model: CompanionSettingsViewModel,
    contentPolicies: ContentPolicySettingsViewModel
  ) {
    self.model = model
    self.contentPolicies = contentPolicies
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
        Text(
          "Unpinned content is evicted to stay under the quota. Pinned content is kept and counted."
        )
        .font(.callout)
        .foregroundStyle(.secondary)
      }

      Section("Retention & Archive") {
        Text(
          "Mirror or Audit controls what happens to observed history. "
            + "Archive Mode is a separate eager-byte setting."
        )
        .font(.callout)
        .foregroundStyle(.secondary)
        if contentPolicies.accounts.isEmpty {
          ContentUnavailableView(
            "No authorized account",
            systemImage: "person.crop.circle.badge.exclamationmark",
            description: Text(
              "Sign in, then refresh. Policy state is read from the agent and is not inferred from local settings."
            ))
        }
        ForEach(contentPolicies.accounts) { account in
          AccountContentPolicySettings(
            account: account,
            model: contentPolicies,
            estimatedArchiveBytes: estimatedArchiveBytes)
        }
        LabeledContent("Estimated allowed Archive size") {
          Text(formattedBytes(estimatedArchiveBytes))
        }
        Stepper(
          "\(Int(estimatedArchiveGigabytes.rounded())) GB estimate",
          value: $estimatedArchiveGigabytes,
          in: 0...5000,
          step: 5)
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
            systemImage: "hand.raised"
          )
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
    .sheet(
      item: $contentPolicies.auditToMirrorPrompt,
      onDismiss: { contentPolicies.dismissAuditToMirrorPrompt() },
      content: { prompt in
        AuditToMirrorConfirmationSheet(prompt: prompt, model: contentPolicies)
      })
  }

  @ViewBuilder
  private var preflightRow: some View {
    let preflight = model.archiveModePreflight(estimatedArchiveBytes: estimatedArchiveBytes)
    switch preflight {
    case .ok(let projected, let available):
      let free = available.map { " · \(formattedBytes($0)) free" } ?? ""
      Label(
        "Projected: \(formattedBytes(projected))\(free)",
        systemImage: "internaldrive"
      )
      .font(.callout)
      .foregroundStyle(.secondary)
    case .lowDisk(let projected, let available):
      Label(
        "Low disk: \(formattedBytes(projected)) needed, \(formattedBytes(available)) free.",
        systemImage: "exclamationmark.triangle.fill"
      )
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
    let policies = ContentPolicySettingsViewModel(
      backend: backend, diskProbe: FixedDiskSpaceProbe(available: 40_000_000_000))
    return StorageSettingsView(model: model, contentPolicies: policies)
  }
#endif

private struct AccountContentPolicySettings: View {
  let account: AccountContentPolicyState
  @Bindable var model: ContentPolicySettingsViewModel
  let estimatedArchiveBytes: UInt64

  var body: some View {
    VStack(alignment: .leading, spacing: 10) {
      Text(account.displayName)
        .font(.headline)
      if let status = account.status {
        Picker(
          "Retention",
          selection: Binding(
            get: { status.retention },
            set: { target in
              Task {
                await model.requestRetention(
                  accountId: account.accountId,
                  target: target)
              }
            })
        ) {
          Text("Mirror").tag(ControlRetentionMode.mirror)
          Text("Audit").tag(ControlRetentionMode.audit)
        }
        .pickerStyle(.segmented)
        .disabled(account.retentionRequest?.disablesControl == true)

        Text(status.retention.explanation)
          .font(.callout)
          .foregroundStyle(.secondary)
        if status.retention == .mirror {
          Text(
            "Switching to Audit is prospective. It cannot recover past deletions, unseen revisions, or content Telegram forbids GramDrive to save."
          )
          .font(.callout)
          .foregroundStyle(.secondary)
        }
        if let request = account.retentionRequest {
          RetentionRequestLabel(request: request)
        }

        Divider()

        Toggle(
          "Archive Mode",
          isOn: Binding(
            get: { status.archiveModeEnabled },
            set: { enabled in
              Task {
                await model.setArchiveMode(
                  accountId: account.accountId,
                  enabled: enabled,
                  estimatedArchiveBytes: estimatedArchiveBytes)
              }
            })
        )
        .disabled(account.archiveRequest != nil)
        Text(status.archiveProgressMessage)
          .font(.callout)
          .foregroundStyle(.secondary)
        Text(status.restrictionsMessage)
          .font(.callout)
          .foregroundStyle(.secondary)

        if status.pendingFilePurges > 0 {
          Label(
            "\(status.pendingFilePurges) retained-byte "
              + "\(status.pendingFilePurges == 1 ? "purge remains" : "purges remain").",
            systemImage: "trash"
          )
          .font(.callout)
          .foregroundStyle(.orange)
          Button("Resume purge") {
            Task {
              await model.resumeRetentionPurge(accountId: account.accountId)
            }
          }
        }
        if let transition = account.lastRetentionTransition,
          transition.current == .mirror
        {
          Text(
            "Last purge: \(transition.purgedRevisions) revisions, "
              + "\(transition.purgedDeletedMetadata) deleted metadata records, "
              + "\(transition.purgedRetainedBytes) retained byte owners."
          )
          .font(.caption)
          .foregroundStyle(.secondary)
        }
      } else if case .loading = account.availability {
        ProgressView("Reading committed policy…")
          .controlSize(.small)
      } else if let message = account.availability.message {
        Label(message, systemImage: "exclamationmark.triangle")
          .foregroundStyle(.orange)
      }

      if let message = account.lastMessage {
        Text(message)
          .font(.callout)
          .foregroundStyle(.secondary)
      }
    }
    .accessibilityElement(children: .contain)
    .padding(.vertical, 6)
  }
}

private struct RetentionRequestLabel: View {
  let request: RetentionRequestState

  var body: some View {
    switch request {
    case .awaitingConfirmation:
      Label("Audit-to-Mirror is awaiting destructive confirmation.", systemImage: "lock")
        .foregroundStyle(.orange)
    case .requesting(let target):
      Label("Requesting \(target.label)…", systemImage: "arrow.triangle.2.circlepath")
        .foregroundStyle(.secondary)
    case .reconciling(let target):
      Label(
        "Connection dropped while requesting \(target.label); rereading committed state.",
        systemImage: "arrow.clockwise"
      )
      .foregroundStyle(.orange)
    case .failed(_, let message):
      Label(message, systemImage: "exclamationmark.triangle.fill")
        .foregroundStyle(.red)
    }
  }
}

private struct AuditToMirrorConfirmationSheet: View {
  @Environment(\.dismiss) private var dismiss
  let prompt: AuditToMirrorPrompt
  let model: ContentPolicySettingsViewModel
  @State private var typedPhrase = ""

  var body: some View {
    VStack(alignment: .leading, spacing: 16) {
      Text("Purge Audit history for \(prompt.displayName)?")
        .font(.title2.bold())
      Text(prompt.destructiveSummary)
      Text("Type this exact phrase:")
        .font(.headline)
      Text(prompt.requiredPhrase)
        .font(.system(.body, design: .monospaced))
        .textSelection(.enabled)
      TextField("Confirmation phrase", text: $typedPhrase)
        .textFieldStyle(.roundedBorder)
        .accessibilityLabel("Destructive confirmation phrase")
      HStack {
        Spacer()
        Button("Cancel", role: .cancel) {
          model.cancelAuditToMirror()
          dismiss()
        }
        Button("Purge and switch to Mirror", role: .destructive) {
          let typed = typedPhrase
          Task {
            await model.confirmAuditToMirror(
              accountId: prompt.accountId,
              typedPhrase: typed)
          }
        }
        .disabled(typedPhrase != prompt.requiredPhrase)
        .keyboardShortcut(.defaultAction)
      }
    }
    .padding(24)
    .frame(width: 520)
  }
}
