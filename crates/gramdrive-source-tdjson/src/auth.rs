//! The authorization state machine: TDLib authorization updates and user
//! inputs become a deterministic, core-facing flow (TASK-260715-51n6jb).
//!
//! # Shape: sans-IO
//!
//! [`AuthMachine`] performs no I/O and holds no client handle. The caller —
//! the coming `DriveSource` adapter, or a native shell through the FFI
//! boundary — owns the wiring: activate the client (any request does; TDLib
//! then starts reporting authorization state), feed every update from the
//! client's [`UpdateStream`] into [`AuthMachine::on_update`], submit the
//! requests each [`AuthStep`] returns, turn user actions into
//! [`AuthInput`]s through [`AuthMachine::on_input`] and submit the request
//! it builds, and classify a failed submission with
//! [`AuthRejection::classify`]. Keeping the machine free of I/O, threads,
//! and timing is what makes every scripted scenario — success, retries,
//! expiry, network loss, cancellation, unknown states — a deterministic
//! test (`tests/auth_flow.rs`).
//!
//! # Determinism: TDLib's reported state is the single source of truth
//!
//! The typed state advances only on TDLib's `updateAuthorizationState`
//! events; inputs never move it. An input is validated against the current
//! state and becomes exactly one request (same state, same input → same
//! request) or a typed [`AuthError`] — never a panic, never a guessed
//! transition. TDLib confirms progress by reporting the next state, so a
//! rejected code or password leaves the machine exactly where TDLib says it
//! is and a retry needs no special path. Interrupted flows resume by the
//! same rule: a fresh machine fed whatever state TDLib reports first —
//! `waitCode` after a restart mid-sign-in, for instance — is immediately in
//! step, because nothing about its position is machine-private.
//!
//! # Provider neutrality (the DEC-003 direction)
//!
//! Everything this module hands the caller is typed, provider-neutral
//! vocabulary — [`AuthState`], [`AuthInput`], [`AuthRejection`],
//! [`RetryAdvice`], [`AuthError`] — suitable for the FFI boundary as-is. No
//! TDLib JSON crosses outward; the raw `@type` string appears only inside
//! [`AuthState::Unsupported`] and [`AuthError`] as diagnostic detail, the
//! same rule the runtime's error type follows. Inward, the machine consumes
//! the same `serde_json::Value` updates the runtime already delivers.
//!
//! # Scope: which states are first-class, and why the rest fail safe
//!
//! V1 signs in an existing personal Telegram account on this device. The
//! first-class paths are phone → code → optional 2FA password, and QR
//! confirmation → optional 2FA password. TDLib states outside that product
//! scope — email-gated sign-in (`authorizationStateWaitEmailAddress`/
//! `…WaitEmailCode`), new-account registration (`…WaitRegistration`), and
//! anything a future TDLib adds — become [`AuthState::Unsupported`]: a
//! typed state the UI can surface honestly, from which [`AuthInput::Cancel`]
//! still works and every other input fails with a typed
//! [`AuthError::UnsupportedState`]. Unknown must never panic or wedge the
//! flow: TDLib upgrades ship new states, and the fail-safe path is the
//! contract that survives them.
//!
//! # Ownership boundaries
//!
//! On `authorizationStateWaitTdlibParameters` the machine answers with
//! [`TdlibConfig::startup_requests`] itself — plumbing the user never sees.
//! [`AuthInput::Cancel`] abandons the flow locally (`close`; the runtime
//! treats the resulting `authorizationStateClosed` as the end of the
//! client). Server-side logout, session revocation, and the on-disk wipe
//! are account removal's flow (TASK-260715-wjaux5, SEC-004), not a cancel.
//!
//! # Secrets
//!
//! The 2FA password and the login code are credentials and ride in
//! [`Secret`]: redacted from every `Debug` form, plaintext reachable only
//! by the crate-private request builder that puts them on the wire to TDLib
//! (SEC-020). The phone number is deliberately not wrapped — TDLib itself
//! echoes it back in clear inside `code_info` and the UI must render it —
//! so wrapping it here would only feign a protection the flow cannot keep.

use serde_json::{Value, json};

use crate::config::{Secret, TdlibConfig};
use crate::error::{TdError, trailing_integer};

