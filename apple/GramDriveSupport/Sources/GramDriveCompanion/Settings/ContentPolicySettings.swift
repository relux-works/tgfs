import Foundation
import GramDriveAgentCore

/// One authorized account's policy state as the companion currently knows it.
public struct AccountContentPolicyState: Identifiable, Equatable, Sendable {
  public var id: Int64 { accountId }
  public var accountId: Int64
  public var displayName: String
  public var availability: ContentPolicyAvailability
  public var retentionRequest: RetentionRequestState?
  public var archiveRequest: Bool?
  public var lastRetentionTransition: ControlRetentionTransition?
  public var lastMessage: String?

  public init(
    accountId: Int64,
    displayName: String,
    availability: ContentPolicyAvailability = .loading
  ) {
    self.accountId = accountId
    self.displayName = displayName
    self.availability = availability
  }

  public var status: ControlContentPolicyStatus? {
    guard case .available(let status) = availability else { return nil }
    return status
  }
}

public enum ContentPolicyAvailability: Equatable, Sendable {
  case loading
  case available(ControlContentPolicyStatus)
  case unavailable(ControlChannelUnavailable)
  case failed(CommandFailure)

  public var message: String? {
    switch self {
    case .loading, .available: nil
    case .unavailable(let reason): reason.message
    case .failed(let failure): failure.message
    }
  }
}

/// Keeps committed state separate from a requested transition.
public enum RetentionRequestState: Equatable, Sendable {
  case awaitingConfirmation(ControlRetentionMode)
  case requesting(ControlRetentionMode)
  /// The channel dropped after submission. The current mode stays whatever
  /// the last committed status said until a read reconciles it.
  case reconciling(ControlRetentionMode)
  case failed(ControlRetentionMode, String)

  public var target: ControlRetentionMode {
    switch self {
    case .awaitingConfirmation(let target),
      .requesting(let target),
      .reconciling(let target),
      .failed(let target, _):
      target
    }
  }

  /// Only an in-flight or confirmation-gated request locks the picker.
  /// A terminal command failure is retryable without relaunching the app.
  public var disablesControl: Bool {
    switch self {
    case .awaitingConfirmation, .requesting, .reconciling:
      true
    case .failed:
      false
    }
  }
}

/// Item-driven destructive confirmation sheet state.
public struct AuditToMirrorPrompt: Identifiable, Equatable, Sendable {
  public var id: Int64 { accountId }
  public var accountId: Int64
  public var displayName: String
  public var requiredPhrase: String

  public init(accountId: Int64, displayName: String, requiredPhrase: String) {
    self.accountId = accountId
    self.displayName = displayName
    self.requiredPhrase = requiredPhrase
  }

  public var destructiveSummary: String {
    "Switching to Mirror permanently purges retained message revisions, "
      + "deleted attachment and profile-story metadata, and retained Audit bytes. "
      + "It cannot be undone."
  }
}

/// Per-account retention and independent Archive controls over the existing
/// typed agent control channel.
@MainActor
@Observable
public final class ContentPolicySettingsViewModel {
  public private(set) var accounts: [AccountContentPolicyState] = []
  public var auditToMirrorPrompt: AuditToMirrorPrompt?

  private let backend: any CompanionBackend
  private let diskProbe: any DiskSpaceProbe
  private let lowDiskBufferBytes: UInt64

  public init(
    backend: any CompanionBackend,
    diskProbe: any DiskSpaceProbe,
    lowDiskBufferBytes: UInt64 = 2_000_000_000
  ) {
    self.backend = backend
    self.diskProbe = diskProbe
    self.lowDiskBufferBytes = lowDiskBufferBytes
  }

  /// Refreshes all and only authorized accounts. Each read remains
  /// account-scoped so one unavailable account cannot overwrite another.
  public func refresh(from readout: HealthReadout) async {
    guard case .running(let snapshot) = readout, let reported = snapshot.accounts else {
      accounts = []
      return
    }
    let authorized = reported.filter { $0.authState == "authorized" }
    let previous = Dictionary(uniqueKeysWithValues: accounts.map { ($0.accountId, $0) })
    accounts = authorized.map { account in
      var state =
        previous[account.accountId]
        ?? AccountContentPolicyState(
          accountId: account.accountId,
          displayName: account.displayName)
      state.displayName = account.displayName
      return state
    }

    let backend = self.backend
    await withTaskGroup(
      of: (Int64, PolicyOutcome<ControlContentPolicyStatus>).self
    ) { group in
      for account in authorized {
        group.addTask {
          (
            account.accountId,
            await backend.fetchContentPolicy(accountId: account.accountId)
          )
        }
      }
      for await (accountId, outcome) in group {
        apply(outcome, to: accountId)
      }
    }
  }

