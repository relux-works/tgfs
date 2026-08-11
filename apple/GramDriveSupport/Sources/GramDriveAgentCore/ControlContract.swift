import Foundation
import GramDriveSupport

/// The wire contract of the agent control channel (BUG-260720-3i74u1) —
/// the third narrow agent IPC channel, beside health and hydration
/// (PLAT-MAC-002's "narrow native service", DEC-006).
///
/// Same family as the hydration channel: newline-delimited JSON documents
/// over a UNIX socket in the shared container, size-capped lines, a typed
/// vocabulary on both sides, and disconnection as the only out-of-band
/// signal. Two connection shapes share the socket:
///
/// - **Command**: the client sends one ``ControlRequest`` line, the server
///   answers with exactly one terminal ``ControlEvent`` line and closes.
///   Status, settings reload, repair, and account removal are commands.
/// - **Auth session**: a ``ControlRequest`` whose operation is
///   ``ControlOperation/authStart`` upgrades the connection to a sign-in
///   session: the server streams ``ControlEvent/authState(_:)`` lines as
///   the flow moves, the client sends ``ControlAuthInputFrame`` lines, and
///   each input is answered by a sequence-correlated
///   ``ControlEvent/authSubmitResult(_:)``. Either side closing the
///   connection abandons the flow (the server cancels the sign-in).
///
/// Content policy commands mutate only account-scoped engine policy through
/// the owned agent. They never expose a drive-item mutation surface to the
/// companion or File Provider.
public enum ControlContract {
  /// Version of this wire contract; a mismatched request is refused with
  /// a typed failure naming the server's version.
  public static let protocolVersion = AgentControlEndpoint.protocolVersion

  /// Upper bound on one request or input line.
  public static let maxRequestLineBytes = 16 * 1024

  /// Upper bound on one server event line.
  public static let maxEventLineBytes = 64 * 1024

  /// The control socket's path rule: `<root>/agent/control.sock` — the
  /// same derivation every GramDrive process computes from the shared
  /// data root.
  public static func socketURL(dataRoot: URL) -> URL {
    AgentControlEndpoint.socketURL(dataRoot: dataRoot)
  }
}

/// The operation a control connection opens with.
public enum ControlOperation: String, Codable, Sendable {
  /// One point-in-time health/status snapshot.
  case status
  /// Re-read the durable settings document and apply it to the running
  /// agent; answers with the settings now in effect.
  case reloadSettings
  /// The agent-side repair pass (engine/state diagnostics).
  case repair
  /// The engine half of the SEC-004 account removal.
  case removeAccount
  /// Upgrade this connection to an interactive sign-in session.
  case authStart
  /// Best-effort File Provider scheduling hint; never performs source I/O
  /// on the serving thread.
  case historyPriority
  /// Aggregate-only File Provider fetch telemetry.
  case providerFetchHealth
  /// Read the committed per-account retention and Archive state.
  case contentPolicyStatus
  /// Change one account's retention selection.
  case setRetention
  /// Change the independent per-account Archive Mode setting.
  case setArchiveMode
  /// Resume an interrupted Audit-to-Mirror physical-file purge.
  case resumeRetentionPurge
  /// Stop accepting work, drain the owned agent, and exit so a replacement
  /// bundle can relaunch the matching executable.
  case prepareForTermination
}

/// The first line of every control connection.
public struct ControlRequest: Codable, Equatable, Sendable {
  /// The client's ``ControlContract/protocolVersion``.
  public var protocolVersion: Int
  /// What this connection is for.
  public var operation: ControlOperation
  /// The removal's parameters; present exactly for
  /// ``ControlOperation/removeAccount``.
  public var removal: ControlRemovalRequest?
  /// Parameters for ``ControlOperation/historyPriority``.
  public var historyPriority: HistoryPriorityRequest?
  /// Parameters for ``ControlOperation/providerFetchHealth``.
  public var providerFetchHealth: ProviderFetchHealthReport?
  /// Parameters for one content-policy operation.
  public var contentPolicy: ControlContentPolicyRequest?
  /// Parameters for an app quit or update-relaunch drain.
  public var termination: ControlTerminationRequest?

