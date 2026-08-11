import Foundation
import GramDriveAgentCore
import Testing

@testable import GramDriveCompanion

@MainActor
@Suite struct ContentPolicySettingsTests {
  private func status(
    accountId: Int64,
    retention: ControlRetentionMode = .mirror,
    archiveModeEnabled: Bool = false,
    pendingFilePurges: UInt64 = 0,
    archiveBackfill: ControlArchiveBackfillProgress = ControlArchiveBackfillProgress()
  ) -> ControlContentPolicyStatus {
    ControlContentPolicyStatus(
      accountId: accountId,
      retention: retention,
      archiveModeEnabled: archiveModeEnabled,
      pendingFilePurges: pendingFilePurges,
      auditToMirrorConfirmationPhrase: "PURGE ACCOUNT \(accountId) AUDIT HISTORY",
      archiveBackfill: archiveBackfill)
  }

  private func readout(_ accounts: AccountHealthSummary...) -> HealthReadout {
    .running(previewSnapshot(accounts: accounts))
  }

  private func model(
    backend: any CompanionBackend,
    availableBytes: UInt64? = 500_000_000_000
  ) -> ContentPolicySettingsViewModel {
    ContentPolicySettingsViewModel(
      backend: backend,
      diskProbe: FixedDiskSpaceProbe(available: availableBytes))
  }