/// The core-facing authorization state, translated from TDLib's
/// `authorizationState*` vocabulary. Carries everything a UI needs to
/// render the step; nothing TDLib-shaped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthState {
    /// No authorization update has arrived yet (client just created, or the
    /// caller has not started pumping updates).
    Starting,
    /// TDLib asked for its parameters; the machine already answered with
    /// the account's startup requests. Not a user-facing wait.
    Configuring,
    /// TDLib waits for the user's phone number — or a QR sign-in request.
    WaitPhoneNumber,
    /// TDLib sent a login code and waits for it.
    WaitCode(CodeInfo),
    /// TDLib waits for another logged-in device to confirm the QR link.
    WaitQrConfirmation {
        /// The `tg://login` link to render as a QR code. A fresh link
        /// arrives as a new update in this same state.
        link: String,
    },
    /// TDLib waits for the account's 2FA password.
    WaitPassword(PasswordInfo),
    /// Authorized; the flow is complete.
    Ready,
    /// TDLib is logging the account out (account removal's flow).
    LoggingOut,
    /// TDLib is closing the client.
    Closing,
    /// The client is closed; the runtime ends its lifecycle here.
    Closed,
    /// TDLib reported a state outside the supported v1 sign-in scope —
    /// email gates, registration, or a state newer than this machine. Only
    /// [`AuthInput::Cancel`] is accepted here (module docs).
    Unsupported {
        /// TDLib's `authorization_state.@type` — diagnostic, not
        /// contractual.
        td_type: String,
    },
}

impl AuthState {
    /// A stable name for diagnostics ([`AuthError::InvalidInput`]).
    pub fn kind(&self) -> &'static str {
        match self {
            AuthState::Starting => "starting",
            AuthState::Configuring => "configuring",
            AuthState::WaitPhoneNumber => "wait-phone-number",
            AuthState::WaitCode(_) => "wait-code",
            AuthState::WaitQrConfirmation { .. } => "wait-qr-confirmation",
            AuthState::WaitPassword(_) => "wait-password",
            AuthState::Ready => "ready",
            AuthState::LoggingOut => "logging-out",
            AuthState::Closing => "closing",
            AuthState::Closed => "closed",
            AuthState::Unsupported { .. } => "unsupported",
        }
    }

    /// Translate one `authorization_state` object. Total: every recognized
    /// `@type` maps to its typed state, everything else to
    /// [`AuthState::Unsupported`]; payload members that are missing or
    /// mistyped degrade to defaults rather than failing — a state report
    /// with a mangled detail is still a state report.
    fn from_td(td_type: &str, auth_state: &Value) -> AuthState {
        match td_type {
            "authorizationStateWaitTdlibParameters" => AuthState::Configuring,
            "authorizationStateWaitPhoneNumber" => AuthState::WaitPhoneNumber,
            "authorizationStateWaitCode" => {
                AuthState::WaitCode(CodeInfo::from_td(auth_state.get("code_info")))
            }
            "authorizationStateWaitOtherDeviceConfirmation" => AuthState::WaitQrConfirmation {
                link: auth_state
                    .get("link")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            },
            "authorizationStateWaitPassword" => {
                AuthState::WaitPassword(PasswordInfo::from_td(auth_state))
            }
            "authorizationStateReady" => AuthState::Ready,
            "authorizationStateLoggingOut" => AuthState::LoggingOut,
            "authorizationStateClosing" => AuthState::Closing,
            "authorizationStateClosed" => AuthState::Closed,
            other => AuthState::Unsupported {
                td_type: other.to_owned(),
            },
        }
    }
}

/// What a UI needs to render the code-entry step, from TDLib's
/// `authenticationCodeInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeInfo {
    /// The phone number the code was sent to, as TDLib reports it (may be
    /// empty on a degraded report).
    pub phone_number: String,
    /// The expected code length, when the delivery method states one.
    pub code_length: Option<i64>,
    /// Seconds before the code can be re-sent, when TDLib states it. The
    /// machine does not enforce this — TDLib answers an early resend with
    /// an error the caller classifies — it exists for the UI's countdown.
    pub resend_timeout_secs: Option<i64>,
}

impl CodeInfo {
    fn from_td(code_info: Option<&Value>) -> CodeInfo {
        let member = |key: &str| code_info.and_then(|info| info.get(key));
        CodeInfo {
            phone_number: member("phone_number")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            code_length: member("type")
                .and_then(|kind| kind.get("length"))
                .and_then(Value::as_i64),
            resend_timeout_secs: member("timeout").and_then(Value::as_i64),
        }
    }
}

/// What a UI needs to render the 2FA password step, from
/// `authorizationStateWaitPassword`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordInfo {
    /// The user's own password hint (may be empty). A hint is display
    /// material, not a secret — the user wrote it to be shown.
    pub hint: String,
    /// Whether a recovery email is set up for this password.
    pub has_recovery_email: bool,
}