  public init(
    protocolVersion: Int = ControlContract.protocolVersion,
    operation: ControlOperation,
    removal: ControlRemovalRequest? = nil,
    historyPriority: HistoryPriorityRequest? = nil,
    providerFetchHealth: ProviderFetchHealthReport? = nil,
    contentPolicy: ControlContentPolicyRequest? = nil,
    termination: ControlTerminationRequest? = nil
  ) {
    self.protocolVersion = protocolVersion
    self.operation = operation
    self.removal = removal
    self.historyPriority = historyPriority
    self.providerFetchHealth = providerFetchHealth
    self.contentPolicy = contentPolicy
    self.termination = termination
  }
}

/// A companion-requested, bounded agent shutdown. `targetBuild` is carried
/// only for an update and is a numeric CFBundleVersion, never user data.
public struct ControlTerminationRequest: Codable, Equatable, Sendable {
  /// Correlates a possibly dropped acknowledgement with the health endpoint.
  public var requestID: UUID
  /// The companion's captured old-process identity. This is mandatory for
  /// every termination mutation: a replacement agent must reject delayed
  /// prepare/cancel/commit bytes for a predecessor.
  public var expectedAgentInstanceID: UUID

  public enum Action: String, Codable, Sendable {
    case prepare
    /// Makes a request-correlated prepared drain irreversible. The server
    /// sends the acknowledgement before beginning teardown.
    case commit
    case cancel
  }

  public enum Reason: String, Codable, Sendable {
    case userQuit
    case update
  }

  public var reason: Reason
  public var targetBuild: String?
  public var action: Action

  public init(
    requestID: UUID = UUID(),
    expectedAgentInstanceID: UUID,
    reason: Reason,
    targetBuild: String? = nil,
    action: Action = .prepare
  ) {
    self.requestID = requestID
    self.expectedAgentInstanceID = expectedAgentInstanceID
    self.reason = reason
    self.targetBuild = targetBuild
    self.action = action
  }
}

/// The retention selection shown to and requested by the companion.
public enum ControlRetentionMode: String, Codable, Equatable, Sendable {
  case mirror
  case audit
}

/// Parameters shared by the account-scoped content-policy commands.
public struct ControlContentPolicyRequest: Codable, Equatable, Sendable {
  public var accountId: Int64
  /// Present exactly for ``ControlOperation/setRetention``.
  public var retention: ControlRetentionMode?
  /// Exact account-specific phrase for a destructive Audit-to-Mirror
  /// request. Mirror-to-Audit carries `nil`.
  public var typedConfirmation: String?
  /// Present exactly for ``ControlOperation/setArchiveMode``.
  public var archiveModeEnabled: Bool?

  public init(
    accountId: Int64,
    retention: ControlRetentionMode? = nil,
    typedConfirmation: String? = nil,
    archiveModeEnabled: Bool? = nil
  ) {
    self.accountId = accountId
    self.retention = retention
    self.typedConfirmation = typedConfirmation
    self.archiveModeEnabled = archiveModeEnabled
  }
}

/// What the engine currently reports about Archive eager backfill.
///
/// Counts are optional for compatibility with an agent whose core reports
/// only the durable on/off setting. `nil` is rendered as "not reported",
/// never as zero or "up to date".
public struct ControlArchiveBackfillProgress: Codable, Equatable, Sendable {
  public var pendingAllowedItems: UInt64?
  public var failedAllowedItems: UInt64?
  public var failureCategory: String?

  public init(
    pendingAllowedItems: UInt64? = nil,
    failedAllowedItems: UInt64? = nil,
    failureCategory: String? = nil
  ) {
    self.pendingAllowedItems = pendingAllowedItems
    self.failedAllowedItems = failedAllowedItems
    self.failureCategory = failureCategory
  }
}

/// Truthful committed policy state for one account.
public struct ControlContentPolicyStatus: Codable, Equatable, Sendable {
  public var accountId: Int64
  public var retention: ControlRetentionMode
  public var archiveModeEnabled: Bool
  /// Crash-resumable physical objects still awaiting purge.
  public var pendingFilePurges: UInt64
  /// The exact phrase required for Audit-to-Mirror.
  public var auditToMirrorConfirmationPhrase: String
  /// Optional Archive progress. Missing fields mean the engine did not
  /// report them; they do not imply there is no work.
  public var archiveBackfill: ControlArchiveBackfillProgress

