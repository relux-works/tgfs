import Foundation

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
/// The v1 command set is deliberately read-only toward the drive (DEC-007):
/// nothing here mutates items or content — sign-in, settings, repair, and
/// the SEC-004 removal are account/host operations.
public enum ControlContract {
    /// Version of this wire contract; a mismatched request is refused with
    /// a typed failure naming the server's version.
    public static let protocolVersion = 1

    /// Upper bound on one request or input line.
    public static let maxRequestLineBytes = 16 * 1024

    /// Upper bound on one server event line.
    public static let maxEventLineBytes = 64 * 1024

    /// The control socket's path rule: `<root>/agent/control.sock` — the
    /// same derivation every GramDrive process computes from the shared
    /// data root.
    public static func socketURL(dataRoot: URL) -> URL {
        dataRoot
            .appendingPathComponent("agent", isDirectory: true)
            .appendingPathComponent("control.sock", isDirectory: false)
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

    public init(
        protocolVersion: Int = ControlContract.protocolVersion,
        operation: ControlOperation,
        removal: ControlRemovalRequest? = nil
    ) {
        self.protocolVersion = protocolVersion
        self.operation = operation
        self.removal = removal
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
}

extension ControlEvent: Codable {
    private enum CodingKeys: String, CodingKey {
        case event
        case failure
        case status
        case settings
        case state
        case result
    }

    private enum Kind: String, Codable {
        case commandDone = "done"
        case commandFailed = "failed"
        case status
        case settings
        case authState = "auth-state"
        case authSubmitResult = "auth-submit-result"
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(Kind.self, forKey: .event) {
        case .commandDone:
            self = .commandDone
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
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .commandDone:
            try container.encode(Kind.commandDone, forKey: .event)
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
        }
    }
}