  public func requestRetention(
    accountId: Int64,
    target: ControlRetentionMode
  ) async {
    guard let index = index(of: accountId), let status = accounts[index].status else {
      return
    }
    guard status.retention != target else { return }
    if status.retention == .audit, target == .mirror {
      guard !status.auditToMirrorConfirmationPhrase.isEmpty else {
        accounts[index].retentionRequest = .failed(
          .mirror,
          "The agent did not provide a destructive confirmation phrase. "
            + "Mirror was not requested.")
        accounts[index].lastMessage =
          "The agent did not provide a destructive confirmation phrase. "
          + "Mirror was not requested."
        return
      }
      accounts[index].retentionRequest = .awaitingConfirmation(.mirror)
      auditToMirrorPrompt = AuditToMirrorPrompt(
        accountId: accountId,
        displayName: accounts[index].displayName,
        requiredPhrase: status.auditToMirrorConfirmationPhrase)
      return
    }
    await performRetention(accountId: accountId, target: target, typedConfirmation: nil)
  }

  /// Cancelling the destructive sheet is deliberately a local no-op.
  public func cancelAuditToMirror() {
    dismissAuditToMirrorPrompt()
  }

  /// SwiftUI clears an item-driven sheet binding before calling `onDismiss`.
  /// Clear the matching local confirmation gate even when the prompt is
  /// already nil; no agent command is sent.
  public func dismissAuditToMirrorPrompt() {
    let promptedAccountId = auditToMirrorPrompt?.accountId
    auditToMirrorPrompt = nil
    for index in accounts.indices
    where promptedAccountId == nil || accounts[index].accountId == promptedAccountId {
      if case .awaitingConfirmation = accounts[index].retentionRequest {
        accounts[index].retentionRequest = nil
      }
    }
  }

  /// Submits exactly one destructive command when the text exactly matches
  /// the engine-provided account phrase.
  public func confirmAuditToMirror(accountId: Int64, typedPhrase: String) async {
    guard let prompt = auditToMirrorPrompt,
      prompt.accountId == accountId,
      !typedPhrase.isEmpty,
      typedPhrase == prompt.requiredPhrase
    else {
      return
    }
    auditToMirrorPrompt = nil
    await performRetention(
      accountId: accountId,
      target: .mirror,
      typedConfirmation: typedPhrase)
  }

  public func setArchiveMode(
    accountId: Int64,
    enabled: Bool,
    estimatedArchiveBytes: UInt64
  ) async {
    guard let index = index(of: accountId), accounts[index].archiveRequest == nil else {
      return
    }
    if enabled {
      let preflight = archiveModePreflight(estimatedArchiveBytes: estimatedArchiveBytes)
      guard !preflight.isLowDisk else {
        accounts[index].lastMessage =
          "Archive Mode was not enabled because the projected allowed content "
          + "does not fit with the required disk reserve."
        return
      }
    }
    accounts[index].archiveRequest = enabled
    let outcome = await backend.setArchiveMode(accountId: accountId, enabled: enabled)
    guard let current = self.index(of: accountId) else { return }
    switch outcome {
    case .value(let transition):
      accounts[current].archiveRequest = nil
      accounts[current].availability = .available(transition.status)
      accounts[current].lastMessage =
        transition.current
        ? "Archive Mode is on for allowed persistent content."
        : "Archive Mode is off. Retention mode did not change."
    case .unavailable(let reason):
      await reconcileArchiveAfterAmbiguousCommand(
        accountId: accountId,
        mutationMessage: reason.message)
    case .failed(let failure):
      await reconcileArchiveAfterAmbiguousCommand(
        accountId: accountId,
        mutationMessage: failure.message)
    }
  }

  public func resumeRetentionPurge(accountId: Int64) async {
    guard let index = index(of: accountId), accounts[index].status != nil else { return }
    accounts[index].lastMessage = "Resuming the retained-byte purge…"
    switch await backend.resumeRetentionPurge(accountId: accountId) {
    case .value(let resume):
      guard let current = self.index(of: accountId) else { return }
      accounts[current].availability = .available(resume.status)
      accounts[current].lastMessage =
        resume.status.pendingFilePurges == 0
        ? "The retained-byte purge is current."
        : "\(resume.status.pendingFilePurges) file purge "
          + "\(resume.status.pendingFilePurges == 1 ? "item remains" : "items remain")."
    case .unavailable(let reason):
      accounts[index].lastMessage = reason.message
    case .failed(let failure):
      accounts[index].lastMessage = failure.message
    }
  }