  public init(
    accountId: Int64,
    retention: ControlRetentionMode,
    archiveModeEnabled: Bool,
    pendingFilePurges: UInt64,
    auditToMirrorConfirmationPhrase: String,
    archiveBackfill: ControlArchiveBackfillProgress = ControlArchiveBackfillProgress()
  ) {
    self.accountId = accountId
    self.retention = retention
    self.archiveModeEnabled = archiveModeEnabled
    self.pendingFilePurges = pendingFilePurges
    self.auditToMirrorConfirmationPhrase = auditToMirrorConfirmationPhrase
    self.archiveBackfill = archiveBackfill
  }

  private enum CodingKeys: String, CodingKey {
    case accountId
    case retention
    case archiveModeEnabled
    case pendingFilePurges
    case auditToMirrorConfirmationPhrase
    case archiveBackfill
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    accountId = try container.decode(Int64.self, forKey: .accountId)
    retention =
      try container.decodeIfPresent(ControlRetentionMode.self, forKey: .retention)
      ?? .mirror
    archiveModeEnabled =
      try container.decodeIfPresent(Bool.self, forKey: .archiveModeEnabled) ?? false
    pendingFilePurges =
      try container.decodeIfPresent(UInt64.self, forKey: .pendingFilePurges) ?? 0
    auditToMirrorConfirmationPhrase =
      try container.decodeIfPresent(
        String.self, forKey: .auditToMirrorConfirmationPhrase) ?? ""
    archiveBackfill =
      try container.decodeIfPresent(
        ControlArchiveBackfillProgress.self, forKey: .archiveBackfill)
      ?? ControlArchiveBackfillProgress()
  }
}

/// Effects and resulting state of one retention transition.
public struct ControlRetentionTransition: Codable, Equatable, Sendable {
  public var previous: ControlRetentionMode
  public var current: ControlRetentionMode
  public var purgedRevisions: UInt64
  public var purgedDeletedMetadata: UInt64
  public var purgedRetainedBytes: UInt64
  public var invalidatedItems: UInt64
  public var invalidatedDocuments: UInt64
  public var acknowledgedFilePurges: UInt64
  public var status: ControlContentPolicyStatus

  public init(
    previous: ControlRetentionMode,
    current: ControlRetentionMode,
    purgedRevisions: UInt64,
    purgedDeletedMetadata: UInt64,
    purgedRetainedBytes: UInt64,
    invalidatedItems: UInt64,
    invalidatedDocuments: UInt64,
    acknowledgedFilePurges: UInt64,
    status: ControlContentPolicyStatus
  ) {
    self.previous = previous
    self.current = current
    self.purgedRevisions = purgedRevisions
    self.purgedDeletedMetadata = purgedDeletedMetadata
    self.purgedRetainedBytes = purgedRetainedBytes
    self.invalidatedItems = invalidatedItems
    self.invalidatedDocuments = invalidatedDocuments
    self.acknowledgedFilePurges = acknowledgedFilePurges
    self.status = status
  }
}

/// Effects and resulting state of one Archive Mode transition.
public struct ControlArchiveModeTransition: Codable, Equatable, Sendable {
  public var previous: Bool
  public var current: Bool
  public var pinnedAllowedItems: UInt64
  public var releasedItems: UInt64
  public var status: ControlContentPolicyStatus

  public init(
    previous: Bool,
    current: Bool,
    pinnedAllowedItems: UInt64,
    releasedItems: UInt64,
    status: ControlContentPolicyStatus
  ) {
    self.previous = previous
    self.current = current
    self.pinnedAllowedItems = pinnedAllowedItems
    self.releasedItems = releasedItems
    self.status = status
  }
}

/// Result of explicitly resuming a crash-interrupted retention purge.
public struct ControlRetentionPurgeResume: Codable, Equatable, Sendable {
  public var acknowledgedFilePurges: UInt64
  public var status: ControlContentPolicyStatus

  public init(
    acknowledgedFilePurges: UInt64,
    status: ControlContentPolicyStatus
  ) {
    self.acknowledgedFilePurges = acknowledgedFilePurges
    self.status = status
  }
}

/// Parameters of an account removal command.
public struct ControlRemovalRequest: Codable, Equatable, Sendable {
  /// The account to remove (its stable Telegram identity).
  public var accountId: Int64
  /// Whether the Telegram session is revoked server-side (`logOut`) or
  /// only ended locally.
  public var revokeSession: Bool