  @Test func authorizedAccountsDefaultToMirrorAndStayIsolated() async {
    let backend = InMemoryCompanionBackend(
      policyStatuses: [
        11: status(accountId: 11),
        22: status(accountId: 22),
      ])
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized"),
        AccountHealthSummary(accountId: 22, displayName: "Two", authState: "authorized"),
        AccountHealthSummary(accountId: 33, displayName: "Signed out", authState: "closed")
      ))

    #expect(vm.accounts.map(\.accountId) == [11, 22])
    #expect(vm.accounts.allSatisfy { $0.status?.retention == .mirror })
    #expect(vm.accounts.allSatisfy { $0.status?.archiveModeEnabled == false })

    await vm.requestRetention(accountId: 11, target: .audit)

    #expect(vm.accounts.first { $0.accountId == 11 }?.status?.retention == .audit)
    #expect(vm.accounts.first { $0.accountId == 22 }?.status?.retention == .mirror)
    #expect(
      backend.recordedPolicyCommands
        == [.setRetention(accountId: 11, target: .audit, typedConfirmation: nil)])
  }

  @Test func mirrorToAuditIsProspectiveAndSurvivesViewModelRelaunch() async {
    let backend = InMemoryCompanionBackend(policyStatuses: [11: status(accountId: 11)])
    let health = readout(
      AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized"))
    let first = model(backend: backend)
    await first.refresh(from: health)
    await first.requestRetention(accountId: 11, target: .audit)

    #expect(first.accounts[0].lastMessage?.contains("future allowed observations") == true)
    #expect(first.accounts[0].lastMessage?.contains("did not recover past or unseen") == true)

    let relaunched = model(backend: backend)
    await relaunched.refresh(from: health)
    #expect(relaunched.accounts[0].status?.retention == .audit)
    #expect(relaunched.accounts[0].status?.archiveModeEnabled == false)
  }

  @Test func destructiveCancelAndWrongTextAreNoOpsAndExactConfirmSendsOnce() async throws {
    let policy = status(accountId: 11, retention: .audit, archiveModeEnabled: true)
    let backend = InMemoryCompanionBackend(policyStatuses: [11: policy])
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized")))

    await vm.requestRetention(accountId: 11, target: .mirror)
    let firstPrompt = try #require(vm.auditToMirrorPrompt)
    #expect(firstPrompt.destructiveSummary.contains("message revisions"))
    #expect(firstPrompt.destructiveSummary.contains("deleted attachment"))
    #expect(firstPrompt.destructiveSummary.contains("profile-story metadata"))
    #expect(firstPrompt.destructiveSummary.contains("retained Audit bytes"))
    vm.cancelAuditToMirror()
    #expect(backend.recordedPolicyCommands.isEmpty)
    #expect(vm.accounts[0].status?.retention == .audit)
    #expect(vm.accounts[0].status?.archiveModeEnabled == true)

    await vm.requestRetention(accountId: 11, target: .mirror)
    await vm.confirmAuditToMirror(accountId: 11, typedPhrase: "almost")
    #expect(backend.recordedPolicyCommands.isEmpty)
    let exact = try #require(vm.auditToMirrorPrompt?.requiredPhrase)
    await vm.confirmAuditToMirror(accountId: 11, typedPhrase: exact)

    #expect(
      backend.recordedPolicyCommands
        == [
          .setRetention(
            accountId: 11,
            target: .mirror,
            typedConfirmation: "PURGE ACCOUNT 11 AUDIT HISTORY")
        ])
    #expect(vm.accounts[0].status?.retention == .mirror)
    #expect(vm.accounts[0].status?.archiveModeEnabled == true)
  }

  @Test func systemSheetDismissalClearsTheLocalGateWithoutACommand() async {
    let policy = status(accountId: 11, retention: .audit)
    let backend = InMemoryCompanionBackend(policyStatuses: [11: policy])
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized")))

    await vm.requestRetention(accountId: 11, target: .mirror)
    #expect(vm.accounts[0].retentionRequest?.disablesControl == true)

    // SwiftUI clears the item binding before invoking the sheet's onDismiss.
    vm.auditToMirrorPrompt = nil
    vm.dismissAuditToMirrorPrompt()

    #expect(vm.accounts[0].retentionRequest == nil)
    #expect(backend.recordedPolicyCommands.isEmpty)
    #expect(vm.accounts[0].status?.retention == .audit)
  }

  @Test func destructiveActionConsumesThePromptBeforeSheetDismissalAndSendsOnce() async throws {
    let policy = status(accountId: 11, retention: .audit)
    let backend = InMemoryCompanionBackend(policyStatuses: [11: policy])
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized")))

    await vm.requestRetention(accountId: 11, target: .mirror)
    let phrase = try #require(vm.auditToMirrorPrompt?.requiredPhrase)

    // This is the destructive button's order: confirmation consumes the
    // item-driven prompt, then SwiftUI delivers the sheet dismissal callback.
    await vm.confirmAuditToMirror(accountId: 11, typedPhrase: phrase)
    vm.dismissAuditToMirrorPrompt()
    vm.dismissAuditToMirrorPrompt()

    #expect(vm.auditToMirrorPrompt == nil)
    #expect(
      backend.recordedPolicyCommands
        == [
          .setRetention(
            accountId: 11,
            target: .mirror,
            typedConfirmation: "PURGE ACCOUNT 11 AUDIT HISTORY")
        ])
    #expect(vm.accounts[0].status?.retention == .mirror)
  }

  @Test func aDroppedDestructiveCommandReconcilesWithoutReplay() async throws {
    let initial = status(accountId: 11, retention: .audit)
    let committed = status(accountId: 11, retention: .mirror, pendingFilePurges: 2)
    let backend = DropAfterCommitPolicyBackend(initial: initial, committed: committed)
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized")))

    await vm.requestRetention(accountId: 11, target: .mirror)
    let phrase = try #require(vm.auditToMirrorPrompt?.requiredPhrase)
    await vm.confirmAuditToMirror(accountId: 11, typedPhrase: phrase)

    #expect(backend.retentionCallCount == 1)
    #expect(backend.fetchCallCount == 2)
    #expect(vm.accounts[0].status?.retention == .mirror)
    #expect(vm.accounts[0].status?.pendingFilePurges == 2)
    #expect(vm.accounts[0].lastMessage?.contains("re-read after the connection dropped") == true)
  }

  @Test func aFailedDestructiveCommandReconcilesCommittedMirrorWithoutReplay() async throws {
    let initial = status(accountId: 11, retention: .audit)
    let committed = status(accountId: 11, retention: .mirror, pendingFilePurges: 3)
    let backend = DropAfterCommitPolicyBackend(
      initial: initial,
      committed: committed,
      retentionOutcome: .failed(.storage))
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized")))

    await vm.requestRetention(accountId: 11, target: .mirror)
    let phrase = try #require(vm.auditToMirrorPrompt?.requiredPhrase)
    await vm.confirmAuditToMirror(accountId: 11, typedPhrase: phrase)

    #expect(backend.retentionCallCount == 1)
    #expect(backend.fetchCallCount == 2)
    #expect(vm.accounts[0].status?.retention == .mirror)
    #expect(vm.accounts[0].status?.pendingFilePurges == 3)
    #expect(
      vm.accounts[0].retentionRequest
        == .failed(.mirror, CommandFailure.storage.message))
    #expect(vm.accounts[0].retentionRequest?.disablesControl == false)
    #expect(vm.accounts[0].lastMessage?.contains(CommandFailure.storage.message) == true)
    #expect(vm.accounts[0].lastMessage?.contains("without replay") == true)
    #expect(
      vm.accounts[0].lastMessage?
        .contains("3 retained-byte purge items remain resumable") == true)
  }

  @Test func interruptedReconciliationSettlesOnALaterSuccessfulRefreshWithoutReplay() async throws {
    let initial = status(accountId: 11, retention: .audit)
    let committed = status(accountId: 11, retention: .mirror, pendingFilePurges: 2)
    let backend = InterruptedReconciliationPolicyBackend(
      initial: initial,
      committed: committed)
    let health = readout(
      AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized"))
    let vm = model(backend: backend)
    await vm.refresh(from: health)

    await vm.requestRetention(accountId: 11, target: .mirror)
    let phrase = try #require(vm.auditToMirrorPrompt?.requiredPhrase)
    await vm.confirmAuditToMirror(accountId: 11, typedPhrase: phrase)

    #expect(vm.accounts[0].retentionRequest == .reconciling(.mirror))
    #expect(vm.accounts[0].retentionRequest?.disablesControl == true)
    #expect(backend.retentionCallCount == 1)
    #expect(backend.fetchCallCount == 2)

    await vm.refresh(from: health)

    #expect(vm.accounts[0].retentionRequest == nil)
    #expect(vm.accounts[0].status?.retention == .mirror)
    #expect(vm.accounts[0].status?.pendingFilePurges == 2)
    #expect(backend.retentionCallCount == 1)
    #expect(backend.fetchCallCount == 3)
  }

  @Test func ordinaryRetentionFailureLeavesTheControlRetryable() async {
    let initial = status(accountId: 11)
    let committed = status(accountId: 11, retention: .audit)
    let backend = RetryableFailurePolicyBackend(initial: initial, committed: committed)
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized")))

    await vm.requestRetention(accountId: 11, target: .audit)

    #expect(vm.accounts[0].retentionRequest == .failed(.audit, CommandFailure.storage.message))
    #expect(vm.accounts[0].retentionRequest?.disablesControl == false)
    #expect(vm.accounts[0].status?.retention == .mirror)

    await vm.requestRetention(accountId: 11, target: .audit)

    #expect(vm.accounts[0].retentionRequest == nil)
    #expect(vm.accounts[0].status?.retention == .audit)
    #expect(backend.retentionCallCount == 2)
  }

  @Test func missingDestructivePhraseFailsClosed() async {
    var unsafeLegacyStatus = status(accountId: 11, retention: .audit)
    unsafeLegacyStatus.auditToMirrorConfirmationPhrase = ""
    let backend = InMemoryCompanionBackend(policyStatuses: [11: unsafeLegacyStatus])
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized")))

    await vm.requestRetention(accountId: 11, target: .mirror)

    #expect(vm.auditToMirrorPrompt == nil)
    #expect(backend.recordedPolicyCommands.isEmpty)
    #expect(vm.accounts[0].status?.retention == .audit)
    #expect(vm.accounts[0].lastMessage?.contains("Mirror was not requested") == true)
  }

  @Test func archiveModeIsSeparateAndLowDiskPreflightPreventsTheCommand() async {
    let backend = InMemoryCompanionBackend(
      policyStatuses: [11: status(accountId: 11, retention: .audit)])
    let health = readout(
      AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized"))
    let lowDisk = model(backend: backend, availableBytes: 20_000_000_000)
    await lowDisk.refresh(from: health)
    await lowDisk.setArchiveMode(
      accountId: 11,
      enabled: true,
      estimatedArchiveBytes: 25_000_000_000)

    #expect(backend.recordedPolicyCommands.isEmpty)
    #expect(lowDisk.accounts[0].status?.retention == .audit)
    #expect(lowDisk.accounts[0].status?.archiveModeEnabled == false)
    #expect(lowDisk.accounts[0].lastMessage?.contains("was not enabled") == true)

    let ample = model(backend: backend)
    await ample.refresh(from: health)
    await ample.setArchiveMode(
      accountId: 11,
      enabled: true,
      estimatedArchiveBytes: 25_000_000_000)
    #expect(
      backend.recordedPolicyCommands
        == [.setArchiveMode(accountId: 11, enabled: true)])
    #expect(ample.accounts[0].status?.retention == .audit)
    #expect(ample.accounts[0].status?.archiveModeEnabled == true)
  }

  @Test(arguments: [
    AmbiguousMutationOutcome.unavailable(.dropped),
    AmbiguousMutationOutcome.failed(.storage),
  ])
  fileprivate func ambiguousArchiveResultReconcilesCommittedStateWithoutReplay(
    outcome: AmbiguousMutationOutcome
  ) async {
    let initial = status(accountId: 11)
    let committed = status(
      accountId: 11,
      archiveModeEnabled: true,
      archiveBackfill: ControlArchiveBackfillProgress(
        pendingAllowedItems: 7,
        failedAllowedItems: 2,
        failureCategory: "storage"))
    let backend = DropAfterCommitPolicyBackend(
      initial: initial,
      committed: committed,
      archiveOutcome: outcome)
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized")))

    await vm.setArchiveMode(
      accountId: 11,
      enabled: true,
      estimatedArchiveBytes: 25_000_000_000)

    #expect(backend.archiveCallCount == 1)
    #expect(backend.fetchCallCount == 2)
    #expect(vm.accounts[0].status?.archiveModeEnabled == true)
    #expect(vm.accounts[0].status?.archiveBackfill.pendingAllowedItems == 7)
    #expect(vm.accounts[0].status?.archiveBackfill.failedAllowedItems == 2)
    #expect(vm.accounts[0].archiveRequest == nil)
    #expect(vm.accounts[0].lastMessage?.contains(outcome.message) == true)
    #expect(vm.accounts[0].lastMessage?.contains("re-read without replaying") == true)
  }

  @Test func progressFailuresRestrictionsAndPurgeResumeStayTruthful() async throws {
    let policy = status(
      accountId: 11,
      archiveModeEnabled: true,
      pendingFilePurges: 4,
      archiveBackfill: ControlArchiveBackfillProgress(
        pendingAllowedItems: 7,
        failedAllowedItems: 2,
        failureCategory: "storage"))
    let backend = InMemoryCompanionBackend(policyStatuses: [11: policy])
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized")))
    let actual = try #require(vm.accounts[0].status)

    #expect(actual.archiveProgressMessage.contains("failed (storage)"))
    #expect(actual.archiveProgressMessage.contains("2 allowed items have failed"))
    #expect(actual.archiveProgressMessage.contains("7 allowed persistent items are waiting"))
    #expect(actual.archiveProgressMessage.lowercased().contains("allowed"))
    #expect(actual.restrictionsMessage.contains("unseen"))
    #expect(actual.restrictionsMessage.contains("forbidden"))
    await vm.resumeRetentionPurge(accountId: 11)
    #expect(vm.accounts[0].status?.pendingFilePurges == 0)
    #expect(
      backend.recordedPolicyCommands
        == [.resumeRetentionPurge(accountId: 11)])
  }

  @Test func unavailableAndFailureStatesRemainPerAccount() async {
    let backend = UnavailablePolicyBackend(
      available: status(accountId: 11),
      unavailableAccountId: 22)
    let vm = model(backend: backend)
    await vm.refresh(
      from: readout(
        AccountHealthSummary(accountId: 11, displayName: "One", authState: "authorized"),
        AccountHealthSummary(accountId: 22, displayName: "Two", authState: "authorized"),
        AccountHealthSummary(accountId: 33, displayName: "Three", authState: "authorized")
      ))

    #expect(vm.accounts.first { $0.accountId == 11 }?.status?.retention == .mirror)
    #expect(
      vm.accounts.first { $0.accountId == 22 }?.availability
        == .unavailable(.agentNotRunning))
    #expect(vm.accounts.first { $0.accountId == 33 }?.availability == .failed(.storage))
  }

  @Test func missingArchiveCountsNeverClaimBackfillIsCurrent() {
    let policy = status(accountId: 11, archiveModeEnabled: true)
    #expect(policy.archiveProgressMessage.contains("not reported"))
    #expect(!policy.archiveProgressMessage.contains("current"))
  }
}