impl PasswordInfo {
    fn from_td(auth_state: &Value) -> PasswordInfo {
        PasswordInfo {
            hint: auth_state
                .get("password_hint")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            has_recovery_email: auth_state
                .get("has_recovery_email_address")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }
}

/// A user action in the authorization flow. Consumed by
/// [`AuthMachine::on_input`] — by value, so a credential-bearing input has
/// exactly one use.
#[derive(Debug)]
pub enum AuthInput {
    /// Submit the phone number to sign in with
    /// (valid in [`AuthState::WaitPhoneNumber`]).
    SubmitPhoneNumber {
        /// The number in international format; TDLib validates it.
        phone_number: String,
    },
    /// Switch to QR sign-in instead of a phone number
    /// (valid in [`AuthState::WaitPhoneNumber`]).
    RequestQrCode,
    /// Submit the received login code (valid in [`AuthState::WaitCode`]).
    SubmitCode {
        /// The code, held as a secret: a live credential until used.
        code: Secret,
    },
    /// Ask TDLib to send a fresh code (valid in [`AuthState::WaitCode`]) —
    /// the recovery for an expired code, and the "didn't get it" path.
    ResendCode,
    /// Submit the 2FA password (valid in [`AuthState::WaitPassword`]).
    SubmitPassword {
        /// The account password.
        password: Secret,
    },
    /// Abandon the sign-in flow: close the client locally. Valid in every
    /// state except [`AuthState::Closed`]; not a logout (module docs).
    Cancel,
}

impl AuthInput {
    /// A stable name for diagnostics ([`AuthError::InvalidInput`]).
    pub fn kind(&self) -> &'static str {
        match self {
            AuthInput::SubmitPhoneNumber { .. } => "submit-phone-number",
            AuthInput::RequestQrCode => "request-qr-code",
            AuthInput::SubmitCode { .. } => "submit-code",
            AuthInput::ResendCode => "resend-code",
            AuthInput::SubmitPassword { .. } => "submit-password",
            AuthInput::Cancel => "cancel",
        }
    }

    /// The request this input maps to. Infallible and context-free by
    /// design: validity against the current state is decided before this
    /// runs. The only place a code or password leaves its [`Secret`] — onto
    /// the wire to TDLib.
    fn request(&self) -> Value {
        match self {
            AuthInput::SubmitPhoneNumber { phone_number } => json!({
                "@type": "setAuthenticationPhoneNumber",
                "phone_number": phone_number,
            }),
            AuthInput::RequestQrCode => json!({
                "@type": "requestQrCodeAuthentication",
                "other_user_ids": [],
            }),
            AuthInput::SubmitCode { code } => json!({
                "@type": "checkAuthenticationCode",
                "code": code.expose(),
            }),
            AuthInput::ResendCode => json!({"@type": "resendAuthenticationCode"}),
            AuthInput::SubmitPassword { password } => json!({
                "@type": "checkAuthenticationPassword",
                "password": password.expose(),
            }),
            AuthInput::Cancel => json!({"@type": "close"}),
        }
    }
}

/// Why the machine rejected an update or an input. These are caller-side
/// conditions, distinct from [`AuthRejection`] — TDLib's answer to a
/// request that did go out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// The input is not valid in the current state (a code before a phone
    /// number, a resend while waiting for the password).
    InvalidInput {
        /// The current state's [`AuthState::kind`].
        state: &'static str,
        /// The rejected input's [`AuthInput::kind`].
        input: &'static str,
    },
    /// The flow sits in a TDLib state this machine does not support; only
    /// [`AuthInput::Cancel`] is accepted there.
    UnsupportedState {
        /// TDLib's `authorization_state.@type` — diagnostic, not
        /// contractual.
        td_type: String,
    },
    /// An `updateAuthorizationState` event that could not be interpreted;
    /// the state is unchanged.
    MalformedUpdate {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::InvalidInput { state, input } => {
                write!(f, "input '{input}' is not valid in auth state '{state}'")
            }
            AuthError::UnsupportedState { td_type } => {
                write!(f, "unsupported TDLib authorization state: {td_type}")
            }
            AuthError::MalformedUpdate { detail } => {
                write!(f, "malformed authorization update: {detail}")
            }
        }
    }
}

impl std::error::Error for AuthError {}