  public init(accountId: Int64, revokeSession: Bool) {
    self.accountId = accountId
    self.revokeSession = revokeSession
  }
}

/// The stable category of a failed control command — the wire form of the
/// FFI `DriveError` categories, so the shell can branch without parsing.
public enum ControlFailureCategory: String, Codable, Equatable, Sendable {
  case invalidArgument
  case notFound
  case authRequired
  case rateLimited
  case sourceUnavailable
  case storage
  case integrity
  case cancelled
  case internalError = "internal"

  /// Lenient decode: an unrecognized category (a newer agent) is an
  /// internal error, never a decode failure.
  public init(from decoder: Decoder) throws {
    let raw = try decoder.singleValueContainer().decode(String.self)
    self = ControlFailureCategory(rawValue: raw) ?? .internalError
  }
}

/// A classified command failure.
public struct ControlCommandFailure: Codable, Equatable, Sendable {
  /// The stable category.
  public var category: ControlFailureCategory
  /// Redacted diagnostic detail; not contractual.
  public var detail: String
  /// Source-stated minimum backoff, when the category is rate limiting.
  public var retryAfterMs: UInt64?

  public init(category: ControlFailureCategory, detail: String, retryAfterMs: UInt64? = nil) {
    self.category = category
    self.detail = detail
    self.retryAfterMs = retryAfterMs
  }
}

/// One sign-in flow state, as the agent reports it. `kind` mirrors the
/// engine's auth vocabulary; the optional payloads carry what that state
/// renders. Kinds outside the client's vocabulary must degrade to an
/// "unsupported" rendering, never a decode failure.
public struct ControlAuthState: Codable, Equatable, Sendable {
  /// The state's stable name: `starting`, `configuring`,
  /// `wait-phone-number`, `wait-code`, `wait-qr-confirmation`,
  /// `wait-password`, `ready`, `logging-out`, `closing`, `closed`,
  /// `unsupported`, `failed`.
  public var kind: String
  /// Rendering material for `wait-code`.
  public var codeInfo: ControlAuthCodeInfo?
  /// Rendering material for `wait-password`.
  public var passwordInfo: ControlAuthPasswordInfo?
  /// The `tg://login` link for `wait-qr-confirmation`.
  public var qrLink: String?
  /// The reported state's name for `unsupported`.
  public var unsupportedKind: String?
  /// The signed-in identity, present exactly on `ready`.
  public var account: ControlAccountIdentity?
  /// The stable failure code, present exactly on `failed`.
  public var failureDetail: String?

  public init(
    kind: String,
    codeInfo: ControlAuthCodeInfo? = nil,
    passwordInfo: ControlAuthPasswordInfo? = nil,
    qrLink: String? = nil,
    unsupportedKind: String? = nil,
    account: ControlAccountIdentity? = nil,
    failureDetail: String? = nil
  ) {
    self.kind = kind
    self.codeInfo = codeInfo
    self.passwordInfo = passwordInfo
    self.qrLink = qrLink
    self.unsupportedKind = unsupportedKind
    self.account = account
    self.failureDetail = failureDetail
  }
}

/// What the code-entry step renders.
public struct ControlAuthCodeInfo: Codable, Equatable, Sendable {
  public var phoneNumber: String
  public var codeLength: Int?
  public var resendTimeoutSeconds: Int?

  public init(phoneNumber: String, codeLength: Int? = nil, resendTimeoutSeconds: Int? = nil) {
    self.phoneNumber = phoneNumber
    self.codeLength = codeLength
    self.resendTimeoutSeconds = resendTimeoutSeconds
  }
}

/// What the 2FA password step renders.
public struct ControlAuthPasswordInfo: Codable, Equatable, Sendable {
  public var hint: String
  public var hasRecoveryEmail: Bool

  public init(hint: String, hasRecoveryEmail: Bool) {
    self.hint = hint
    self.hasRecoveryEmail = hasRecoveryEmail
  }
}

/// The signed-in account, reported with the `ready` state.
public struct ControlAccountIdentity: Codable, Equatable, Sendable {
  public var accountId: Int64
  public var displayName: String

  public init(accountId: Int64, displayName: String) {
    self.accountId = accountId
    self.displayName = displayName
  }
}

/// One user action in a sign-in session.
public enum ControlAuthInput: Codable, Equatable, Sendable {
  case submitPhoneNumber(String)
  case requestQrCode
  case submitCode(String)
  case resendCode
  case submitPassword(String)
  case cancel