private enum AmbiguousMutationOutcome: Equatable, Sendable {
  case unavailable(ControlChannelUnavailable)
  case failed(CommandFailure)

  var message: String {
    switch self {
    case .unavailable(let reason): reason.message
    case .failed(let failure): failure.message
    }
  }
}

private final class DropAfterCommitPolicyBackend: CompanionBackend, @unchecked Sendable {
  private let lock = NSLock()
  private var current: ControlContentPolicyStatus
  private let committed: ControlContentPolicyStatus
  private let retentionOutcome: AmbiguousMutationOutcome
  private let archiveOutcome: AmbiguousMutationOutcome
  private var fetches = 0
  private var retentionCalls = 0
  private var archiveCalls = 0

  init(
    initial: ControlContentPolicyStatus,
    committed: ControlContentPolicyStatus,
    retentionOutcome: AmbiguousMutationOutcome = .unavailable(.dropped),
    archiveOutcome: AmbiguousMutationOutcome = .unavailable(.notWired)
  ) {
    current = initial
    self.committed = committed
    self.retentionOutcome = retentionOutcome
    self.archiveOutcome = archiveOutcome
  }

  var fetchCallCount: Int { lock.withContentPolicyLock { fetches } }
  var retentionCallCount: Int { lock.withContentPolicyLock { retentionCalls } }
  var archiveCallCount: Int { lock.withContentPolicyLock { archiveCalls } }