/// TDLib's typed answer to an authorization request it refused, classified
/// from the [`TdError`] the submission resolved to. The named variants are
/// Telegram's contractual error identifiers; everything else lands in a
/// typed passthrough, never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRejection {
    /// `PHONE_NUMBER_INVALID` — the number cannot be signed in.
    InvalidPhoneNumber,
    /// `PHONE_NUMBER_BANNED` — the number is banned from Telegram.
    PhoneNumberBanned,
    /// `PHONE_CODE_INVALID`/`PHONE_CODE_EMPTY` — the code is wrong.
    InvalidCode,
    /// `PHONE_CODE_EXPIRED` — the code lapsed; a fresh one must be sent.
    ExpiredCode,
    /// `PASSWORD_HASH_INVALID` — the 2FA password is wrong.
    InvalidPassword,
    /// Flood control (code 429 / `FLOOD_WAIT`): retry after the stated
    /// delay.
    RateLimited {
        /// The wait Telegram stated, when its message carried one.
        retry_after_secs: Option<u64>,
    },
    /// A transport-level failure (TDLib code 500 — connection loss,
    /// timeouts). The flow position is unchanged; the same input may simply
    /// be retried.
    Network,
    /// The client or runtime ended underneath the flow; no retry can
    /// succeed on this session.
    SessionEnded,
    /// Any other failure, passed through typed.
    Other {
        /// TDLib's numeric error code (0 for runtime-minted failures).
        code: i64,
        /// Diagnostic detail; not contractual.
        message: String,
    },
}

impl AuthRejection {
    /// Classify a failed authorization request.
    ///
    /// The uppercase identifiers matched here are Telegram API error
    /// identifiers — contractual, unlike the free-text messages around
    /// them. Code 500 groups TDLib's transport failures ("Failed to
    /// connect", "Timeout expired"): for this flow the right reading is
    /// "the network let you down, retry", and a genuine internal error
    /// misread that way costs one failed retry, not a wrong transition.
    pub fn classify(error: &TdError) -> AuthRejection {
        match error {
            TdError::Td { code, message } => match message.as_str() {
                "PHONE_NUMBER_INVALID" => AuthRejection::InvalidPhoneNumber,
                "PHONE_NUMBER_BANNED" => AuthRejection::PhoneNumberBanned,
                "PHONE_CODE_INVALID" | "PHONE_CODE_EMPTY" => AuthRejection::InvalidCode,
                "PHONE_CODE_EXPIRED" => AuthRejection::ExpiredCode,
                "PASSWORD_HASH_INVALID" => AuthRejection::InvalidPassword,
                _ if *code == 429
                    || message.starts_with("Too Many Requests")
                    || message.starts_with("FLOOD_WAIT") =>
                {
                    AuthRejection::RateLimited {
                        retry_after_secs: trailing_integer(message),
                    }
                }
                _ if *code == 500 => AuthRejection::Network,
                _ => AuthRejection::Other {
                    code: *code,
                    message: message.clone(),
                },
            },
            TdError::ClientClosed | TdError::Shutdown => AuthRejection::SessionEnded,
            TdError::InvalidRequest { detail } | TdError::Protocol { detail } => {
                AuthRejection::Other {
                    code: 0,
                    message: detail.clone(),
                }
            }
        }
    }

    /// What the caller can do about this rejection — the typed form of the
    /// story's "explicit UX/error mapping".
    pub fn advice(&self) -> RetryAdvice {
        match self {
            AuthRejection::InvalidPhoneNumber
            | AuthRejection::InvalidCode
            | AuthRejection::InvalidPassword => RetryAdvice::ReviseInput,
            AuthRejection::ExpiredCode => RetryAdvice::RequestNewCode,
            AuthRejection::RateLimited { retry_after_secs } => RetryAdvice::WaitThenRetry {
                after_secs: *retry_after_secs,
            },
            AuthRejection::Network => RetryAdvice::RetrySameInput,
            AuthRejection::PhoneNumberBanned
            | AuthRejection::SessionEnded
            | AuthRejection::Other { .. } => RetryAdvice::Abort,
        }
    }
}