  private enum CodingKeys: String, CodingKey {
    case kind
    case value
  }

  private enum Kind: String, Codable {
    case submitPhoneNumber = "submit-phone-number"
    case requestQrCode = "request-qr-code"
    case submitCode = "submit-code"
    case resendCode = "resend-code"
    case submitPassword = "submit-password"
    case cancel
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    switch try container.decode(Kind.self, forKey: .kind) {
    case .submitPhoneNumber:
      self = .submitPhoneNumber(try container.decode(String.self, forKey: .value))
    case .requestQrCode:
      self = .requestQrCode
    case .submitCode:
      self = .submitCode(try container.decode(String.self, forKey: .value))
    case .resendCode:
      self = .resendCode
    case .submitPassword:
      self = .submitPassword(try container.decode(String.self, forKey: .value))
    case .cancel:
      self = .cancel
    }
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    switch self {
    case .submitPhoneNumber(let value):
      try container.encode(Kind.submitPhoneNumber, forKey: .kind)
      try container.encode(value, forKey: .value)
    case .requestQrCode:
      try container.encode(Kind.requestQrCode, forKey: .kind)
    case .submitCode(let value):
      try container.encode(Kind.submitCode, forKey: .kind)
      try container.encode(value, forKey: .value)
    case .resendCode:
      try container.encode(Kind.resendCode, forKey: .kind)
    case .submitPassword(let value):
      try container.encode(Kind.submitPassword, forKey: .kind)
      try container.encode(value, forKey: .value)
    case .cancel:
      try container.encode(Kind.cancel, forKey: .kind)
    }
  }
}

/// One client line on an auth connection: an input with the client's own
/// sequence number, echoed back in the matching
/// ``ControlEvent/authSubmitResult(_:)``.
public struct ControlAuthInputFrame: Codable, Equatable, Sendable {
  public var seq: UInt64
  public var input: ControlAuthInput

  public init(seq: UInt64, input: ControlAuthInput) {
    self.seq = seq
    self.input = input
  }
}

/// TDLib's classified refusal of one sign-in input, on the wire.
public struct ControlAuthRejection: Codable, Equatable, Sendable {
  /// The stable rejection name: `invalid-phone-number`,
  /// `phone-number-banned`, `invalid-code`, `expired-code`,
  /// `invalid-password`, `rate-limited`, `network`, `session-ended`,
  /// `other`.
  public var kind: String
  /// Stated minimum wait for `rate-limited`.
  public var retryAfterSeconds: UInt64?
  /// The source's numeric code for `other`.
  public var code: Int64?
  /// Diagnostic detail for `other`; not contractual.
  public var detail: String?

  public init(
    kind: String,
    retryAfterSeconds: UInt64? = nil,
    code: Int64? = nil,
    detail: String? = nil
  ) {
    self.kind = kind
    self.retryAfterSeconds = retryAfterSeconds
    self.code = code
    self.detail = detail
  }
}

/// The answer to one ``ControlAuthInputFrame``.
public struct ControlAuthSubmitResult: Codable, Equatable, Sendable {
  /// The frame's sequence number, echoed.
  public var seq: UInt64
  /// `accepted`, `rejected`, or `invalid-for-state`.
  public var outcome: String
  /// The classified rejection, present exactly for `rejected`.
  public var rejection: ControlAuthRejection?

  public init(seq: UInt64, outcome: String, rejection: ControlAuthRejection? = nil) {
    self.seq = seq
    self.outcome = outcome
    self.rejection = rejection
  }
}

/// One server line on a control connection.
public enum ControlEvent: Equatable, Sendable {
  /// A command completed.
  case commandDone
  /// The agent atomically accepted the request-correlated irreversible
  /// termination commit. The companion must still wait for the endpoint to
  /// disappear before it gives AppKit its `true` reply.
  case terminationCommitAccepted
  /// A command failed, classified. Terminal for command connections; on
  /// an auth connection it refuses the upgrade.
  case commandFailed(ControlCommandFailure)
  /// The status answer.
  case status(AgentHealthSnapshot)
  /// The settings now in effect (the settings-reload answer).
  case settings(AgentSettings)
  /// A sign-in state transition.
  case authState(ControlAuthState)
  /// The answer to one input frame.
  case authSubmitResult(ControlAuthSubmitResult)
  /// Committed per-account policy state.
  case contentPolicyStatus(ControlContentPolicyStatus)
  /// A committed retention transition and its resulting state.
  case retentionChanged(ControlRetentionTransition)
  /// A committed Archive Mode transition and its resulting state.
  case archiveModeChanged(ControlArchiveModeTransition)
  /// A purge-resume pass and its resulting state.
  case retentionPurgeResumed(ControlRetentionPurgeResume)
}