  func fetchHealth() async -> HealthReadout { .notRunning }
  func loadSettings() throws -> AgentSettings { AgentSettings() }
  func saveSettings(_: AgentSettings) throws {}
  func makeAuthorizationSession() -> any AuthorizationSession {
    UnavailableAuthorizationSession(reason: .notWired)
  }
  func requestRepair() async -> CommandOutcome { .unavailable(.notWired) }
  func removeAccount(_: RemovalConfirmation) async -> CommandOutcome {
    .unavailable(.notWired)
  }
  func fetchContentPolicy(
    accountId _: Int64
  ) async -> PolicyOutcome<ControlContentPolicyStatus> {
    lock.withContentPolicyLock {
      fetches += 1
      return .value(current)
    }
  }
  func setRetention(
    accountId _: Int64,
    target _: ControlRetentionMode,
    typedConfirmation _: String?
  ) async -> PolicyOutcome<ControlRetentionTransition> {
    lock.withContentPolicyLock {
      retentionCalls += 1
      current = committed
      switch retentionOutcome {
      case .unavailable(let reason): return .unavailable(reason)
      case .failed(let failure): return .failed(failure)
      }
    }
  }
  func setArchiveMode(
    accountId _: Int64,
    enabled _: Bool
  ) async -> PolicyOutcome<ControlArchiveModeTransition> {
    lock.withContentPolicyLock {
      archiveCalls += 1
      current = committed
      switch archiveOutcome {
      case .unavailable(let reason): return .unavailable(reason)
      case .failed(let failure): return .failed(failure)
      }
    }
  }
  func resumeRetentionPurge(
    accountId _: Int64
  ) async -> PolicyOutcome<ControlRetentionPurgeResume> {
    .unavailable(.notWired)
  }
}