impl std::fmt::Display for AuthRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthRejection::InvalidPhoneNumber => write!(f, "the phone number was rejected"),
            AuthRejection::PhoneNumberBanned => write!(f, "the phone number is banned"),
            AuthRejection::InvalidCode => write!(f, "the login code is wrong"),
            AuthRejection::ExpiredCode => write!(f, "the login code expired"),
            AuthRejection::InvalidPassword => write!(f, "the password is wrong"),
            AuthRejection::RateLimited { retry_after_secs } => match retry_after_secs {
                Some(secs) => write!(f, "rate limited; retry after {secs}s"),
                None => write!(f, "rate limited"),
            },
            AuthRejection::Network => write!(f, "network failure; retry"),
            AuthRejection::SessionEnded => write!(f, "the session ended"),
            AuthRejection::Other { code, message } => {
                write!(f, "authorization failed ({code}): {message}")
            }
        }
    }
}

impl std::error::Error for AuthRejection {}

/// What the caller should do next after a rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAdvice {
    /// Transient failure: submit the very same input again.
    RetrySameInput,
    /// The value was wrong: ask the user to correct it and resubmit.
    ReviseInput,
    /// The code lapsed: send [`AuthInput::ResendCode`], then the new code.
    RequestNewCode,
    /// Flood control: wait (for `after_secs` when stated), then retry.
    WaitThenRetry {
        /// The wait Telegram stated, when known.
        after_secs: Option<u64>,
    },
    /// Not recoverable inside this flow; surface it and stop.
    Abort,
}

/// The effect of one update: the state entered (if the update was an
/// authorization transition) and the requests the caller must now submit,
/// in order.
#[derive(Debug)]
pub struct AuthStep {
    /// The state this update moved the machine into — already applied;
    /// `None` for updates that are not authorization transitions. TDLib may
    /// re-enter the current state with fresh details (a new QR link, a
    /// re-sent code's info), so `Some` does not imply the variant changed.
    pub entered: Option<AuthState>,
    /// Requests to submit to the client, in order. Non-empty only for the
    /// configuration answer today; a caller must not assume that.
    pub requests: Vec<Value>,
}

impl AuthStep {
    fn ignored() -> AuthStep {
        AuthStep {
            entered: None,
            requests: Vec::new(),
        }
    }
}

/// The deterministic authorization state machine for one account's client.
/// Sans-IO; the caller owns the wiring (module docs).
#[derive(Debug)]
pub struct AuthMachine {
    config: TdlibConfig,
    state: AuthState,
}

impl AuthMachine {
    /// A machine for one account, starting before any reported state. The
    /// config supplies the startup answer to TDLib's parameters request.
    pub fn new(config: TdlibConfig) -> AuthMachine {
        AuthMachine {
            config,
            state: AuthState::Starting,
        }
    }

    /// The current typed state.
    pub fn state(&self) -> &AuthState {
        &self.state
    }

    /// Feed one update from the client's stream.
    ///
    /// Updates that are not `updateAuthorizationState` are ignored (an
    /// [`AuthStep`] with nothing in it) — the caller pumps the whole
    /// stream through without pre-filtering. An authorization update moves
    /// the typed state and may carry requests to submit; one whose shape
    /// cannot be interpreted is a typed [`AuthError::MalformedUpdate`] and
    /// leaves the state unchanged. Never panics.
    pub fn on_update(&mut self, update: &Value) -> Result<AuthStep, AuthError> {
        if update.get("@type").and_then(Value::as_str) != Some("updateAuthorizationState") {
            return Ok(AuthStep::ignored());
        }
        let Some(auth_state) = update.get("authorization_state") else {
            return Err(AuthError::MalformedUpdate {
                detail: "updateAuthorizationState without an authorization_state member".to_owned(),
            });
        };
        let Some(td_type) = auth_state.get("@type").and_then(Value::as_str) else {
            return Err(AuthError::MalformedUpdate {
                detail: "authorization_state without a string @type".to_owned(),
            });
        };
        let entered = AuthState::from_td(td_type, auth_state);
        let requests = if entered == AuthState::Configuring {
            self.config.startup_requests()
        } else {
            Vec::new()
        };
        self.state = entered.clone();
        Ok(AuthStep {
            entered: Some(entered),
            requests,
        })
    }

    /// Turn a user action into the one request to submit, or a typed error
    /// when the action is not valid in the current state. The state does
    /// not move here — TDLib's next update confirms (or denies) progress.
    pub fn on_input(&self, input: AuthInput) -> Result<Value, AuthError> {
        let valid = match (&self.state, &input) {
            (
                AuthState::WaitPhoneNumber,
                AuthInput::SubmitPhoneNumber { .. } | AuthInput::RequestQrCode,
            ) => true,
            (AuthState::WaitCode(_), AuthInput::SubmitCode { .. } | AuthInput::ResendCode) => true,
            (AuthState::WaitPassword(_), AuthInput::SubmitPassword { .. }) => true,
            // Cancel works from anywhere there is still a client to close.
            (AuthState::Closed, AuthInput::Cancel) => false,
            (_, AuthInput::Cancel) => true,
            _ => false,
        };
        if !valid {
            if let AuthState::Unsupported { td_type } = &self.state {
                return Err(AuthError::UnsupportedState {
                    td_type: td_type.clone(),
                });
            }
            return Err(AuthError::InvalidInput {
                state: self.state.kind(),
                input: input.kind(),
            });
        }
        Ok(input.request())
    }
}

