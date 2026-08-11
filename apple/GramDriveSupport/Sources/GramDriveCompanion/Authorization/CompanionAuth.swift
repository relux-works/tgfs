import Foundation

/// The authorization flow as the shell renders it — a faithful mirror of the
/// core's provider-neutral auth vocabulary (`gramdrive-source-tdjson::auth`:
/// `AuthState`, `AuthInput`, `AuthRejection`, `RetryAdvice`).
///
/// The shell never talks to TDLib: authorization is a Telegram operation and
/// runs in the engine-hosting agent, which owns the client. The shell drives
/// it through the ``AuthorizationSession`` seam (an agent control channel)
/// and renders whatever state the agent reports. Keeping these types
/// isomorphic to the Rust vocabulary makes the eventual wiring a thin
/// mapping rather than a translation layer — and lets every screen state be
/// exercised deterministically today against a scripted session.

/// The core-facing authorization state, one screen the shell can render.
/// Mirror of `auth::AuthState`.
public enum CompanionAuthState: Equatable, Sendable {
    /// No session started yet, or none exists.
    case idle
    /// A session started; no authorization update has arrived yet.
    case starting
    /// The agent is configuring the client (answering TDLib parameters).
    /// Not a user-facing wait; shown as indeterminate progress.
    case configuring
    /// Waiting for the user's phone number — or a switch to QR sign-in.
    case waitPhoneNumber
    /// A login code was sent; waiting for the user to enter it.
    case waitCode(CompanionCodeInfo)
    /// Waiting for another logged-in device to confirm the QR link.
    case waitQrConfirmation(link: String)
    /// Waiting for the account's 2FA password.
    case waitPassword(CompanionPasswordInfo)
    /// Authorized; the flow is complete.
    case ready
    /// The account is being logged out (account removal's flow).
    case loggingOut
    /// The client is closing.
    case closing
    /// The client is closed; the session ended.
    case closed
    /// TDLib reported a state outside the supported v1 sign-in scope (email
    /// gates, registration) or newer than the core. Only ``CompanionAuthInput/cancel``
    /// is accepted here. Mirror of `AuthState::Unsupported`.
    case unsupported(kind: String)
    /// The sign-in authorized but could not be persisted (the agent's
    /// finalization failed); the session is over and a fresh sign-in is
    /// required. `detail` is a stable redacted code, diagnostic only.
    case failed(detail: String)

    /// A stable diagnostic name, matching `AuthState::kind`.
    public var kind: String {
        switch self {
        case .idle: return "idle"
        case .starting: return "starting"
        case .configuring: return "configuring"
        case .waitPhoneNumber: return "wait-phone-number"
        case .waitCode: return "wait-code"
        case .waitQrConfirmation: return "wait-qr-confirmation"
        case .waitPassword: return "wait-password"
        case .ready: return "ready"
        case .loggingOut: return "logging-out"
        case .closing: return "closing"
        case .closed: return "closed"
        case .unsupported: return "unsupported"
        case .failed: return "failed"
        }
    }

    /// Whether this is a state the flow can still act in (an input other than
    /// cancel could be valid). Terminal and out-of-scope states are not.
    public var acceptsInput: Bool {
        switch self {
        case .waitPhoneNumber, .waitCode, .waitQrConfirmation, .waitPassword:
            return true
        case .idle, .starting, .configuring, .ready, .loggingOut, .closing,
            .closed, .unsupported, .failed:
            return false
        }
    }
}

/// What the shell needs to render the code-entry step. Mirror of `auth::CodeInfo`.
public struct CompanionCodeInfo: Equatable, Sendable {
    /// The phone number the code was sent to (TDLib echoes it in clear).
    public var phoneNumber: String
    /// Expected code length, when the delivery method states one.
    public var codeLength: Int?
    /// Seconds before a resend is allowed, when TDLib states it — for the
    /// UI's countdown only; the agent, not the shell, enforces it.
    public var resendTimeoutSeconds: Int?

    public init(phoneNumber: String, codeLength: Int? = nil, resendTimeoutSeconds: Int? = nil) {
        self.phoneNumber = phoneNumber
        self.codeLength = codeLength
        self.resendTimeoutSeconds = resendTimeoutSeconds
    }
}

/// What the shell needs to render the 2FA password step. Mirror of `auth::PasswordInfo`.
public struct CompanionPasswordInfo: Equatable, Sendable {
    /// The user's own password hint (may be empty). Display material, not a
    /// secret — the user wrote it to be shown.
    public var hint: String
    /// Whether a recovery email is configured for this password.
    public var hasRecoveryEmail: Bool

    public init(hint: String, hasRecoveryEmail: Bool) {
        self.hint = hint
        self.hasRecoveryEmail = hasRecoveryEmail
    }
}

/// A user action in the authorization flow. Mirror of `auth::AuthInput`.
///
/// The login code and 2FA password are plain `String` here: at the shell
/// boundary they are transient UI values the user just typed. Wrapping them
/// into the core's `Secret` is the control channel's job, at the point they
/// cross into the agent — a wrapper the shell applied would be theater, since
/// the value already sits in a `TextField`'s storage.
public enum CompanionAuthInput: Equatable, Sendable {
    case submitPhoneNumber(String)
    case requestQrCode
    case submitCode(String)
    case resendCode
    case submitPassword(String)
    case cancel

    /// A stable diagnostic name, matching `AuthInput::kind`.
    public var kind: String {
        switch self {
        case .submitPhoneNumber: return "submit-phone-number"
        case .requestQrCode: return "request-qr-code"
        case .submitCode: return "submit-code"
        case .resendCode: return "resend-code"
        case .submitPassword: return "submit-password"
        case .cancel: return "cancel"
        }
    }