  public func archiveModePreflight(estimatedArchiveBytes: UInt64) -> ArchiveModePreflight {
    let available = diskProbe.availableCapacityBytes()
    guard let available else {
      return .ok(projectedBytes: estimatedArchiveBytes, availableBytes: nil)
    }
    let needed = estimatedArchiveBytes.addingReportingOverflow(lowDiskBufferBytes)
    return needed.overflow || needed.partialValue > available
      ? .lowDisk(projectedBytes: estimatedArchiveBytes, availableBytes: available)
      : .ok(projectedBytes: estimatedArchiveBytes, availableBytes: available)
  }

  private func performRetention(
    accountId: Int64,
    target: ControlRetentionMode,
    typedConfirmation: String?
  ) async {
    guard let index = index(of: accountId) else { return }
    if case .requesting = accounts[index].retentionRequest { return }
    accounts[index].retentionRequest = .requesting(target)
    let outcome = await backend.setRetention(
      accountId: accountId,
      target: target,
      typedConfirmation: typedConfirmation)
    guard let current = self.index(of: accountId) else { return }
    switch outcome {
    case .value(let transition):
      accounts[current].availability = .available(transition.status)
      accounts[current].retentionRequest = nil
      accounts[current].lastRetentionTransition = transition
      accounts[current].lastMessage = Self.transitionMessage(transition)
    case .failed(let failure):
      accounts[current].retentionRequest = .reconciling(target)
      accounts[current].lastMessage =
        failure.message
        + " Re-reading committed engine state without replaying the request."
      await reconcileAfterFailedCommand(
        accountId: accountId,
        target: target,
        failureMessage: failure.message)
    case .unavailable(let reason):
      accounts[current].retentionRequest = .reconciling(target)
      accounts[current].lastMessage =
        reason.message
        + " Refreshing committed engine state; the request will not be replayed."
      await reconcileAfterDroppedCommand(accountId: accountId, target: target)
    }
  }

  private func reconcileAfterDroppedCommand(
    accountId: Int64,
    target: ControlRetentionMode
  ) async {
    let outcome = await backend.fetchContentPolicy(accountId: accountId)
    guard let index = index(of: accountId) else { return }
    switch outcome {
    case .value(let status):
      accounts[index].availability = .available(status)
      accounts[index].retentionRequest = nil
      accounts[index].lastMessage = Self.reconciliationMessage(
        status: status,
        target: target)
    case .unavailable(let reason):
      accounts[index].lastMessage = reason.message
    case .failed(let failure):
      accounts[index].lastMessage =
        failure.message
        + " Committed engine state is still unknown; refresh will reconcile it without replay."
    }
  }

  /// A typed mutation failure can still arrive after the engine committed the
  /// policy and a fallible cleanup/status step failed. Re-read state, but keep
  /// the actionable failure visible and never replay the destructive command.
  private func reconcileAfterFailedCommand(
    accountId: Int64,
    target: ControlRetentionMode,
    failureMessage: String
  ) async {
    let outcome = await backend.fetchContentPolicy(accountId: accountId)
    guard let index = index(of: accountId) else { return }
    accounts[index].retentionRequest = .failed(target, failureMessage)
    switch outcome {
    case .value(let status):
      accounts[index].availability = .available(status)
      accounts[index].lastMessage =
        failureMessage + " "
        + Self.failedMutationReconciliationMessage(status: status, target: target)
    case .unavailable(let reason):
      accounts[index].lastMessage =
        failureMessage
        + " The status re-read was unavailable: \(reason.message) "
        + "Committed engine state is still unknown; refresh will reconcile it without replay."
    case .failed(let failure):
      accounts[index].lastMessage =
        failureMessage
        + " The status re-read failed: \(failure.message) "
        + "Committed engine state is still unknown; refresh will reconcile it without replay."
    }
  }

  /// Archive transactions and their response/status projection can fail at
  /// different points. Resolve the ambiguity with a status read only.
  private func reconcileArchiveAfterAmbiguousCommand(
    accountId: Int64,
    mutationMessage: String
  ) async {
    let outcome = await backend.fetchContentPolicy(accountId: accountId)
    guard let index = index(of: accountId) else { return }
    accounts[index].archiveRequest = nil
    switch outcome {
    case .value(let status):
      accounts[index].availability = .available(status)
      accounts[index].lastMessage =
        mutationMessage
        + " Archive Mode state was re-read without replaying the request; "
        + "the agent reports \(status.archiveModeEnabled ? "on" : "off")."
    case .unavailable(let reason):
      accounts[index].lastMessage =
        mutationMessage
        + " The status re-read was unavailable: \(reason.message) "
        + "Committed Archive Mode state is still unknown; refresh will reconcile it without replay."
    case .failed(let failure):
      accounts[index].lastMessage =
        mutationMessage
        + " The status re-read failed: \(failure.message) "
        + "Committed Archive Mode state is still unknown; refresh will reconcile it without replay."
    }
  }