#[cfg(test)]
mod tests {
    use gramdrive_model::identity::AccountId;

    use super::*;
    use crate::config::{
        AccountConfig, ApiCredentials, DatabaseKey, InMemorySecrets, StorageLayout,
    };

    fn machine() -> AuthMachine {
        let secrets = InMemorySecrets::new(ApiCredentials {
            api_id: 424242,
            api_hash: Secret::new("api-hash-sentinel"),
        })
        .with_key(AccountId(7), DatabaseKey::from_bytes(b"key".to_vec()));
        let layout = StorageLayout::new("/root");
        let config = AccountConfig::mirror(AccountId(7), &layout)
            .resolve(&secrets)
            .unwrap();
        AuthMachine::new(config)
    }

    fn auth_update(state: Value) -> Value {
        json!({"@type": "updateAuthorizationState", "authorization_state": state})
    }

    /// Drive `machine` into `state` through a normal update.
    fn enter(machine: &mut AuthMachine, state: Value) {
        machine.on_update(&auth_update(state)).unwrap();
    }

    #[test]
    fn configuration_request_answers_the_parameters_state() {
        let mut machine = machine();
        let step = machine
            .on_update(&auth_update(
                json!({"@type": "authorizationStateWaitTdlibParameters"}),
            ))
            .unwrap();
        assert_eq!(step.entered, Some(AuthState::Configuring));
        // The full ordered startup sequence: parameters, then the options.
        assert_eq!(step.requests[0]["@type"], "setTdlibParameters");
        assert_eq!(step.requests.len(), 6);
        // Correlation ids belong to the runtime; the machine must not mint
        // any.
        assert!(step.requests.iter().all(|r| r.get("@extra").is_none()));
    }

    #[test]
    fn non_auth_updates_are_ignored_without_a_transition() {
        let mut machine = machine();
        enter(
            &mut machine,
            json!({"@type": "authorizationStateWaitPhoneNumber"}),
        );
        let step = machine
            .on_update(&json!({"@type": "updateConnectionState",
                "state": {"@type": "connectionStateWaitingForNetwork"}}))
            .unwrap();
        assert!(step.entered.is_none());
        assert!(step.requests.is_empty());
        assert_eq!(machine.state(), &AuthState::WaitPhoneNumber);
    }

    #[test]
    fn wait_code_extracts_the_code_info_and_degrades_safely() {
        let mut machine = machine();
        enter(
            &mut machine,
            json!({"@type": "authorizationStateWaitCode", "code_info": {
                "phone_number": "+15550100",
                "type": {"@type": "authenticationCodeTypeSms", "length": 5},
                "timeout": 120,
            }}),
        );
        assert_eq!(
            machine.state(),
            &AuthState::WaitCode(CodeInfo {
                phone_number: "+15550100".to_owned(),
                code_length: Some(5),
                resend_timeout_secs: Some(120),
            })
        );

        // A degraded report — no code_info at all — is still the state.
        enter(&mut machine, json!({"@type": "authorizationStateWaitCode"}));
        assert_eq!(
            machine.state(),
            &AuthState::WaitCode(CodeInfo {
                phone_number: String::new(),
                code_length: None,
                resend_timeout_secs: None,
            })
        );
    }

    #[test]
    fn wait_password_and_qr_states_carry_their_display_material() {
        let mut machine = machine();
        enter(
            &mut machine,
            json!({"@type": "authorizationStateWaitPassword",
                "password_hint": "the usual",
                "has_recovery_email_address": true}),
        );
        assert_eq!(
            machine.state(),
            &AuthState::WaitPassword(PasswordInfo {
                hint: "the usual".to_owned(),
                has_recovery_email: true,
            })
        );

        enter(
            &mut machine,
            json!({"@type": "authorizationStateWaitOtherDeviceConfirmation",
                "link": "tg://login?token=abc"}),
        );
        assert_eq!(
            machine.state(),
            &AuthState::WaitQrConfirmation {
                link: "tg://login?token=abc".to_owned(),
            }
        );
    }