extension ControlEvent: Codable {
  private enum CodingKeys: String, CodingKey {
    case event
    case failure
    case status
    case settings
    case state
    case result
    case contentPolicy
    case retentionTransition
    case archiveModeTransition
    case purgeResume
  }

  private enum Kind: String, Codable {
    case commandDone = "done"
    case terminationCommitAccepted = "termination-commit-accepted"
    case commandFailed = "failed"
    case status
    case settings
    case authState = "auth-state"
    case authSubmitResult = "auth-submit-result"
    case contentPolicyStatus = "content-policy-status"
    case retentionChanged = "retention-changed"
    case archiveModeChanged = "archive-mode-changed"
    case retentionPurgeResumed = "retention-purge-resumed"
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    switch try container.decode(Kind.self, forKey: .event) {
    case .commandDone:
      self = .commandDone
    case .terminationCommitAccepted:
      self = .terminationCommitAccepted
    case .commandFailed:
      self = .commandFailed(try container.decode(ControlCommandFailure.self, forKey: .failure))
    case .status:
      self = .status(try container.decode(AgentHealthSnapshot.self, forKey: .status))
    case .settings:
      self = .settings(try container.decode(AgentSettings.self, forKey: .settings))
    case .authState:
      self = .authState(try container.decode(ControlAuthState.self, forKey: .state))
    case .authSubmitResult:
      self = .authSubmitResult(
        try container.decode(ControlAuthSubmitResult.self, forKey: .result))
    case .contentPolicyStatus:
      self = .contentPolicyStatus(
        try container.decode(ControlContentPolicyStatus.self, forKey: .contentPolicy))
    case .retentionChanged:
      self = .retentionChanged(
        try container.decode(
          ControlRetentionTransition.self, forKey: .retentionTransition))
    case .archiveModeChanged:
      self = .archiveModeChanged(
        try container.decode(
          ControlArchiveModeTransition.self, forKey: .archiveModeTransition))
    case .retentionPurgeResumed:
      self = .retentionPurgeResumed(
        try container.decode(
          ControlRetentionPurgeResume.self, forKey: .purgeResume))
    }
  }

  public func encode(to encoder: Encoder) throws {
    var container = encoder.container(keyedBy: CodingKeys.self)
    switch self {
    case .commandDone:
      try container.encode(Kind.commandDone, forKey: .event)
    case .terminationCommitAccepted:
      try container.encode(Kind.terminationCommitAccepted, forKey: .event)
    case .commandFailed(let failure):
      try container.encode(Kind.commandFailed, forKey: .event)
      try container.encode(failure, forKey: .failure)
    case .status(let snapshot):
      try container.encode(Kind.status, forKey: .event)
      try container.encode(snapshot, forKey: .status)
    case .settings(let settings):
      try container.encode(Kind.settings, forKey: .event)
      try container.encode(settings, forKey: .settings)
    case .authState(let state):
      try container.encode(Kind.authState, forKey: .event)
      try container.encode(state, forKey: .state)
    case .authSubmitResult(let result):
      try container.encode(Kind.authSubmitResult, forKey: .event)
      try container.encode(result, forKey: .result)
    case .contentPolicyStatus(let status):
      try container.encode(Kind.contentPolicyStatus, forKey: .event)
      try container.encode(status, forKey: .contentPolicy)
    case .retentionChanged(let transition):
      try container.encode(Kind.retentionChanged, forKey: .event)
      try container.encode(transition, forKey: .retentionTransition)
    case .archiveModeChanged(let transition):
      try container.encode(Kind.archiveModeChanged, forKey: .event)
      try container.encode(transition, forKey: .archiveModeTransition)
    case .retentionPurgeResumed(let resume):
      try container.encode(Kind.retentionPurgeResumed, forKey: .event)
      try container.encode(resume, forKey: .purgeResume)
    }
  }
}