  private func apply(
    _ outcome: PolicyOutcome<ControlContentPolicyStatus>,
    to accountId: Int64
  ) {
    guard let index = index(of: accountId) else { return }
    switch outcome {
    case .value(let status):
      accounts[index].availability = .available(status)
      if case .reconciling(let target) = accounts[index].retentionRequest {
        accounts[index].retentionRequest = nil
        accounts[index].lastMessage = Self.reconciliationMessage(
          status: status,
          target: target)
      }
    case .unavailable(let reason):
      accounts[index].availability = .unavailable(reason)
    case .failed(let failure):
      accounts[index].availability = .failed(failure)
    }
  }

  private func index(of accountId: Int64) -> Int? {
    accounts.firstIndex { $0.accountId == accountId }
  }

  private static func transitionMessage(_ transition: ControlRetentionTransition) -> String {
    guard transition.previous != transition.current else {
      return "\(transition.current.label) was already active."
    }
    if transition.current == .audit {
      return
        "Audit now retains future allowed observations. It did not recover past or unseen content."
    }
    return
      "Mirror is active. Purged \(transition.purgedRevisions) retained revisions, "
      + "\(transition.purgedDeletedMetadata) deleted metadata records, and "
      + "\(transition.purgedRetainedBytes) retained byte owners."
  }

  private static func reconciliationMessage(
    status: ControlContentPolicyStatus,
    target: ControlRetentionMode
  ) -> String {
    status.retention == target
      ? "The agent committed \(target.label). Current state was re-read after the connection dropped."
      : "The agent still reports \(status.retention.label). The destructive request was not replayed."
  }

  private static func failedMutationReconciliationMessage(
    status: ControlContentPolicyStatus,
    target: ControlRetentionMode
  ) -> String {
    guard status.retention == target else {
      return
        "The agent still reports \(status.retention.label) after a status re-read without replay. "
        + "The policy request can be retried."
    }
    guard target == .mirror, status.pendingFilePurges > 0 else {
      return
        "The agent reports \(target.label) after a status re-read without replay."
    }
    return
      "The agent reports Mirror after a status re-read without replay. "
      + "\(status.pendingFilePurges) retained-byte purge "
      + "\(status.pendingFilePurges == 1 ? "item remains" : "items remain") resumable."
  }
}

extension ControlRetentionMode {
  public var label: String {
    switch self {
    case .mirror: "Mirror"
    case .audit: "Audit"
    }
  }

  public var explanation: String {
    switch self {
    case .mirror:
      "Reflects current observed Telegram state. Observed edits replace content and observed deletions purge it."
    case .audit:
      "Prospectively retains allowed observations and already materialized allowed bytes. It cannot recover deleted-before-observation content or unseen revisions."
    }
  }
}

extension ControlContentPolicyStatus {
  public var archiveProgressMessage: String {
    guard archiveModeEnabled else {
      return "Off — no eager Archive backfill is requested."
    }
    if let category = archiveBackfill.failureCategory {
      var details = ["Archive backfill failed (\(category))."]
      if let failed = archiveBackfill.failedAllowedItems {
        details.append(
          "\(failed) allowed \(failed == 1 ? "item has" : "items have") failed.")
      }
      if let pending = archiveBackfill.pendingAllowedItems {
        details.append(
          pending == 0
            ? "No allowed persistent items are waiting for backfill."
            : "\(pending) allowed persistent \(pending == 1 ? "item is" : "items are") "
              + "waiting for backfill and remain resumable.")
      } else {
        details.append("The agent did not report a pending allowed-item count.")
      }
      return details.joined(separator: " ")
    }
    if let failed = archiveBackfill.failedAllowedItems, failed > 0 {
      return "\(failed) allowed \(failed == 1 ? "item has" : "items have") failed and can retry."
    }
    if let pending = archiveBackfill.pendingAllowedItems {
      return pending == 0
        ? "Known allowed persistent content is current."
        : "\(pending) allowed persistent \(pending == 1 ? "item is" : "items are") waiting for backfill."
    }
    return "On — this agent has not reported an Archive backfill count."
  }

  public var restrictionsMessage: String {
    "Protected, view-once, self-destruct, secret-chat, ephemeral active-story, unseen, and otherwise forbidden content is not archived."
  }
}