    #[test]
    fn unknown_states_become_typed_unsupported_never_a_panic() {
        let mut machine = machine();
        for td_type in [
            "authorizationStateWaitEmailAddress",
            "authorizationStateWaitEmailCode",
            "authorizationStateWaitRegistration",
            "authorizationStateInventedTomorrow",
        ] {
            enter(&mut machine, json!({"@type": td_type}));
            assert_eq!(
                machine.state(),
                &AuthState::Unsupported {
                    td_type: td_type.to_owned(),
                }
            );
        }
    }

    #[test]
    fn malformed_auth_updates_are_typed_errors_and_leave_state_alone() {
        let mut machine = machine();
        enter(
            &mut machine,
            json!({"@type": "authorizationStateWaitPhoneNumber"}),
        );

        let missing_state = json!({"@type": "updateAuthorizationState"});
        assert!(matches!(
            machine.on_update(&missing_state),
            Err(AuthError::MalformedUpdate { .. })
        ));
        let unusable_type =
            json!({"@type": "updateAuthorizationState", "authorization_state": {"@type": 7}});
        assert!(matches!(
            machine.on_update(&unusable_type),
            Err(AuthError::MalformedUpdate { .. })
        ));
        assert_eq!(machine.state(), &AuthState::WaitPhoneNumber);
    }

    #[test]
    fn resume_mid_flow_needs_no_history() {
        // A fresh machine fed a mid-flow state (restart during sign-in) is
        // immediately in step.
        let mut machine = machine();
        enter(&mut machine, json!({"@type": "authorizationStateWaitCode"}));
        assert!(matches!(machine.state(), AuthState::WaitCode(_)));
        assert!(
            machine
                .on_input(AuthInput::SubmitCode {
                    code: Secret::new("13579"),
                })
                .is_ok()
        );
    }

    #[test]
    fn input_validity_follows_the_state_table() {
        let mut machine = machine();

        // Starting: nothing but cancel.
        assert!(matches!(
            machine.on_input(AuthInput::SubmitPhoneNumber {
                phone_number: "+15550100".to_owned(),
            }),
            Err(AuthError::InvalidInput {
                state: "starting",
                input: "submit-phone-number",
            })
        ));
        assert_eq!(
            machine.on_input(AuthInput::Cancel).unwrap()["@type"],
            "close"
        );

        enter(
            &mut machine,
            json!({"@type": "authorizationStateWaitPhoneNumber"}),
        );
        let request = machine
            .on_input(AuthInput::SubmitPhoneNumber {
                phone_number: "+15550100".to_owned(),
            })
            .unwrap();
        assert_eq!(request["@type"], "setAuthenticationPhoneNumber");
        assert_eq!(request["phone_number"], "+15550100");
        assert_eq!(
            machine.on_input(AuthInput::RequestQrCode).unwrap()["@type"],
            "requestQrCodeAuthentication"
        );
        assert!(machine.on_input(AuthInput::ResendCode).is_err());

        enter(&mut machine, json!({"@type": "authorizationStateWaitCode"}));
        let request = machine
            .on_input(AuthInput::SubmitCode {
                code: Secret::new("13579"),
            })
            .unwrap();
        assert_eq!(request["@type"], "checkAuthenticationCode");
        assert_eq!(request["code"], "13579");
        assert_eq!(
            machine.on_input(AuthInput::ResendCode).unwrap()["@type"],
            "resendAuthenticationCode"
        );
        assert!(
            machine
                .on_input(AuthInput::SubmitPassword {
                    password: Secret::new("pw"),
                })
                .is_err()
        );

        enter(
            &mut machine,
            json!({"@type": "authorizationStateWaitPassword"}),
        );
        let request = machine
            .on_input(AuthInput::SubmitPassword {
                password: Secret::new("pw-sentinel"),
            })
            .unwrap();
        assert_eq!(request["@type"], "checkAuthenticationPassword");
        assert_eq!(request["password"], "pw-sentinel");

        // Ready: the flow is over; inputs are refused, cancel still closes.
        enter(&mut machine, json!({"@type": "authorizationStateReady"}));
        assert!(machine.on_input(AuthInput::ResendCode).is_err());
        assert_eq!(
            machine.on_input(AuthInput::Cancel).unwrap()["@type"],
            "close"
        );

        // Closed: there is no client left to close.
        enter(&mut machine, json!({"@type": "authorizationStateClosed"}));
        assert!(matches!(
            machine.on_input(AuthInput::Cancel),
            Err(AuthError::InvalidInput {
                state: "closed",
                input: "cancel",
            })
        ));
    }