private final class InterruptedReconciliationPolicyBackend:
  CompanionBackend, @unchecked Sendable
{
  private let lock = NSLock()
  private let initial: ControlContentPolicyStatus
  private let committed: ControlContentPolicyStatus
  private var fetches = 0
  private var retentionCalls = 0

  init(initial: ControlContentPolicyStatus, committed: ControlContentPolicyStatus) {
    self.initial = initial
    self.committed = committed
  }

  var fetchCallCount: Int { lock.withContentPolicyLock { fetches } }
  var retentionCallCount: Int { lock.withContentPolicyLock { retentionCalls } }

  func fetchHealth() async -> HealthReadout { .notRunning }
  func loadSettings() throws -> AgentSettings { AgentSettings() }
  func saveSettings(_: AgentSettings) throws {}
  func makeAuthorizationSession() -> any AuthorizationSession {
    UnavailableAuthorizationSession(reason: .notWired)
  }
  func requestRepair() async -> CommandOutcome { .unavailable(.notWired) }
  func removeAccount(_: RemovalConfirmation) async -> CommandOutcome {
    .unavailable(.notWired)
  }
  func fetchContentPolicy(
    accountId _: Int64
  ) async -> PolicyOutcome<ControlContentPolicyStatus> {
    lock.withContentPolicyLock {
      fetches += 1
      let outcome: PolicyOutcome<ControlContentPolicyStatus>
      switch fetches {
      case 1: outcome = .value(initial)
      case 2: outcome = .unavailable(.agentNotRunning)
      default: outcome = .value(committed)
      }
      return outcome
    }
  }
  func setRetention(
    accountId _: Int64,
    target _: ControlRetentionMode,
    typedConfirmation _: String?
  ) async -> PolicyOutcome<ControlRetentionTransition> {
    lock.withContentPolicyLock {
      retentionCalls += 1
      return .unavailable(.dropped)
    }
  }
  func setArchiveMode(
    accountId _: Int64,
    enabled _: Bool
  ) async -> PolicyOutcome<ControlArchiveModeTransition> {
    .unavailable(.notWired)
  }
  func resumeRetentionPurge(
    accountId _: Int64
  ) async -> PolicyOutcome<ControlRetentionPurgeResume> {
    .unavailable(.notWired)
  }
}