    /// Whether this input is structurally valid in `state`, by the same
    /// validity table the core enforces (`AuthMachine::on_input`). Cancel is
    /// valid everywhere except a closed session.
    public func isValid(in state: CompanionAuthState) -> Bool {
        switch (self, state) {
        case (.cancel, .closed): return false
        case (.cancel, _): return true
        case (.submitPhoneNumber, .waitPhoneNumber), (.requestQrCode, .waitPhoneNumber):
            return true
        case (.submitCode, .waitCode), (.resendCode, .waitCode):
            return true
        case (.submitPassword, .waitPassword):
            return true
        default:
            return false
        }
    }
}

/// TDLib's typed answer to an authorization request it refused. Mirror of
/// `auth::AuthRejection`, paired with its ``CompanionRetryAdvice``.
public enum CompanionAuthRejection: Equatable, Sendable {
    case invalidPhoneNumber
    case phoneNumberBanned
    case invalidCode
    case expiredCode
    case invalidPassword
    case rateLimited(retryAfterSeconds: UInt64?)
    case network
    case sessionEnded
    case other(code: Int64, message: String)

    /// What the user can do about this rejection. Mirror of `AuthRejection::advice`.
    public var advice: CompanionRetryAdvice {
        switch self {
        case .invalidPhoneNumber, .invalidCode, .invalidPassword:
            return .reviseInput
        case .expiredCode:
            return .requestNewCode
        case .rateLimited(let after):
            return .waitThenRetry(afterSeconds: after)
        case .network:
            return .retrySameInput
        case .phoneNumberBanned, .sessionEnded, .other:
            return .abort
        }
    }

    /// A short, human-readable line for the UI. Matches the intent of
    /// `AuthRejection`'s `Display`, redacted (no account material).
    public var message: String {
        switch self {
        case .invalidPhoneNumber: return "The phone number was rejected."
        case .phoneNumberBanned: return "This phone number is banned from Telegram."
        case .invalidCode: return "That login code is wrong."
        case .expiredCode: return "The login code expired — request a new one."
        case .invalidPassword: return "That password is wrong."
        case .rateLimited(let after):
            if let after { return "Too many attempts — wait \(after)s and try again." }
            return "Too many attempts — wait and try again."
        case .network: return "Network failure — try again."
        case .sessionEnded: return "The sign-in session ended."
        case .other(let code, let message):
            return "Authorization failed (\(code)): \(message)"
        }
    }
}

/// What the shell should do next after a rejection. Mirror of `auth::RetryAdvice`.
public enum CompanionRetryAdvice: Equatable, Sendable {
    /// Transient: submit the very same input again.
    case retrySameInput
    /// The value was wrong: correct it and resubmit.
    case reviseInput
    /// The code lapsed: resend, then enter the new code.
    case requestNewCode
    /// Flood control: wait (for `afterSeconds` when stated), then retry.
    case waitThenRetry(afterSeconds: UInt64?)
    /// Not recoverable in this flow: surface it and stop.
    case abort
}

/// The result of starting an authorization session.
public enum AuthStartResult: Equatable, Sendable {
    /// The session started; its state stream is now live.
    case started
    /// No agent control channel is available to drive authorization.
    case unavailable(ControlChannelUnavailable)
}

/// The result of submitting one ``CompanionAuthInput``.
public enum AuthSubmitResult: Equatable, Sendable {
    /// Accepted; the agent applied it. The next state arrives on the stream.
    case accepted
    /// The request went out and TDLib refused it — classified, with advice.
    case rejected(CompanionAuthRejection)
    /// The input is not valid in the current state (a caller-side condition,
    /// mirror of `AuthError::InvalidInput`). The flow position is unchanged.
    case invalidForState
    /// The control channel dropped mid-flow.
    case unavailable(ControlChannelUnavailable)
}

/// The seam through which the shell drives one authorization flow in the
/// agent. Production wiring lives in the agent control channel (a future
/// story); tests and previews substitute a scripted session.
///
/// A session yields authorization states on ``states`` and accepts inputs on
/// ``submit(_:)``. The state stream is the single source of truth for the
/// screen, exactly as TDLib's reported state is for the core machine: inputs
/// never move the rendered state on their own, they provoke the agent, which
/// reports the next state.
public protocol AuthorizationSession: Sendable {
    /// The live authorization states, newest last. Finishes when the session
    /// ends (closed, or the channel dropped).
    var states: AsyncStream<CompanionAuthState> { get }
    /// Starts the flow (activates the client in the agent).
    func start() async -> AuthStartResult
    /// Submits one user action.
    func submit(_ input: CompanionAuthInput) async -> AuthSubmitResult
    /// Abandons the flow and returns only after ``states`` has finished and
    /// the session's resources are safe for a replacement session to acquire.
    func cancel() async
}

/// An authorization session that has no channel to drive: `start` reports
/// the reason, its state stream is empty, and every input is unavailable.
/// Preview- and test-support material for the unavailable screen states —
/// the live backend produces real sessions (``LiveAuthorizationSession``)
/// and reports `agentNotRunning` through them instead.
public struct UnavailableAuthorizationSession: AuthorizationSession {
    private let reason: ControlChannelUnavailable

    public init(reason: ControlChannelUnavailable) {
        self.reason = reason
    }

    public var states: AsyncStream<CompanionAuthState> {
        AsyncStream { $0.finish() }
    }

    public func start() async -> AuthStartResult { .unavailable(reason) }

    public func submit(_ input: CompanionAuthInput) async -> AuthSubmitResult {
        .unavailable(reason)
    }

    public func cancel() async {}
}