    #[test]
    fn unsupported_state_accepts_only_cancel_with_a_typed_error() {
        let mut machine = machine();
        enter(
            &mut machine,
            json!({"@type": "authorizationStateWaitEmailAddress"}),
        );
        assert_eq!(
            machine.on_input(AuthInput::SubmitCode {
                code: Secret::new("13579"),
            }),
            Err(AuthError::UnsupportedState {
                td_type: "authorizationStateWaitEmailAddress".to_owned(),
            })
        );
        assert_eq!(
            machine.on_input(AuthInput::Cancel).unwrap()["@type"],
            "close"
        );
    }

    #[test]
    fn rejection_classification_covers_the_contractual_identifiers() {
        let td = |code: i64, message: &str| TdError::Td {
            code,
            message: message.to_owned(),
        };
        let cases = [
            (
                td(400, "PHONE_NUMBER_INVALID"),
                AuthRejection::InvalidPhoneNumber,
            ),
            (
                td(400, "PHONE_NUMBER_BANNED"),
                AuthRejection::PhoneNumberBanned,
            ),
            (td(400, "PHONE_CODE_INVALID"), AuthRejection::InvalidCode),
            (td(400, "PHONE_CODE_EMPTY"), AuthRejection::InvalidCode),
            (td(400, "PHONE_CODE_EXPIRED"), AuthRejection::ExpiredCode),
            (
                td(400, "PASSWORD_HASH_INVALID"),
                AuthRejection::InvalidPassword,
            ),
            (
                td(429, "Too Many Requests: retry after 17"),
                AuthRejection::RateLimited {
                    retry_after_secs: Some(17),
                },
            ),
            (
                td(420, "FLOOD_WAIT_120"),
                AuthRejection::RateLimited {
                    retry_after_secs: Some(120),
                },
            ),
            (
                td(500, "Failed to connect to Telegram"),
                AuthRejection::Network,
            ),
            (
                td(406, "UPDATE_APP_TO_LOGIN"),
                AuthRejection::Other {
                    code: 406,
                    message: "UPDATE_APP_TO_LOGIN".to_owned(),
                },
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(AuthRejection::classify(&error), expected, "{error}");
        }
        assert_eq!(
            AuthRejection::classify(&TdError::ClientClosed),
            AuthRejection::SessionEnded
        );
        assert_eq!(
            AuthRejection::classify(&TdError::Shutdown),
            AuthRejection::SessionEnded
        );
        assert_eq!(
            AuthRejection::classify(&TdError::Protocol {
                detail: "detail".to_owned(),
            }),
            AuthRejection::Other {
                code: 0,
                message: "detail".to_owned(),
            }
        );
    }

    #[test]
    fn advice_maps_every_rejection_to_a_next_step() {
        let cases = [
            (AuthRejection::InvalidPhoneNumber, RetryAdvice::ReviseInput),
            (AuthRejection::InvalidCode, RetryAdvice::ReviseInput),
            (AuthRejection::InvalidPassword, RetryAdvice::ReviseInput),
            (AuthRejection::ExpiredCode, RetryAdvice::RequestNewCode),
            (
                AuthRejection::RateLimited {
                    retry_after_secs: Some(9),
                },
                RetryAdvice::WaitThenRetry {
                    after_secs: Some(9),
                },
            ),
            (AuthRejection::Network, RetryAdvice::RetrySameInput),
            (AuthRejection::PhoneNumberBanned, RetryAdvice::Abort),
            (AuthRejection::SessionEnded, RetryAdvice::Abort),
            (
                AuthRejection::Other {
                    code: 1,
                    message: String::new(),
                },
                RetryAdvice::Abort,
            ),
        ];
        for (rejection, expected) in cases {
            assert_eq!(rejection.advice(), expected, "{rejection}");
        }
    }

    #[test]
    fn credential_inputs_redact_under_debug() {
        let code = AuthInput::SubmitCode {
            code: Secret::new("13579-sentinel"),
        };
        let password = AuthInput::SubmitPassword {
            password: Secret::new("pw-sentinel"),
        };
        for (input, sentinel) in [(&code, "13579"), (&password, "pw-sentinel")] {
            let rendered = format!("{input:?}");
            assert!(!rendered.contains(sentinel), "{rendered}");
            assert!(rendered.contains("<redacted>"), "{rendered}");
        }
    }
}