private final class RetryableFailurePolicyBackend: CompanionBackend, @unchecked Sendable {
  private let lock = NSLock()
  private var current: ControlContentPolicyStatus
  private let committed: ControlContentPolicyStatus
  private var retentionCalls = 0

  init(initial: ControlContentPolicyStatus, committed: ControlContentPolicyStatus) {
    current = initial
    self.committed = committed
  }

  var retentionCallCount: Int { lock.withContentPolicyLock { retentionCalls } }

  func fetchHealth() async -> HealthReadout { .notRunning }
  func loadSettings() throws -> AgentSettings { AgentSettings() }
  func saveSettings(_: AgentSettings) throws {}
  func makeAuthorizationSession() -> any AuthorizationSession {
    UnavailableAuthorizationSession(reason: .notWired)
  }
  func requestRepair() async -> CommandOutcome { .unavailable(.notWired) }
  func removeAccount(_: RemovalConfirmation) async -> CommandOutcome {
    .unavailable(.notWired)
  }
  func fetchContentPolicy(
    accountId _: Int64
  ) async -> PolicyOutcome<ControlContentPolicyStatus> {
    lock.withContentPolicyLock { .value(current) }
  }
  func setRetention(
    accountId _: Int64,
    target _: ControlRetentionMode,
    typedConfirmation _: String?
  ) async -> PolicyOutcome<ControlRetentionTransition> {
    lock.withContentPolicyLock {
      retentionCalls += 1
      guard retentionCalls > 1 else { return .failed(.storage) }
      let previous = current.retention
      current = committed
      return .value(
        ControlRetentionTransition(
          previous: previous,
          current: committed.retention,
          purgedRevisions: 0,
          purgedDeletedMetadata: 0,
          purgedRetainedBytes: 0,
          invalidatedItems: 0,
          invalidatedDocuments: 0,
          acknowledgedFilePurges: 0,
          status: committed))
    }
  }
  func setArchiveMode(
    accountId _: Int64,
    enabled _: Bool
  ) async -> PolicyOutcome<ControlArchiveModeTransition> {
    .unavailable(.notWired)
  }
  func resumeRetentionPurge(
    accountId _: Int64
  ) async -> PolicyOutcome<ControlRetentionPurgeResume> {
    .unavailable(.notWired)
  }
}

private struct UnavailablePolicyBackend: CompanionBackend {
  let available: ControlContentPolicyStatus
  let unavailableAccountId: Int64

  func fetchHealth() async -> HealthReadout { .notRunning }
  func loadSettings() throws -> AgentSettings { AgentSettings() }
  func saveSettings(_: AgentSettings) throws {}
  func makeAuthorizationSession() -> any AuthorizationSession {
    UnavailableAuthorizationSession(reason: .notWired)
  }
  func requestRepair() async -> CommandOutcome { .unavailable(.notWired) }
  func removeAccount(_: RemovalConfirmation) async -> CommandOutcome {
    .unavailable(.notWired)
  }
  func fetchContentPolicy(
    accountId: Int64
  ) async -> PolicyOutcome<ControlContentPolicyStatus> {
    if accountId == available.accountId { return .value(available) }
    if accountId == unavailableAccountId { return .unavailable(.agentNotRunning) }
    return .failed(.storage)
  }
  func setRetention(
    accountId _: Int64,
    target _: ControlRetentionMode,
    typedConfirmation _: String?
  ) async -> PolicyOutcome<ControlRetentionTransition> {
    .unavailable(.notWired)
  }
  func setArchiveMode(
    accountId _: Int64,
    enabled _: Bool
  ) async -> PolicyOutcome<ControlArchiveModeTransition> {
    .unavailable(.notWired)
  }
  func resumeRetentionPurge(
    accountId _: Int64
  ) async -> PolicyOutcome<ControlRetentionPurgeResume> {
    .unavailable(.notWired)
  }
}

extension NSLock {
  fileprivate func withContentPolicyLock<T>(_ body: () throws -> T) rethrows -> T {
    lock()
    defer { unlock() }
    return try body()
  }
}
