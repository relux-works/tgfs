//! The exported authorization surface: one sign-in session, driven by the
//! engine-hosting process (BUG-260720-3i74u1).
//!
//! # Shape
//!
//! [`AuthSession`] hosts one TDLib client for one interactive sign-in: the
//! host constructs it with an [`AuthSessionConfig`], a [`SecretVault`] (the
//! OS-keychain seam — DEC-002 keeps keychain code native), and an
//! [`AuthStateListener`] that receives every [`AuthPhase`] the flow enters.
//! Inputs go through [`AuthSession::submit`]; the reported phase stream is
//! the single source of truth for the caller's UI, exactly as TDLib's
//! reported state is for the underlying machine
//! (`gramdrive-source-tdjson::auth`). The vocabulary here is
//! provider-neutral (DEC-003): it mirrors the source crate's typed auth
//! vocabulary and adds the session-level phases ([`AuthPhase::Finalizing`],
//! [`AuthPhase::Complete`], [`AuthPhase::Failed`]) that exist only at this
//! boundary.
//!
//! # The sign-in slot and finalization
//!
//! A sign-in starts before the account's Telegram identity is known, but
//! TDLib needs a database directory up front. The session therefore runs in
//! a provisional *slot* — `account-0` under `<data_dir>/telegram`, an id no
//! real Telegram account can carry — and *finalizes* once TDLib reports
//! `Ready`: read the identity (`getMe`), close the client cleanly, move the
//! slot's storage to `account-<id>`, re-home the database key in the vault,
//! and upsert the account row (and its root item) into shared durable
//! state, so provider processes see the account immediately. Session
//! persistence across restarts is TDLib's own database, now at its final
//! per-account path; [`probe_authorization`] proves it by opening the
//! account's client and observing `Ready` without any user input.
//!
//! A fresh session wipes any stale slot first (a crash mid-sign-in leaves
//! at most a half-finished slot, never a half-finished account), and one
//! sign-in per data root runs at a time.
//!
//! # Build variants
//!
//! The real tdjson runtime is claimed once per process, under the same
//! `GRAMDRIVE_TDLIB_ARTIFACT_DIR` env gate as the source crate
//! (`build.rs`). Hermetic builds — every `make check` run — compile this
//! module without the real linkage, and [`AuthSession::start`] truthfully
//! fails with [`DriveError::SourceUnavailable`]; the in-crate tests drive
//! the full surface over the deterministic mock instead.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, CanonicalKey, ItemId, ItemKey, NamespaceVersion,
};
use gramdrive_model::version::MetadataVersion;
use gramdrive_source_tdjson::auth::{AuthError, AuthInput, AuthMachine, AuthRejection, AuthState};
use gramdrive_source_tdjson::config::{
    AccountConfig, ApiCredentials, DatabaseKey, DeviceMetadata, Secret, SecretError, SecretSource,
    StorageLayout, TdlibConfig,
};
use gramdrive_source_tdjson::error::TdError;
use gramdrive_source_tdjson::runtime::{TdClient, TdRuntime, UpdateRecvError, UpdateStream};
use gramdrive_state::StateStore;
use gramdrive_state::repo::{
    AccountRecord, AuthFinalizationPhase, AuthFinalizationRecord, ItemAvailability, ItemRecord,
    RetentionMode, SourceKind, WriteTxn,
};
use serde_json::{Value, json};

use crate::api::DriveError;
use crate::shared_state::{shared_state_layout, upsert_fixed_root_structure};

/// The provisional account id a sign-in runs under until the account's real
/// Telegram identity is known. No real Telegram account carries id 0.
const SIGN_IN_SLOT: AccountId = AccountId(0);

/// How long the session waits for TDLib to answer one plumbing request
/// (startup configuration, `getMe`, close).
const PLUMBING_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`AuthSession::submit`] waits for TDLib's answer to a user
/// input before classifying the silence as a network failure.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(75);

/// The subtree of the data root that holds per-account Telegram state
/// (`<data_dir>/telegram/account-<id>/{tdlib,files}`), beside — never
/// inside — the core-owned `state/` and `cache/` layout.
const TELEGRAM_SUBTREE: &str = "telegram";

// MARK: - Exported vocabulary

/// Product API credentials handed across the vault seam. Transport shape
/// only: the Rust side wraps the hash into the source crate's redacting
/// `Secret` immediately and never logs it.
#[derive(Clone, uniffi::Record)]
pub struct VaultApiCredentials {
    /// The product `api_id`.
    pub api_id: i32,
    /// The product `api_hash`.
    pub api_hash: String,
}

// Manual: the hash is secret material and must never reach a log through a
// derived Debug (the same rule the source crate's ApiCredentials follows).
impl std::fmt::Debug for VaultApiCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultApiCredentials")
            .field("api_id", &self.api_id)
            .field("api_hash", &"<redacted>")
            .finish()
    }
}

/// The OS-keychain seam (SEC-003): api credentials and per-account database
/// keys are read at runtime from the host's secret store, never from
/// configuration. Implemented natively (Security.framework on Apple);
/// called from session background threads — implementations must be
/// thread-safe and must not block indefinitely.
#[uniffi::export(with_foreign)]
pub trait SecretVault: Send + Sync {
    /// The product `api_id`/`api_hash`.
    fn api_credentials(&self) -> Result<VaultApiCredentials, DriveError>;
    /// The stored database key for `account_id`, or `None` when the account
    /// has none. Never creates.
    fn database_key(&self, account_id: i64) -> Result<Option<Vec<u8>>, DriveError>;
    /// The database key for `account_id`, creating (and durably storing) a
    /// fresh 32-byte key from platform entropy when none exists.
    fn ensure_database_key(&self, account_id: i64) -> Result<Vec<u8>, DriveError>;
    /// Stores (creating or overwriting) `account_id`'s database key.
    fn store_database_key(&self, account_id: i64, key: Vec<u8>) -> Result<(), DriveError>;
    /// Removes `account_id`'s database key. Missing is success.
    fn delete_database_key(&self, account_id: i64) -> Result<(), DriveError>;
}

/// What the code-entry step renders. Mirror of the source crate's
/// `auth::CodeInfo`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AuthCodeInfo {
    /// The phone number the code was sent to (TDLib echoes it in clear).
    pub phone_number: String,
    /// Expected code length, when the delivery method states one.
    pub code_length: Option<i64>,
    /// Seconds before a resend is allowed, when TDLib states it.
    pub resend_timeout_secs: Option<i64>,
}

/// What the 2FA password step renders. Mirror of `auth::PasswordInfo`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AuthPasswordInfo {
    /// The user's own password hint (may be empty); display material.
    pub hint: String,
    /// Whether a recovery email is configured.
    pub has_recovery_email: bool,
}

/// One session-level phase of the sign-in flow, reported through
/// [`AuthStateListener`]. The TDLib-reported states mirror
/// `auth::AuthState`; `Finalizing`/`Complete`/`Failed` are session-level:
/// they cover the storage/identity finalization that runs after TDLib
/// reports `Ready` and before the account exists durably.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AuthPhase {
    /// The session exists; TDLib has not reported a state yet.
    Starting,
    /// TDLib is being configured (parameters answered). Not a user-facing
    /// wait.
    Configuring,
    /// Waiting for the phone number — or a switch to QR sign-in.
    WaitPhoneNumber,
    /// A login code was sent.
    WaitCode {
        /// Rendering material for the code step.
        info: AuthCodeInfo,
    },
    /// Waiting for another logged-in device to confirm the QR link.
    WaitQrConfirmation {
        /// The `tg://login` link to render as a QR code.
        link: String,
    },
    /// Waiting for the account's 2FA password.
    WaitPassword {
        /// Rendering material for the password step.
        info: AuthPasswordInfo,
    },
    /// Authorized; the session is persisting the account (identity read,
    /// storage move, durable account row). Not a user-facing wait.
    Finalizing,
    /// The sign-in completed and the account exists durably; the session is
    /// over.
    Complete {
        /// The account's stable Telegram identity.
        account_id: i64,
        /// The account's display name, for immediate rendering.
        display_name: String,
    },
    /// The account is being logged out.
    LoggingOut,
    /// The client is closing.
    Closing,
    /// The client closed; the session ended without completing.
    Closed,
    /// TDLib reported a state outside the supported v1 sign-in scope. Only
    /// cancel is accepted.
    Unsupported {
        /// The reported state's stable diagnostic name.
        kind: String,
    },
    /// The flow failed after authorization (finalization could not persist
    /// the account); the session is over and a fresh sign-in is required.
    Failed {
        /// Stable, redacted failure code; diagnostic, not contractual.
        detail: String,
    },
}

/// One user action in the sign-in flow. Mirror of `auth::AuthInput`; the
/// code and password are plain strings at this boundary and are wrapped
/// into the source crate's redacting `Secret` immediately on entry.
#[derive(Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AuthCommand {
    /// Submit the phone number.
    SubmitPhoneNumber {
        /// The number, in international format.
        phone_number: String,
    },
    /// Switch to QR sign-in.
    RequestQrCode,
    /// Submit the login code.
    SubmitCode {
        /// The code the user typed.
        code: String,
    },
    /// Request a fresh login code.
    ResendCode,
    /// Submit the 2FA password.
    SubmitPassword {
        /// The password the user typed.
        password: String,
    },
    /// Abandon the flow (closes the client).
    Cancel,
}

// Manual: the login code and 2FA password are secret material and must
// never reach a log through a derived Debug (the same rule
// `VaultApiCredentials` and the source crate's `AuthInput` follow). The
// phone number is display material TDLib itself echoes in clear, so it
// stays visible for diagnostics.
impl std::fmt::Debug for AuthCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthCommand::SubmitPhoneNumber { phone_number } => f
                .debug_struct("SubmitPhoneNumber")
                .field("phone_number", phone_number)
                .finish(),
            AuthCommand::RequestQrCode => f.write_str("RequestQrCode"),
            AuthCommand::SubmitCode { .. } => f
                .debug_struct("SubmitCode")
                .field("code", &"<redacted>")
                .finish(),
            AuthCommand::ResendCode => f.write_str("ResendCode"),
            AuthCommand::SubmitPassword { .. } => f
                .debug_struct("SubmitPassword")
                .field("password", &"<redacted>")
                .finish(),
            AuthCommand::Cancel => f.write_str("Cancel"),
        }
    }
}

/// TDLib's typed answer to a sign-in request it refused. Mirror of
/// `auth::AuthRejection`.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AuthRejectionInfo {
    /// The phone number was rejected.
    InvalidPhoneNumber,
    /// The phone number is banned from Telegram.
    PhoneNumberBanned,
    /// The login code is wrong.
    InvalidCode,
    /// The login code expired; request a new one.
    ExpiredCode,
    /// The 2FA password is wrong.
    InvalidPassword,
    /// Flood control; wait, then retry.
    RateLimited {
        /// Source-stated minimum wait, when it supplied one.
        retry_after_secs: Option<u64>,
    },
    /// A transient network failure; the same input may be retried.
    Network,
    /// The sign-in session ended (client closed under the flow).
    SessionEnded,
    /// Any other TDLib rejection.
    Other {
        /// TDLib's numeric error code.
        code: i64,
        /// TDLib's message; diagnostic, not contractual.
        detail: String,
    },
}

/// The outcome of one [`AuthSession::submit`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AuthSubmitOutcome {
    /// TDLib accepted the request; the next phase arrives via the listener.
    Accepted,
    /// TDLib refused the request, classified. The flow position is
    /// unchanged; the rejection's advice says what to do.
    Rejected {
        /// The classified rejection.
        rejection: AuthRejectionInfo,
    },
    /// The input is not valid in the current phase (the flow position is
    /// unchanged, nothing was sent).
    InvalidForState,
}

/// The outcome of probing an existing account's stored authorization.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum AuthProbeOutcome {
    /// The stored session is live: TDLib reached `Ready` without input.
    Authorized {
        /// The account id the probe confirmed (from `getMe`).
        account_id: i64,
        /// The account's current display name, when readable.
        display_name: Option<String>,
    },
    /// The stored session cannot authorize without user input; a fresh
    /// sign-in is required.
    SignedOut {
        /// The state the probe observed instead of `Ready` (stable
        /// diagnostic name).
        kind: String,
    },
}

/// Receives every phase the sign-in flow enters, in order. Calls arrive
/// synchronously on a session background thread — never a platform main
/// thread; implementations must be thread-safe, return quickly, and never
/// throw (the standard callback contract, README.md § Callback dispatch).
#[uniffi::export(with_foreign)]
pub trait AuthStateListener: Send + Sync {
    /// One phase transition.
    fn on_phase(&self, phase: AuthPhase);
}

/// Host-supplied configuration for one sign-in session or probe.
#[derive(Debug, Clone, uniffi::Record)]
pub struct AuthSessionConfig {
    /// The data root every GramDrive process shares (same value as
    /// `CoreConfig.data_dir`). Must be non-empty.
    pub data_dir: String,
    /// Whether to connect to Telegram's test data centers.
    pub use_test_dc: bool,
    /// `device_model` disclosed to Telegram (SEC-030); must be truthful.
    pub device_model: String,
    /// `system_version` disclosed to Telegram.
    pub system_version: String,
    /// The shipped product version.
    pub application_version: String,
    /// BCP-47 language code.
    pub system_language_code: String,
}

// MARK: - Internal plumbing

// Crate-internal plumbing, shared with `crate::removal`.
impl AuthSessionConfig {
    fn device_metadata(&self) -> DeviceMetadata {
        DeviceMetadata {
            device_model: self.device_model.clone(),
            system_version: self.system_version.clone(),
            application_version: self.application_version.clone(),
            system_language_code: self.system_language_code.clone(),
        }
    }

    pub(crate) fn storage_layout(&self) -> StorageLayout {
        StorageLayout::new(std::path::Path::new(&self.data_dir).join(TELEGRAM_SUBTREE))
    }

    pub(crate) fn validate(&self) -> Result<(), DriveError> {
        if self.data_dir.is_empty() {
            return Err(DriveError::InvalidArgument {
                detail: "data_dir must be a non-empty directory path".to_owned(),
            });
        }
        Ok(())
    }

    pub(crate) fn tdlib_config(
        &self,
        account: AccountId,
        secrets: &VaultSecrets,
    ) -> Result<TdlibConfig, DriveError> {
        let mut plan = AccountConfig::mirror(account, &self.storage_layout());
        plan.device = self.device_metadata();
        plan.use_test_dc = self.use_test_dc;
        plan.resolve(secrets).map_err(secret_to_drive_error)
    }
}

fn secret_to_drive_error(error: SecretError) -> DriveError {
    match error {
        SecretError::NotFound { what } => DriveError::AuthRequired {
            detail: format!("missing secret: {what}"),
        },
        SecretError::Corrupt { what } => DriveError::Integrity {
            detail: format!("corrupt secret: {what}"),
        },
        SecretError::Backend { detail } => DriveError::Storage {
            detail: format!("secret store: {detail}"),
        },
    }
}

/// Adapts the exported [`SecretVault`] to the source crate's
/// [`SecretSource`]/[`SecretStore`]. `create_missing_key` distinguishes the
/// sign-in slot (fresh key on demand) from probes and removals (a missing
/// key means signed out, and nothing may be created).
pub(crate) struct VaultSecrets {
    vault: Arc<dyn SecretVault>,
    create_missing_key: bool,
}

impl VaultSecrets {
    /// A source that never creates keys — probes and removals.
    pub(crate) fn read_only(vault: Arc<dyn SecretVault>) -> VaultSecrets {
        VaultSecrets {
            vault,
            create_missing_key: false,
        }
    }
}

impl SecretSource for VaultSecrets {
    fn api_credentials(&self) -> Result<ApiCredentials, SecretError> {
        let credentials = self.vault.api_credentials().map_err(backend)?;
        Ok(ApiCredentials {
            api_id: credentials.api_id,
            api_hash: Secret::new(credentials.api_hash),
        })
    }

    fn database_key(&self, account: AccountId) -> Result<DatabaseKey, SecretError> {
        let bytes = if self.create_missing_key {
            self.vault.ensure_database_key(account.0).map_err(backend)?
        } else {
            self.vault
                .database_key(account.0)
                .map_err(backend)?
                .ok_or(SecretError::NotFound {
                    what: "database key",
                })?
        };
        DatabaseKey::from_stored(&bytes)
    }
}

fn backend(error: DriveError) -> SecretError {
    SecretError::Backend {
        detail: error.to_string(),
    }
}

pub(crate) fn td_to_drive_error(error: TdError) -> DriveError {
    match error {
        TdError::Td { code, message } => DriveError::SourceUnavailable {
            detail: format!("TDLib error {code}: {message}"),
        },
        TdError::InvalidRequest { detail } => DriveError::InvalidArgument { detail },
        TdError::ClientClosed => DriveError::Cancelled {
            detail: "the sign-in client is closed".to_owned(),
        },
        TdError::Shutdown => DriveError::SourceUnavailable {
            detail: "the source runtime is shut down".to_owned(),
        },
        TdError::Protocol { detail } => DriveError::Internal { detail },
    }
}

fn rejection_info(rejection: AuthRejection) -> AuthRejectionInfo {
    match rejection {
        AuthRejection::InvalidPhoneNumber => AuthRejectionInfo::InvalidPhoneNumber,
        AuthRejection::PhoneNumberBanned => AuthRejectionInfo::PhoneNumberBanned,
        AuthRejection::InvalidCode => AuthRejectionInfo::InvalidCode,
        AuthRejection::ExpiredCode => AuthRejectionInfo::ExpiredCode,
        AuthRejection::InvalidPassword => AuthRejectionInfo::InvalidPassword,
        AuthRejection::RateLimited { retry_after_secs } => {
            AuthRejectionInfo::RateLimited { retry_after_secs }
        }
        AuthRejection::Network => AuthRejectionInfo::Network,
        AuthRejection::SessionEnded => AuthRejectionInfo::SessionEnded,
        AuthRejection::Other { code, message } => AuthRejectionInfo::Other {
            code,
            detail: message,
        },
    }
}

fn phase_of(state: &AuthState) -> AuthPhase {
    match state {
        AuthState::Starting => AuthPhase::Starting,
        AuthState::Configuring => AuthPhase::Configuring,
        AuthState::WaitPhoneNumber => AuthPhase::WaitPhoneNumber,
        AuthState::WaitCode(info) => AuthPhase::WaitCode {
            info: AuthCodeInfo {
                phone_number: info.phone_number.clone(),
                code_length: info.code_length,
                resend_timeout_secs: info.resend_timeout_secs,
            },
        },
        AuthState::WaitQrConfirmation { link } => {
            AuthPhase::WaitQrConfirmation { link: link.clone() }
        }
        AuthState::WaitPassword(info) => AuthPhase::WaitPassword {
            info: AuthPasswordInfo {
                hint: info.hint.clone(),
                has_recovery_email: info.has_recovery_email,
            },
        },
        AuthState::Ready => AuthPhase::Finalizing,
        AuthState::LoggingOut => AuthPhase::LoggingOut,
        AuthState::Closing => AuthPhase::Closing,
        AuthState::Closed => AuthPhase::Closed,
        AuthState::Unsupported { td_type } => AuthPhase::Unsupported {
            kind: td_type.clone(),
        },
    }
}

fn command_input(command: AuthCommand) -> AuthInput {
    match command {
        AuthCommand::SubmitPhoneNumber { phone_number } => {
            AuthInput::SubmitPhoneNumber { phone_number }
        }
        AuthCommand::RequestQrCode => AuthInput::RequestQrCode,
        AuthCommand::SubmitCode { code } => AuthInput::SubmitCode {
            code: Secret::new(code),
        },
        AuthCommand::ResendCode => AuthInput::ResendCode,
        AuthCommand::SubmitPassword { password } => AuthInput::SubmitPassword {
            password: Secret::new(password),
        },
        AuthCommand::Cancel => AuthInput::Cancel,
    }
}

fn now_ms() -> i64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(now.as_millis()).unwrap_or(i64::MAX)
}

// MARK: - The shared runtime

/// The process's one tdjson runtime. `td_receive` is process-global and
/// single-owner, so the runtime is claimed exactly once and shared by every
/// session and probe for the process's lifetime.
#[cfg(real_tdjson)]
pub(crate) fn shared_runtime() -> Result<Arc<TdRuntime>, DriveError> {
    use gramdrive_source_tdjson::runtime::RuntimeConfig;
    static RUNTIME: OnceLock<Result<Arc<TdRuntime>, String>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            let (sender, receiver) = gramdrive_source_tdjson::real::RealTdJson::claim()
                .ok_or_else(|| "the tdjson receive stream is already claimed".to_owned())?;
            TdRuntime::start(sender, receiver, RuntimeConfig::default())
                .map(Arc::new)
                .map_err(|error| error.to_string())
        })
        .clone()
        .map_err(|detail| DriveError::SourceUnavailable { detail })
}

#[cfg(not(real_tdjson))]
pub(crate) fn shared_runtime() -> Result<Arc<TdRuntime>, DriveError> {
    Err(DriveError::SourceUnavailable {
        detail: "the Telegram runtime is not linked in this build".to_owned(),
    })
}

/// One sign-in (or probe, or removal) per account scope at a time: the slot
/// directory and the per-account TDLib database both refuse concurrent
/// owners.
pub(crate) struct ScopeGuard {
    key: String,
}

static ACTIVE_SCOPES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn active_scopes() -> &'static Mutex<HashSet<String>> {
    ACTIVE_SCOPES.get_or_init(|| Mutex::new(HashSet::new()))
}

impl ScopeGuard {
    pub(crate) fn acquire(data_dir: &str, account: AccountId) -> Result<ScopeGuard, DriveError> {
        // Canonicalize so two spellings of the same root (a trailing slash, a
        // `.`/`..` segment, a symlinked temp dir) resolve to one scope key;
        // fall back to the raw string when the path does not exist yet.
        let root = std::fs::canonicalize(data_dir)
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| data_dir.to_owned());
        let key = format!("{root}#{}", account.0);
        let mut scopes = active_scopes().lock().unwrap_or_else(|e| e.into_inner());
        if !scopes.insert(key.clone()) {
            return Err(DriveError::InvalidArgument {
                detail: "another sign-in or probe is already running for this account".to_owned(),
            });
        }
        Ok(ScopeGuard { key })
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let mut scopes = active_scopes().lock().unwrap_or_else(|e| e.into_inner());
        scopes.remove(&self.key);
    }
}

// MARK: - The session

struct SessionShared {
    machine: Mutex<AuthMachine>,
    client: TdClient,
    listener: Arc<dyn AuthStateListener>,
    vault: Arc<dyn SecretVault>,
    config: AuthSessionConfig,
    database_key: Vec<u8>,
    closed: AtomicBool,
    /// True only after the auth pump has returned and released its
    /// `ScopeGuard`. `closed` means a close was requested or observed;
    /// `pump_finished` is the stronger replacement-session barrier.
    pump_finished: tokio::sync::watch::Sender<bool>,
    // The runtime must outlive the session: dropping the last handle shuts
    // the receive loop down (production's process-wide static also keeps
    // it, but the session must be correct on its own).
    _runtime: Arc<TdRuntime>,
}

/// One interactive sign-in flow, hosted over the process's Telegram
/// runtime. See the module docs for the session shape and the finalization
/// contract.
#[derive(uniffi::Object)]
pub struct AuthSession {
    shared: Arc<SessionShared>,
}

// Manual: the shared internals hold the database key; nothing here needs
// printing beyond identity.
impl std::fmt::Debug for AuthSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSession")
            .field("closed", &self.shared.closed.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

// Defense in depth (Swift call sites do close today): a host that drops the
// session handle without `shutdown()` would otherwise strand the pump thread
// spinning on `recv_timeout` and hold the sign-in slot `ScopeGuard`
// forever, blocking every future sign-in in the process. `shutdown()` is
// idempotent and closes the client, which ends the update stream and
// unwinds the pump — releasing the slot scope the pump owns.
impl Drop for AuthSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl AuthSession {
    /// Starts a sign-in session: wipes any stale sign-in slot, provisions a
    /// fresh slot database key, opens a TDLib client, and begins reporting
    /// phases to `listener`.
    ///
    /// Fails with [`DriveError::SourceUnavailable`] when this build carries
    /// no Telegram runtime, and with [`DriveError::InvalidArgument`] when a
    /// sign-in is already running for this data root.
    #[uniffi::constructor]
    pub fn start(
        config: AuthSessionConfig,
        vault: Arc<dyn SecretVault>,
        listener: Arc<dyn AuthStateListener>,
    ) -> Result<Arc<Self>, DriveError> {
        let runtime = shared_runtime()?;
        Self::start_over(runtime, config, vault, listener)
    }

    /// Submits one user action. `Accepted`/`Rejected`/`InvalidForState`
    /// describe the flow; an `Err` is a channel-level failure (the session
    /// is closed, or the runtime is gone).
    pub async fn submit(&self, command: AuthCommand) -> Result<AuthSubmitOutcome, DriveError> {
        if self.shared.closed.load(Ordering::Acquire) {
            return Err(DriveError::Cancelled {
                detail: "the sign-in session is closed".to_owned(),
            });
        }
        let input = command_input(command);
        let request = {
            let machine = self
                .shared
                .machine
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            machine.on_input(input)
        };
        let request = match request {
            Ok(request) => request,
            Err(AuthError::InvalidInput { .. } | AuthError::UnsupportedState { .. }) => {
                return Ok(AuthSubmitOutcome::InvalidForState);
            }
            Err(AuthError::MalformedUpdate { detail }) => {
                return Err(DriveError::Internal { detail });
            }
        };
        let pending = match self.shared.client.request(request) {
            Ok(pending) => pending,
            Err(TdError::ClientClosed) => {
                return Ok(AuthSubmitOutcome::Rejected {
                    rejection: AuthRejectionInfo::SessionEnded,
                });
            }
            Err(error) => return Err(td_to_drive_error(error)),
        };
        match tokio::time::timeout(SUBMIT_TIMEOUT, pending).await {
            // TDLib is silent past any reasonable answer window; the same
            // input can be retried, which is exactly `Network`'s advice.
            Err(_elapsed) => Ok(AuthSubmitOutcome::Rejected {
                rejection: AuthRejectionInfo::Network,
            }),
            Ok(Ok(_answer)) => Ok(AuthSubmitOutcome::Accepted),
            Ok(Err(TdError::ClientClosed)) => Ok(AuthSubmitOutcome::Rejected {
                rejection: AuthRejectionInfo::SessionEnded,
            }),
            Ok(Err(error @ TdError::Td { .. })) => Ok(AuthSubmitOutcome::Rejected {
                rejection: rejection_info(AuthRejection::classify(&error)),
            }),
            Ok(Err(error)) => Err(td_to_drive_error(error)),
        }
    }

    /// Abandons the flow: closes the client (TDLib confirms with `Closing`
    /// then `Closed`, which the listener still receives). Idempotent.
    pub fn shutdown(&self) {
        if self.shared.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        // Runtime-level close: valid regardless of the machine's position.
        // Dropping the handle is fine — the close request is already sent.
        drop(self.shared.client.close());
    }

    /// Waits until the auth pump has exited and released the single-sign-in
    /// scope. A `Closed` listener callback happens immediately before that
    /// release, so callers replacing a session must use this stronger
    /// barrier instead of treating the callback as proof the slot is free.
    pub async fn wait_closed(&self) {
        let mut finished = self.shared.pump_finished.subscribe();
        if *finished.borrow() {
            return;
        }
        while finished.changed().await.is_ok() {
            if *finished.borrow() {
                return;
            }
        }
    }
}

impl AuthSession {
    /// Starts a session over an explicit runtime — the seam the in-crate
    /// tests use with the deterministic mock.
    pub(crate) fn start_over(
        runtime: Arc<TdRuntime>,
        config: AuthSessionConfig,
        vault: Arc<dyn SecretVault>,
        listener: Arc<dyn AuthStateListener>,
    ) -> Result<Arc<Self>, DriveError> {
        config.validate()?;
        let guard = ScopeGuard::acquire(&config.data_dir, SIGN_IN_SLOT)?;

        // A process may have stopped after preparing or committing an older
        // replacement. Converge that decision before touching the one global
        // sign-in slot: prepared work restores its incumbent, while committed
        // work only removes rollback artifacts.
        recover_all_auth_finalizations(&config, &vault)?;

        // A stale slot is a crashed or abandoned sign-in; a fresh flow must
        // never resume it half-way.
        let layout = config.storage_layout();
        layout
            .wipe_account(SIGN_IN_SLOT)
            .map_err(|error| DriveError::Storage {
                detail: format!("could not clear the sign-in slot: {error}"),
            })?;
        vault.delete_database_key(SIGN_IN_SLOT.0)?;
        let database_key = vault.ensure_database_key(SIGN_IN_SLOT.0)?;

        // TDLib creates its own directories, but a deterministic slot makes
        // finalization's storage move total rather than dependent on how far
        // the client got.
        let slot_paths = layout.account_paths(SIGN_IN_SLOT);
        std::fs::create_dir_all(slot_paths.database_directory()).map_err(|error| {
            DriveError::Storage {
                detail: format!("could not create the sign-in slot: {error}"),
            }
        })?;

        let secrets = VaultSecrets {
            vault: Arc::clone(&vault),
            create_missing_key: true,
        };
        let tdlib_config = config.tdlib_config(SIGN_IN_SLOT, &secrets)?;

        let (client, updates) = runtime.create_client().map_err(td_to_drive_error)?;

        let (pump_finished, _) = tokio::sync::watch::channel(false);
        let shared = Arc::new(SessionShared {
            machine: Mutex::new(AuthMachine::new(tdlib_config)),
            client: client.clone(),
            listener,
            vault,
            config,
            database_key,
            closed: AtomicBool::new(false),
            pump_finished,
            _runtime: runtime,
        });

        // `Starting` is emitted from the pump thread (below), never this
        // constructor's — often a platform main thread, which the listener
        // contract forbids.

        // Activate the client: any request makes TDLib start reporting
        // authorization state. The answer itself is irrelevant.
        drop(client.request(json!({"@type": "getOption", "name": "version"})));

        let pump_shared = Arc::clone(&shared);
        std::thread::Builder::new()
            .name("gramdrive-auth-pump".to_owned())
            .spawn(move || {
                {
                    let _guard = guard;
                    pump(&pump_shared, &updates);
                }
                // Publish completion only after `_guard` has dropped. This
                // ordering is the contract used by replacement sign-ins.
                pump_shared.pump_finished.send_replace(true);
            })
            .map_err(|error| DriveError::Internal {
                detail: format!("could not spawn the auth pump: {error}"),
            })?;

        Ok(Arc::new(Self { shared }))
    }
}

/// The session's update pump: feeds every update into the machine, submits
/// the requests each step demands, reports phase transitions, and runs
/// finalization when TDLib reports `Ready`. Owns the session's end: the
/// pump returning is the session being over.
fn pump(shared: &Arc<SessionShared>, updates: &UpdateStream) {
    // The first phase, emitted on this background thread — never the
    // constructor's (often main) thread (README.md § Callback dispatch).
    shared.listener.on_phase(AuthPhase::Starting);
    loop {
        let update = match updates.recv_timeout(Duration::from_millis(500)) {
            Ok(update) => update,
            Err(UpdateRecvError::Timeout) => continue,
            Err(UpdateRecvError::Closed) => return,
        };
        let step = {
            let mut machine = shared.machine.lock().unwrap_or_else(|e| e.into_inner());
            machine.on_update(&update)
        };
        let step = match step {
            Ok(step) => step,
            // A malformed authorization update leaves the state unchanged;
            // the stream keeps pumping (fail-safe, never wedge).
            Err(_error) => continue,
        };
        for request in step.requests {
            if let Ok(pending) = shared.client.request(request) {
                drop(pending.wait_timeout(PLUMBING_TIMEOUT));
            }
        }
        let Some(entered) = step.entered else {
            continue;
        };
        if entered == AuthState::Ready {
            finalize(shared, updates);
            return;
        }
        shared.listener.on_phase(phase_of(&entered));
        if entered == AuthState::Closed {
            shared.closed.store(true, Ordering::Release);
            return;
        }
    }
}

/// Post-`Ready` finalization: identity, clean close, storage move, vault
/// re-home, durable account row. Emits `Finalizing` then `Complete` — or
/// `Failed` with a stable redacted code when a step cannot complete.
fn finalize(shared: &Arc<SessionShared>, updates: &UpdateStream) {
    shared.listener.on_phase(AuthPhase::Finalizing);
    shared.closed.store(true, Ordering::Release);

    let identity = match read_identity(shared) {
        Ok(identity) => identity,
        Err(code) => return fail(shared, code),
    };

    if !close_and_drain(shared, updates) {
        return fail(shared, "finalize-close");
    }

    if let Err(code) = persist_account(shared, &identity) {
        return fail(shared, code);
    }

    shared.listener.on_phase(AuthPhase::Complete {
        account_id: identity.account_id,
        display_name: identity.display_name.clone(),
    });
}

fn fail(shared: &Arc<SessionShared>, code: &str) {
    shared.listener.on_phase(AuthPhase::Failed {
        detail: code.to_owned(),
    });
}

struct SignedInIdentity {
    account_id: i64,
    display_name: String,
}

/// `getMe` against the freshly authorized client.
fn read_identity(shared: &Arc<SessionShared>) -> Result<SignedInIdentity, &'static str> {
    let pending = shared
        .client
        .request(json!({"@type": "getMe"}))
        .map_err(|_| "finalize-identity")?;
    let user = pending
        .wait_timeout(PLUMBING_TIMEOUT)
        .map_err(|_| "finalize-identity")?
        .map_err(|_| "finalize-identity")?;
    let account_id = user
        .get("id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or("finalize-identity")?;
    Ok(SignedInIdentity {
        account_id,
        display_name: display_name_of(&user),
    })
}

fn display_name_of(user: &Value) -> String {
    let first = user.get("first_name").and_then(Value::as_str).unwrap_or("");
    let last = user.get("last_name").and_then(Value::as_str).unwrap_or("");
    let joined = format!("{first} {last}");
    let trimmed = joined.trim();
    if trimmed.is_empty() {
        "Telegram Account".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Closes the client and drains its updates until TDLib confirms `Closed`
/// (the database is not safe to move before that).
fn close_and_drain(shared: &Arc<SessionShared>, updates: &UpdateStream) -> bool {
    drop(shared.client.close());
    let deadline = std::time::Instant::now() + PLUMBING_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match updates.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(update) => {
                let mut machine = shared.machine.lock().unwrap_or_else(|e| e.into_inner());
                if let Ok(step) = machine.on_update(&update)
                    && step.entered == Some(AuthState::Closed)
                {
                    return true;
                }
            }
            // The runtime closes the stream once the client reports
            // `authorizationStateClosed` — an ended stream IS the
            // confirmation when the update itself was consumed internally.
            Err(UpdateRecvError::Closed) => return true,
            Err(UpdateRecvError::Timeout) => {}
        }
    }
}

/// The storage move, the vault re-home, and the durable account row.
fn persist_account(
    shared: &Arc<SessionShared>,
    identity: &SignedInIdentity,
) -> Result<(), &'static str> {
    persist_account_with_hook(shared, identity, |_| Ok(()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FinalizationStep {
    JournalPrepared,
    KeyBackedUp,
    StorageBackedUp,
    StorageInstalled,
    KeyInstalled,
    SuccessorProven,
    StateCommitted,
}

fn persist_account_with_hook(
    shared: &Arc<SessionShared>,
    identity: &SignedInIdentity,
    mut after_step: impl FnMut(FinalizationStep) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let account = AccountId(identity.account_id);
    let layout = shared.config.storage_layout();

    // Finalization mutates the *real* account's storage, vault key, and
    // durable row. The pump holds only the sign-in *slot* scope (account 0),
    // which does not cover the real id — so take the real account's scope
    // here too. Without it a concurrent `probe_authorization` /
    // `remove_account` of the same id (each under its own scope, e.g. Repair
    // clicked while sign-in finalizes) races the wipe/rename below, a hole in
    // the one-op-per-account-scope invariant. Contention fails the finalize
    // fail-safe rather than corrupting the account.
    let _account_guard = ScopeGuard::acquire(&shared.config.data_dir, account)
        .map_err(|_| "finalize-account-busy")?;

    // Resolve any older attempt for this identity before creating a fresh
    // decision record. This is idempotent and happens while the account scope
    // is exclusively owned.
    recover_auth_finalization_locked(&shared.config, &shared.vault, account)
        .map_err(|_| "finalize-recovery")?;

    let backup_account = rollback_key_account(account);
    let backup_dir = auth_backup_dir(&layout, account);
    let target_dir = layout.account_dir(account);
    let staged_dir = layout.account_dir(SIGN_IN_SLOT);
    let incumbent_key = shared
        .vault
        .database_key(account.0)
        .map_err(|_| "finalize-vault")?;
    let had_tdlib_state = target_dir.exists();
    let had_account_row =
        account_row_exists(&shared.config.data_dir, account).map_err(|_| "finalize-state")?;
    let record = AuthFinalizationRecord {
        account,
        phase: AuthFinalizationPhase::Prepared,
        had_account_row,
        had_database_key: incumbent_key.is_some(),
        had_tdlib_state,
    };

    prepare_auth_finalization(&shared.config.data_dir, record).map_err(|_| "finalize-state")?;

    let transaction = (|| {
        after_step(FinalizationStep::JournalPrepared)?;

        if let Some(key) = incumbent_key {
            shared
                .vault
                .store_database_key(backup_account.0, key)
                .map_err(|_| "finalize-vault")?;
        }
        after_step(FinalizationStep::KeyBackedUp)?;

        if had_tdlib_state {
            if backup_dir.exists() {
                return Err("finalize-recovery");
            }
            std::fs::rename(&target_dir, &backup_dir).map_err(|_| "finalize-storage")?;
            sync_directory(layout.root()).map_err(|_| "finalize-storage")?;
        }
        after_step(FinalizationStep::StorageBackedUp)?;

        std::fs::rename(&staged_dir, &target_dir).map_err(|_| "finalize-storage")?;
        sync_directory(layout.root()).map_err(|_| "finalize-storage")?;
        after_step(FinalizationStep::StorageInstalled)?;

        shared
            .vault
            .store_database_key(account.0, shared.database_key.clone())
            .map_err(|_| "finalize-vault")?;
        after_step(FinalizationStep::KeyInstalled)?;

        // Proof before the decision: the staged TDLib database is in the
        // stable directory and the stable keychain alias resolves to its key.
        if !layout.account_paths(account).database_directory().is_dir()
            || shared
                .vault
                .database_key(account.0)
                .map_err(|_| "finalize-vault")?
                .as_deref()
                != Some(shared.database_key.as_slice())
        {
            return Err("finalize-proof");
        }
        after_step(FinalizationStep::SuccessorProven)?;

        // The one explicit commit point: the provider-visible successor row
        // and the journal decision become durable in the same SQLite commit.
        commit_account_finalization(&shared.config.data_dir, account, &identity.display_name)
            .map_err(|_| "finalize-state")?;
        after_step(FinalizationStep::StateCommitted)?;
        Ok(())
    })();

    if let Err(code) = transaction {
        let phase = auth_finalization(&shared.config.data_dir, account)
            .map_err(|_| "finalize-recovery")?
            .map(|record| record.phase);
        if phase == Some(AuthFinalizationPhase::Prepared) {
            recover_auth_finalization_locked(&shared.config, &shared.vault, account)
                .map_err(|_| "finalize-recovery")?;
        }
        return Err(code);
    }

    // Cleanup is intentionally post-proof and idempotent. A cleanup error
    // leaves the committed journal in place for restart recovery; it never
    // turns a committed successor into a reported failed sign-in.
    drop(recover_auth_finalization_locked(
        &shared.config,
        &shared.vault,
        account,
    ));
    Ok(())
}

fn rollback_key_account(account: AccountId) -> AccountId {
    debug_assert!(account.0 > 0);
    AccountId(-account.0)
}

fn auth_backup_dir(layout: &StorageLayout, account: AccountId) -> std::path::PathBuf {
    layout
        .root()
        .join(format!(".account-{}.auth-backup", account.0))
}

fn sync_directory(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn account_row_exists(data_dir: &str, account: AccountId) -> Result<bool, DriveError> {
    let mut store = shared_state_store(data_dir)?;
    let txn = store.read_txn().map_err(storage_error)?;
    Ok(txn
        .account(AccountKey {
            account_id: account,
        })
        .map_err(storage_error)?
        .is_some())
}

fn auth_finalization(
    data_dir: &str,
    account: AccountId,
) -> Result<Option<AuthFinalizationRecord>, DriveError> {
    let mut store = shared_state_store(data_dir)?;
    let txn = store.read_txn().map_err(storage_error)?;
    txn.auth_finalization(account).map_err(storage_error)
}

fn prepare_auth_finalization(
    data_dir: &str,
    record: AuthFinalizationRecord,
) -> Result<(), DriveError> {
    let mut store = shared_state_store(data_dir)?;
    let txn = store.write_txn().map_err(storage_error)?;
    txn.prepare_auth_finalization(record)
        .map_err(storage_error)?;
    txn.commit().map_err(storage_error)
}

fn clear_auth_finalization(data_dir: &str, account: AccountId) -> Result<(), DriveError> {
    let mut store = shared_state_store(data_dir)?;
    let txn = store.write_txn().map_err(storage_error)?;
    txn.clear_auth_finalization(account)
        .map_err(storage_error)?;
    txn.commit().map_err(storage_error)
}

pub(crate) fn recover_all_auth_finalizations(
    config: &AuthSessionConfig,
    vault: &Arc<dyn SecretVault>,
) -> Result<(), DriveError> {
    if !shared_state_database_exists(&config.data_dir)? {
        return Ok(());
    }
    let records = {
        let mut store = shared_state_store(&config.data_dir)?;
        let txn = store.read_txn().map_err(storage_error)?;
        txn.auth_finalizations().map_err(storage_error)?
    };
    for record in records {
        let _guard = ScopeGuard::acquire(&config.data_dir, record.account)?;
        recover_auth_finalization_locked(config, vault, record.account)?;
    }
    Ok(())
}

pub(crate) fn recover_auth_finalization_locked(
    config: &AuthSessionConfig,
    vault: &Arc<dyn SecretVault>,
    account: AccountId,
) -> Result<(), DriveError> {
    if !shared_state_database_exists(&config.data_dir)? {
        return Ok(());
    }
    let Some(record) = auth_finalization(&config.data_dir, account)? else {
        return Ok(());
    };
    let layout = config.storage_layout();
    let target_dir = layout.account_dir(account);
    let backup_dir = auth_backup_dir(&layout, account);
    let backup_account = rollback_key_account(account);

    match record.phase {
        AuthFinalizationPhase::Prepared => {
            if record.had_database_key {
                if let Some(key) = vault.database_key(backup_account.0)? {
                    vault.store_database_key(account.0, key)?;
                    vault.delete_database_key(backup_account.0)?;
                }
            } else {
                vault.delete_database_key(account.0)?;
            }

            if backup_dir.exists() {
                layout
                    .wipe_account(account)
                    .map_err(|error| DriveError::Storage {
                        detail: format!("auth recovery could not remove staged state: {error}"),
                    })?;
                std::fs::rename(&backup_dir, &target_dir).map_err(|error| DriveError::Storage {
                    detail: format!("auth recovery could not restore incumbent state: {error}"),
                })?;
                sync_directory(layout.root()).map_err(|error| DriveError::Storage {
                    detail: format!("auth recovery could not sync restored state: {error}"),
                })?;
            } else if !record.had_tdlib_state {
                layout
                    .wipe_account(account)
                    .map_err(|error| DriveError::Storage {
                        detail: format!("auth recovery could not remove new state: {error}"),
                    })?;
            }
        }
        AuthFinalizationPhase::Committed => {
            // A committed decision is only cleaned after all three successor
            // resources are still present. Never erase the rollback material
            // when the committed side cannot be proven.
            if !account_row_exists(&config.data_dir, account)? {
                return Err(DriveError::Integrity {
                    detail: "committed auth finalization has no account row".to_owned(),
                });
            }
            if !target_dir.is_dir() || vault.database_key(account.0)?.is_none() {
                return Err(DriveError::Integrity {
                    detail: "committed auth finalization is missing TDLib state or key".to_owned(),
                });
            }
            match std::fs::remove_dir_all(&backup_dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(DriveError::Storage {
                        detail: format!("auth recovery could not remove state backup: {error}"),
                    });
                }
            }
            vault.delete_database_key(backup_account.0)?;
            vault.delete_database_key(SIGN_IN_SLOT.0)?;
        }
    }

    clear_auth_finalization(&config.data_dir, account)
}

fn shared_state_database_exists(data_dir: &str) -> Result<bool, DriveError> {
    Ok(std::path::Path::new(&shared_state_layout(data_dir.to_owned())?.database_file).is_file())
}

/// The coordinator-side in-process write handle over the shared durable
/// state (`shared_state.rs` § Writes), shared with `crate::removal`.
pub(crate) fn shared_state_store(data_dir: &str) -> Result<StateStore, DriveError> {
    let layout = shared_state_layout(data_dir.to_owned())?;
    StateStore::open(&layout.database_file).map_err(|error| DriveError::Storage {
        detail: format!("state open: {error}"),
    })
}

/// Upserts the account row and its root item — the same coordinator-side
/// in-process write path the engine host uses (`shared_state.rs` § Writes).
fn write_account_row(
    txn: &WriteTxn<'_>,
    account: AccountId,
    display_name: &str,
) -> Result<(), DriveError> {
    let scope = AccountScope {
        account: AccountKey {
            account_id: account,
        },
        namespace_version: NamespaceVersion(1),
    };
    let root_id: ItemId = ItemKey::Canonical(CanonicalKey::Account(scope.account)).id();
    let now = now_ms();
    txn.upsert_account(&AccountRecord {
        account: scope.account,
        source_kind: SourceKind::LocalTdlib,
        display_name: display_name.to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope.namespace_version,
        display_timezone: "UTC".to_owned(),
        retention_mode: RetentionMode::Mirror,
        archive_mode: false,
        secret_ref: None,
        created_at_ms: now,
        updated_at_ms: now,
    })
    .map_err(storage_error)?;
    txn.upsert_item(&ItemRecord {
        id: root_id.clone(),
        parent: None,
        display_name: display_name.to_owned(),
        safe_name: display_name.to_owned(),
        metadata_version: MetadataVersion::new("v1").map_err(|error| DriveError::Internal {
            detail: format!("metadata version: {error}"),
        })?,
        content: None,
        aggregate_size: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: Some(now),
        modified_at_ms: Some(now),
        deleted_at_ms: None,
    })
    .map_err(storage_error)?;
    upsert_fixed_root_structure(txn, scope, root_id, now)?;
    Ok(())
}

fn commit_account_finalization(
    data_dir: &str,
    account: AccountId,
    display_name: &str,
) -> Result<(), DriveError> {
    let mut store = shared_state_store(data_dir)?;
    let txn = store.write_txn().map_err(storage_error)?;
    write_account_row(&txn, account, display_name)?;
    txn.commit_auth_finalization(account)
        .map_err(storage_error)?;
    txn.commit().map_err(storage_error)
}

fn storage_error(error: impl std::fmt::Display) -> DriveError {
    DriveError::Storage {
        detail: format!("state write: {error}"),
    }
}

// MARK: - The probe

/// Probes an existing account's stored authorization: opens the account's
/// client over its persisted database and reports whether TDLib reaches
/// `Ready` without user input, then closes the client. The restart-
/// persistence proof (and the agent's repair diagnostic).
#[uniffi::export(async_runtime = "tokio")]
pub async fn probe_authorization(
    config: AuthSessionConfig,
    account_id: i64,
    vault: Arc<dyn SecretVault>,
) -> Result<AuthProbeOutcome, DriveError> {
    let runtime = shared_runtime()?;
    tokio::task::spawn_blocking(move || probe_over(&runtime, &config, account_id, &vault))
        .await
        .map_err(|error| DriveError::Internal {
            detail: format!("probe task: {error}"),
        })?
}

pub(crate) fn probe_over(
    runtime: &Arc<TdRuntime>,
    config: &AuthSessionConfig,
    account_id: i64,
    vault: &Arc<dyn SecretVault>,
) -> Result<AuthProbeOutcome, DriveError> {
    config.validate()?;
    if account_id <= 0 {
        return Err(DriveError::InvalidArgument {
            detail: "account_id must be a positive Telegram identity".to_owned(),
        });
    }
    let account = AccountId(account_id);
    let _guard = ScopeGuard::acquire(&config.data_dir, account)?;
    recover_auth_finalization_locked(config, vault, account)?;

    let secrets = VaultSecrets {
        vault: Arc::clone(vault),
        create_missing_key: false,
    };
    if vault.database_key(account.0)?.is_none() {
        return Ok(AuthProbeOutcome::SignedOut {
            kind: "no-database-key".to_owned(),
        });
    }
    let tdlib_config = config.tdlib_config(account, &secrets)?;

    let (client, updates) = runtime.create_client().map_err(td_to_drive_error)?;
    let mut machine = AuthMachine::new(tdlib_config);
    drop(client.request(json!({"@type": "getOption", "name": "version"})));

    let deadline = std::time::Instant::now() + PLUMBING_TIMEOUT;
    let outcome = loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break Err(DriveError::SourceUnavailable {
                detail: "the source reported no definitive authorization state".to_owned(),
            });
        }
        let update = match updates.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(update) => update,
            Err(UpdateRecvError::Timeout) => continue,
            Err(UpdateRecvError::Closed) => {
                break Err(DriveError::SourceUnavailable {
                    detail: "the sign-in client closed before a definitive state".to_owned(),
                });
            }
        };
        let step = match machine.on_update(&update) {
            Ok(step) => step,
            Err(_error) => continue,
        };
        for request in step.requests {
            if let Ok(pending) = client.request(request) {
                drop(pending.wait_timeout(PLUMBING_TIMEOUT));
            }
        }
        match step.entered {
            Some(AuthState::Ready) => {
                let display_name = client
                    .request(json!({"@type": "getMe"}))
                    .ok()
                    .and_then(|pending| pending.wait_timeout(PLUMBING_TIMEOUT).ok())
                    .and_then(Result::ok)
                    .as_ref()
                    .map(display_name_of);
                break Ok(AuthProbeOutcome::Authorized {
                    account_id: account.0,
                    display_name,
                });
            }
            Some(
                state @ (AuthState::WaitPhoneNumber
                | AuthState::WaitCode(_)
                | AuthState::WaitQrConfirmation { .. }
                | AuthState::WaitPassword(_)
                | AuthState::Unsupported { .. }),
            ) => {
                break Ok(AuthProbeOutcome::SignedOut {
                    kind: state.kind().to_owned(),
                });
            }
            _ => {}
        }
    };

    // Always leave the client closed; the engine owns long-lived clients.
    drop(client.close());
    let drain_deadline = std::time::Instant::now() + PLUMBING_TIMEOUT;
    while std::time::Instant::now() < drain_deadline {
        match updates.recv_timeout(Duration::from_millis(500)) {
            Ok(_update) => {}
            Err(UpdateRecvError::Closed) => break,
            Err(UpdateRecvError::Timeout) => {}
        }
    }
    outcome
}

/// Shared fixtures for this crate's auth and removal test modules: the
/// deterministic mock runtime, an in-memory vault, and canned TDLib events.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use gramdrive_source_tdjson::mock::{MockHandle, MockTdJson};
    use gramdrive_source_tdjson::runtime::RuntimeConfig;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU32;
    use std::time::Instant;

    pub(crate) const GUARD: Duration = Duration::from_secs(5);

    pub(crate) struct TempRoot {
        pub(crate) path: PathBuf,
    }

    impl TempRoot {
        pub(crate) fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("gramdrive-ffi-auth-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("temp root");
            Self { path }
        }

        pub(crate) fn as_str(&self) -> &str {
            self.path.to_str().expect("temp path is UTF-8")
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    pub(crate) fn config(root: &TempRoot) -> AuthSessionConfig {
        AuthSessionConfig {
            data_dir: root.as_str().to_owned(),
            use_test_dc: true,
            device_model: "GramDrive Tests".to_owned(),
            system_version: "test".to_owned(),
            application_version: "0.0.0".to_owned(),
            system_language_code: "en".to_owned(),
        }
    }

    /// An in-memory vault: creds fixed, keys per account.
    #[derive(Default)]
    pub(crate) struct FakeVault {
        keys: Mutex<std::collections::HashMap<i64, Vec<u8>>>,
    }

    impl FakeVault {
        pub(crate) fn key(&self, account: i64) -> Option<Vec<u8>> {
            self.keys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&account)
                .cloned()
        }
    }

    impl SecretVault for FakeVault {
        fn api_credentials(&self) -> Result<VaultApiCredentials, DriveError> {
            Ok(VaultApiCredentials {
                api_id: 424242,
                api_hash: "api-hash-sentinel".to_owned(),
            })
        }

        fn database_key(&self, account_id: i64) -> Result<Option<Vec<u8>>, DriveError> {
            Ok(self.key(account_id))
        }

        fn ensure_database_key(&self, account_id: i64) -> Result<Vec<u8>, DriveError> {
            let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
            Ok(keys
                .entry(account_id)
                .or_insert_with(|| vec![7u8; 32])
                .clone())
        }

        fn store_database_key(&self, account_id: i64, key: Vec<u8>) -> Result<(), DriveError> {
            self.keys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(account_id, key);
            Ok(())
        }

        fn delete_database_key(&self, account_id: i64) -> Result<(), DriveError> {
            self.keys
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&account_id);
            Ok(())
        }
    }

    pub(crate) fn auth_update(client_id: i32, state_json: &str) -> String {
        format!(
            concat!(
                r#"{{"@type":"updateAuthorizationState","#,
                r#""authorization_state":{},"@client_id":{}}}"#
            ),
            state_json, client_id
        )
    }

    pub(crate) fn ok_response(extra: u64, client_id: i32) -> String {
        format!(r#"{{"@type":"ok","@extra":{extra},"@client_id":{client_id}}}"#)
    }

    pub(crate) fn error_response(extra: u64, client_id: i32, code: i64, message: &str) -> String {
        format!(
            r#"{{"@type":"error","code":{code},"message":"{message}","@extra":{extra},"@client_id":{client_id}}}"#
        )
    }

    pub(crate) fn me_response(extra: u64, client_id: i32) -> String {
        format!(
            concat!(
                r#"{{"@type":"user","id":777000123,"first_name":"Test","#,
                r#""last_name":"User","@extra":{},"@client_id":{}}}"#
            ),
            extra, client_id
        )
    }

    pub(crate) fn start_runtime() -> (Arc<TdRuntime>, MockHandle) {
        let (sender, receiver, handle) = MockTdJson::new();
        let runtime = TdRuntime::start(
            sender,
            receiver,
            RuntimeConfig {
                receive_timeout: Duration::from_millis(20),
                ..RuntimeConfig::default()
            },
        )
        .expect("runtime starts");
        (Arc::new(runtime), handle)
    }

    /// The sessions do not expose their client id; the activation request
    /// is the first thing they send, so the mock's sent log carries it.
    pub(crate) fn client_id_of(handle: &MockHandle) -> i32 {
        let deadline = Instant::now() + GUARD;
        loop {
            if let Some(sent) = handle.take_sent().first() {
                return sent.client_id;
            }
            assert!(Instant::now() < deadline, "no request arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(crate) fn kick(handle: &MockHandle, client_id: i32) {
        handle.push_event(&auth_update(
            client_id,
            r#"{"@type":"authorizationStateWaitTdlibParameters"}"#,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;
    use crate::shared_state::{SharedStateStore, StateRole};
    use gramdrive_source_tdjson::mock::{MockHandle, SentRequest};
    use std::time::Instant;

    /// Records phases and lets tests wait for one.
    #[derive(Default)]
    struct RecordingListener {
        phases: Mutex<Vec<AuthPhase>>,
    }

    impl AuthStateListener for RecordingListener {
        fn on_phase(&self, phase: AuthPhase) {
            self.phases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(phase);
        }
    }

    impl RecordingListener {
        fn phases(&self) -> Vec<AuthPhase> {
            self.phases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        fn wait_for(&self, predicate: impl Fn(&AuthPhase) -> bool) -> AuthPhase {
            let deadline = Instant::now() + GUARD;
            loop {
                if let Some(phase) = self.phases().into_iter().rev().find(|p| predicate(p)) {
                    return phase;
                }
                assert!(
                    Instant::now() < deadline,
                    "phase did not arrive within the guard; saw {:?}",
                    self.phases()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn kind_of(phase: &AuthPhase) -> &'static str {
        match phase {
            AuthPhase::Starting => "starting",
            AuthPhase::Configuring => "configuring",
            AuthPhase::WaitPhoneNumber => "wait-phone-number",
            AuthPhase::WaitCode { .. } => "wait-code",
            AuthPhase::WaitQrConfirmation { .. } => "wait-qr-confirmation",
            AuthPhase::WaitPassword { .. } => "wait-password",
            AuthPhase::Finalizing => "finalizing",
            AuthPhase::Complete { .. } => "complete",
            AuthPhase::LoggingOut => "logging-out",
            AuthPhase::Closing => "closing",
            AuthPhase::Closed => "closed",
            AuthPhase::Unsupported { .. } => "unsupported",
            AuthPhase::Failed { .. } => "failed",
        }
    }

    const WAIT_PHONE: &str = r#"{"@type":"authorizationStateWaitPhoneNumber"}"#;
    const WAIT_PASSWORD: &str = concat!(
        r#"{"@type":"authorizationStateWaitPassword","#,
        r#""password_hint":"the usual","has_recovery_email_address":true}"#
    );
    const WAIT_QR: &str = concat!(
        r#"{"@type":"authorizationStateWaitOtherDeviceConfirmation","#,
        r#""link":"tg://login?token=abc"}"#
    );
    const WAIT_CODE: &str = concat!(
        r#"{"@type":"authorizationStateWaitCode","code_info":{"#,
        r#""phone_number":"+9996612222","#,
        r#""type":{"@type":"authenticationCodeTypeTelegramMessage","length":5},"#,
        r#""timeout":60}}"#
    );
    const READY: &str = r#"{"@type":"authorizationStateReady"}"#;
    const CLOSING: &str = r#"{"@type":"authorizationStateClosing"}"#;
    const CLOSED: &str = r#"{"@type":"authorizationStateClosed"}"#;
    const WAIT_EMAIL: &str = r#"{"@type":"authorizationStateWaitEmailAddress"}"#;

    /// The scripted Telegram side of a full phone → code → password
    /// sign-in, including finalization's getMe and clean close.
    fn sign_in_responder() -> impl FnMut(&SentRequest) -> Vec<String> + Send + 'static {
        |sent: &SentRequest| {
            let extra = sent.extra().expect("runtime injects @extra");
            let cid = sent.client_id;
            match sent.request_type().as_deref() {
                Some("setTdlibParameters") => {
                    vec![ok_response(extra, cid), auth_update(cid, WAIT_PHONE)]
                }
                Some("setAuthenticationPhoneNumber") => {
                    vec![ok_response(extra, cid), auth_update(cid, WAIT_CODE)]
                }
                Some("requestQrCodeAuthentication") => {
                    vec![ok_response(extra, cid), auth_update(cid, WAIT_QR)]
                }
                Some("checkAuthenticationCode") => {
                    vec![ok_response(extra, cid), auth_update(cid, WAIT_PASSWORD)]
                }
                Some("checkAuthenticationPassword") => {
                    vec![ok_response(extra, cid), auth_update(cid, READY)]
                }
                Some("getMe") => vec![me_response(extra, cid)],
                Some("close") => vec![
                    ok_response(extra, cid),
                    auth_update(cid, CLOSING),
                    auth_update(cid, CLOSED),
                ],
                _ => vec![ok_response(extra, cid)],
            }
        }
    }

    struct SessionFixture {
        root: TempRoot,
        vault: Arc<FakeVault>,
        listener: Arc<RecordingListener>,
        session: Arc<AuthSession>,
        handle: MockHandle,
        client_id: i32,
    }

    fn start_session() -> SessionFixture {
        let (runtime, handle) = start_runtime();
        handle.set_responder(sign_in_responder());
        let root = TempRoot::new();
        // Coordinator open creates the canonical shared-state layout, the
        // same precondition the agent process guarantees in production.
        drop(
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Coordinator)
                .expect("coordinator open"),
        );
        let vault = Arc::new(FakeVault::default());
        let listener = Arc::new(RecordingListener::default());
        let session = AuthSession::start_over(
            runtime,
            config(&root),
            Arc::clone(&vault) as Arc<dyn SecretVault>,
            Arc::clone(&listener) as Arc<dyn AuthStateListener>,
        )
        .expect("session starts");
        let client_id = client_id_of(&handle);
        kick(&handle, client_id);
        SessionFixture {
            root,
            vault,
            listener,
            session,
            handle,
            client_id,
        }
    }

    const TEST_ACCOUNT: AccountId = AccountId(777000123);
    const INCUMBENT_KEY: &[u8] = &[9u8; 32];

    fn seed_incumbent(fixture: &SessionFixture) {
        let config = config(&fixture.root);
        let layout = config.storage_layout();
        let target = layout
            .account_paths(TEST_ACCOUNT)
            .database_directory()
            .to_owned();
        std::fs::create_dir_all(&target).expect("incumbent tdlib directory");
        std::fs::write(target.join("incumbent"), b"incumbent").expect("incumbent marker");
        let staged = layout
            .account_paths(SIGN_IN_SLOT)
            .database_directory()
            .to_owned();
        std::fs::write(staged.join("successor"), b"successor").expect("successor marker");
        fixture
            .vault
            .store_database_key(TEST_ACCOUNT.0, INCUMBENT_KEY.to_vec())
            .expect("incumbent key");

        let mut store = shared_state_store(&config.data_dir).expect("state store");
        let txn = store.write_txn().expect("state write");
        write_account_row(&txn, TEST_ACCOUNT, "Incumbent").expect("incumbent row");
        txn.commit().expect("commit incumbent row");
    }

    fn account_display_name(root: &TempRoot) -> String {
        let store = SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider)
            .expect("provider state");
        store
            .accounts()
            .expect("accounts")
            .into_iter()
            .find(|account| account.account_id == TEST_ACCOUNT.0)
            .expect("test account")
            .display_name
    }

    fn assert_authorized_probe(root: &TempRoot, vault: &Arc<FakeVault>) {
        let (runtime, handle) = start_runtime();
        handle.set_responder(|sent: &SentRequest| {
            let extra = sent.extra().expect("extra");
            let cid = sent.client_id;
            match sent.request_type().as_deref() {
                Some("setTdlibParameters") => {
                    vec![ok_response(extra, cid), auth_update(cid, READY)]
                }
                Some("getMe") => vec![me_response(extra, cid)],
                Some("close") => vec![
                    ok_response(extra, cid),
                    auth_update(cid, CLOSING),
                    auth_update(cid, CLOSED),
                ],
                _ => vec![ok_response(extra, cid)],
            }
        });
        let kick_handle = handle;
        let kick_thread = std::thread::spawn(move || {
            let client_id = client_id_of(&kick_handle);
            kick(&kick_handle, client_id);
        });
        let outcome = probe_over(
            &runtime,
            &config(root),
            TEST_ACCOUNT.0,
            &(Arc::clone(vault) as Arc<dyn SecretVault>),
        )
        .expect("incumbent remains probeable");
        kick_thread.join().expect("kick thread");
        assert!(matches!(outcome, AuthProbeOutcome::Authorized { .. }));
    }

    fn assert_incumbent_preserved(fixture: &SessionFixture) {
        let config = config(&fixture.root);
        let layout = config.storage_layout();
        let target = layout
            .account_paths(TEST_ACCOUNT)
            .database_directory()
            .to_owned();
        assert!(target.join("incumbent").is_file());
        assert!(!target.join("successor").exists());
        assert_eq!(
            fixture.vault.key(TEST_ACCOUNT.0),
            Some(INCUMBENT_KEY.to_vec())
        );
        assert!(
            fixture
                .vault
                .key(rollback_key_account(TEST_ACCOUNT).0)
                .is_none()
        );
        assert!(!auth_backup_dir(&layout, TEST_ACCOUNT).exists());
        assert_eq!(account_display_name(&fixture.root), "Incumbent");
        assert!(
            auth_finalization(&config.data_dir, TEST_ACCOUNT)
                .expect("journal read")
                .is_none()
        );
        assert_authorized_probe(&fixture.root, &fixture.vault);
    }

    #[test]
    fn every_precommit_failure_restores_an_openable_incumbent() {
        let steps = [
            FinalizationStep::JournalPrepared,
            FinalizationStep::KeyBackedUp,
            FinalizationStep::StorageBackedUp,
            FinalizationStep::StorageInstalled,
            FinalizationStep::KeyInstalled,
            FinalizationStep::SuccessorProven,
        ];
        for failed_step in steps {
            let fixture = start_session();
            fixture
                .listener
                .wait_for(|phase| *phase == AuthPhase::WaitPhoneNumber);
            seed_incumbent(&fixture);
            let result = persist_account_with_hook(
                &fixture.session.shared,
                &SignedInIdentity {
                    account_id: TEST_ACCOUNT.0,
                    display_name: "Successor".to_owned(),
                },
                |step| {
                    if step == failed_step {
                        Err("injected-finalization-failure")
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(
                result,
                Err("injected-finalization-failure"),
                "{failed_step:?}"
            );
            assert_incumbent_preserved(&fixture);
        }
    }

    #[test]
    fn restart_recovery_rolls_back_precommit_and_is_idempotent() {
        let fixture = start_session();
        fixture
            .listener
            .wait_for(|phase| *phase == AuthPhase::WaitPhoneNumber);
        seed_incumbent(&fixture);
        let config = config(&fixture.root);
        let layout = config.storage_layout();
        let backup = auth_backup_dir(&layout, TEST_ACCOUNT);
        let target = layout.account_dir(TEST_ACCOUNT);
        prepare_auth_finalization(
            &config.data_dir,
            AuthFinalizationRecord {
                account: TEST_ACCOUNT,
                phase: AuthFinalizationPhase::Prepared,
                had_account_row: true,
                had_database_key: true,
                had_tdlib_state: true,
            },
        )
        .expect("prepare journal");
        fixture
            .vault
            .store_database_key(rollback_key_account(TEST_ACCOUNT).0, INCUMBENT_KEY.to_vec())
            .expect("backup key");
        std::fs::rename(&target, &backup).expect("backup incumbent state");
        std::fs::rename(layout.account_dir(SIGN_IN_SLOT), &target).expect("install staged state");
        fixture
            .vault
            .store_database_key(TEST_ACCOUNT.0, vec![7u8; 32])
            .expect("install staged key");

        recover_auth_finalization_locked(
            &config,
            &(Arc::clone(&fixture.vault) as Arc<dyn SecretVault>),
            TEST_ACCOUNT,
        )
        .expect("restart rollback");
        recover_auth_finalization_locked(
            &config,
            &(Arc::clone(&fixture.vault) as Arc<dyn SecretVault>),
            TEST_ACCOUNT,
        )
        .expect("idempotent restart rollback");
        assert_incumbent_preserved(&fixture);
    }

    #[test]
    fn restart_recovery_keeps_the_committed_successor_and_cleans_backups() {
        let fixture = start_session();
        fixture
            .listener
            .wait_for(|phase| *phase == AuthPhase::WaitPhoneNumber);
        seed_incumbent(&fixture);
        let result = persist_account_with_hook(
            &fixture.session.shared,
            &SignedInIdentity {
                account_id: TEST_ACCOUNT.0,
                display_name: "Successor".to_owned(),
            },
            |step| {
                if step == FinalizationStep::StateCommitted {
                    Err("injected-postcommit-crash")
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(result, Err("injected-postcommit-crash"));

        let config = config(&fixture.root);
        let journal = auth_finalization(&config.data_dir, TEST_ACCOUNT)
            .expect("journal read")
            .expect("committed journal retained");
        assert_eq!(journal.phase, AuthFinalizationPhase::Committed);
        recover_auth_finalization_locked(
            &config,
            &(Arc::clone(&fixture.vault) as Arc<dyn SecretVault>),
            TEST_ACCOUNT,
        )
        .expect("postcommit cleanup");
        recover_auth_finalization_locked(
            &config,
            &(Arc::clone(&fixture.vault) as Arc<dyn SecretVault>),
            TEST_ACCOUNT,
        )
        .expect("idempotent postcommit cleanup");

        let layout = config.storage_layout();
        let target = layout
            .account_paths(TEST_ACCOUNT)
            .database_directory()
            .to_owned();
        assert!(target.join("successor").is_file());
        assert!(!target.join("incumbent").exists());
        assert_eq!(fixture.vault.key(TEST_ACCOUNT.0), Some(vec![7u8; 32]));
        assert!(fixture.vault.key(SIGN_IN_SLOT.0).is_none());
        assert!(!auth_backup_dir(&layout, TEST_ACCOUNT).exists());
        assert_eq!(account_display_name(&fixture.root), "Successor");
        assert!(
            auth_finalization(&config.data_dir, TEST_ACCOUNT)
                .expect("journal read")
                .is_none()
        );
        assert_authorized_probe(&fixture.root, &fixture.vault);
    }

    #[tokio::test]
    async fn phone_code_password_flow_completes_and_persists_the_account() {
        let fixture = start_session();
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);

        let outcome = fixture
            .session
            .submit(AuthCommand::SubmitPhoneNumber {
                phone_number: "+9996612222".to_owned(),
            })
            .await
            .expect("submit");
        assert_eq!(outcome, AuthSubmitOutcome::Accepted);
        let code_phase = fixture.listener.wait_for(|p| kind_of(p) == "wait-code");
        let AuthPhase::WaitCode { info } = code_phase else {
            panic!("expected wait-code");
        };
        assert_eq!(info.phone_number, "+9996612222");
        assert_eq!(info.code_length, Some(5));

        let outcome = fixture
            .session
            .submit(AuthCommand::SubmitCode {
                code: "22222".to_owned(),
            })
            .await
            .expect("submit");
        assert_eq!(outcome, AuthSubmitOutcome::Accepted);
        let password_phase = fixture.listener.wait_for(|p| kind_of(p) == "wait-password");
        let AuthPhase::WaitPassword { info } = password_phase else {
            panic!("expected wait-password");
        };
        assert_eq!(info.hint, "the usual");
        assert!(info.has_recovery_email);

        let outcome = fixture
            .session
            .submit(AuthCommand::SubmitPassword {
                password: "hunter2".to_owned(),
            })
            .await
            .expect("submit");
        assert_eq!(outcome, AuthSubmitOutcome::Accepted);

        let complete = fixture.listener.wait_for(|p| kind_of(p) == "complete");
        assert_eq!(
            complete,
            AuthPhase::Complete {
                account_id: 777000123,
                display_name: "Test User".to_owned(),
            }
        );
        let kinds: Vec<&str> = fixture.listener.phases().iter().map(kind_of).collect();
        assert!(kinds.contains(&"finalizing"), "saw {kinds:?}");

        // The storage moved from the slot to the account's directory.
        let telegram = fixture.root.path.join(TELEGRAM_SUBTREE);
        assert!(!telegram.join("account-0").exists());
        assert!(telegram.join("account-777000123").join("tdlib").exists());

        // The key re-homed.
        assert!(fixture.vault.key(0).is_none());
        assert_eq!(fixture.vault.key(777000123), Some(vec![7u8; 32]));

        // The account row and its root item are durable and provider-visible.
        let store = SharedStateStore::open(fixture.root.as_str().to_owned(), StateRole::Provider)
            .expect("provider open");
        let accounts = store.accounts().expect("accounts");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].account_id, 777000123);
        assert_eq!(accounts[0].display_name, "Test User");
        assert_eq!(accounts[0].auth_state, "authorized");
        let root_item = store
            .item(accounts[0].root_item_id.clone())
            .expect("root read");
        assert!(root_item.is_some(), "root item exists");
        let children = store
            .children(accounts[0].root_item_id.clone(), None, 10)
            .expect("fixed root structure");
        let names: Vec<_> = children
            .iter()
            .map(|item| item.display_name.as_str())
            .collect();
        assert_eq!(children.len(), 4);
        assert!(names.contains(&"Chats"));
        assert!(names.contains(&"Archive"));
        assert!(names.contains(&"Stories"));
        assert!(names.contains(&"Folders"));
    }

    #[tokio::test]
    async fn qr_flow_reports_the_link() {
        let fixture = start_session();
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);
        let outcome = fixture
            .session
            .submit(AuthCommand::RequestQrCode)
            .await
            .expect("submit");
        assert_eq!(outcome, AuthSubmitOutcome::Accepted);
        let qr = fixture
            .listener
            .wait_for(|p| kind_of(p) == "wait-qr-confirmation");
        assert_eq!(
            qr,
            AuthPhase::WaitQrConfirmation {
                link: "tg://login?token=abc".to_owned()
            }
        );
    }

    #[tokio::test]
    async fn invalid_input_for_the_state_is_typed_not_sent() {
        let fixture = start_session();
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);
        let outcome = fixture
            .session
            .submit(AuthCommand::SubmitCode {
                code: "12345".to_owned(),
            })
            .await
            .expect("submit");
        assert_eq!(outcome, AuthSubmitOutcome::InvalidForState);
    }

    #[tokio::test]
    async fn rejections_are_classified() {
        let fixture = start_session();
        fixture.handle.set_responder(|sent: &SentRequest| {
            let extra = sent.extra().expect("extra");
            let cid = sent.client_id;
            match sent.request_type().as_deref() {
                Some("setTdlibParameters") => {
                    vec![ok_response(extra, cid), auth_update(cid, WAIT_PHONE)]
                }
                Some("setAuthenticationPhoneNumber") => {
                    vec![error_response(extra, cid, 400, "PHONE_NUMBER_INVALID")]
                }
                _ => vec![ok_response(extra, cid)],
            }
        });
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);
        let outcome = fixture
            .session
            .submit(AuthCommand::SubmitPhoneNumber {
                phone_number: "+1".to_owned(),
            })
            .await
            .expect("submit");
        assert_eq!(
            outcome,
            AuthSubmitOutcome::Rejected {
                rejection: AuthRejectionInfo::InvalidPhoneNumber
            }
        );
    }

    #[tokio::test]
    async fn flood_wait_maps_to_rate_limited_with_the_stated_delay() {
        let fixture = start_session();
        fixture.handle.set_responder(|sent: &SentRequest| {
            let extra = sent.extra().expect("extra");
            let cid = sent.client_id;
            match sent.request_type().as_deref() {
                Some("setTdlibParameters") => {
                    vec![ok_response(extra, cid), auth_update(cid, WAIT_PHONE)]
                }
                Some("setAuthenticationPhoneNumber") => vec![error_response(
                    extra,
                    cid,
                    429,
                    "Too Many Requests: retry after 17",
                )],
                _ => vec![ok_response(extra, cid)],
            }
        });
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);
        let outcome = fixture
            .session
            .submit(AuthCommand::SubmitPhoneNumber {
                phone_number: "+9996612222".to_owned(),
            })
            .await
            .expect("submit");
        assert_eq!(
            outcome,
            AuthSubmitOutcome::Rejected {
                rejection: AuthRejectionInfo::RateLimited {
                    retry_after_secs: Some(17)
                }
            }
        );
    }

    #[tokio::test]
    async fn close_reports_closing_then_closed_and_ends_the_session() {
        let fixture = start_session();
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);
        fixture.session.shutdown();
        fixture.listener.wait_for(|p| *p == AuthPhase::Closed);
        let error = fixture
            .session
            .submit(AuthCommand::SubmitPhoneNumber {
                phone_number: "+1".to_owned(),
            })
            .await
            .expect_err("closed session refuses inputs");
        assert!(matches!(error, DriveError::Cancelled { .. }));
    }

    #[tokio::test]
    async fn unsupported_states_fail_safe() {
        let fixture = start_session();
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);
        fixture
            .handle
            .push_event(&auth_update(fixture.client_id, WAIT_EMAIL));
        let unsupported = fixture.listener.wait_for(|p| kind_of(p) == "unsupported");
        assert_eq!(
            unsupported,
            AuthPhase::Unsupported {
                kind: "authorizationStateWaitEmailAddress".to_owned()
            }
        );
        let outcome = fixture
            .session
            .submit(AuthCommand::SubmitPhoneNumber {
                phone_number: "+1".to_owned(),
            })
            .await
            .expect("submit");
        assert_eq!(outcome, AuthSubmitOutcome::InvalidForState);
    }

    #[tokio::test]
    async fn cancel_is_accepted_in_the_unsupported_state() {
        // Cancel is the one input the flow accepts from `Unsupported` (module
        // docs): it closes the client rather than being refused InvalidForState.
        let fixture = start_session();
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);
        fixture
            .handle
            .push_event(&auth_update(fixture.client_id, WAIT_EMAIL));
        fixture.listener.wait_for(|p| kind_of(p) == "unsupported");
        let outcome = fixture
            .session
            .submit(AuthCommand::Cancel)
            .await
            .expect("submit");
        assert_eq!(outcome, AuthSubmitOutcome::Accepted);
        fixture.listener.wait_for(|p| *p == AuthPhase::Closed);
    }

    #[tokio::test]
    async fn dropping_a_session_without_close_frees_the_sign_in_slot() {
        // A host that drops the handle without shutdown() must not strand the
        // pump thread or hold the slot scope: Drop→shutdown() unwinds the pump,
        // and a fresh sign-in over the same root then succeeds.
        let root = TempRoot::new();
        let (runtime, handle) = start_runtime();
        handle.set_responder(sign_in_responder());
        let listener = Arc::new(RecordingListener::default());
        let session = AuthSession::start_over(
            Arc::clone(&runtime),
            config(&root),
            Arc::new(FakeVault::default()) as Arc<dyn SecretVault>,
            Arc::clone(&listener) as Arc<dyn AuthStateListener>,
        )
        .expect("first session starts");
        let client_id = client_id_of(&handle);
        kick(&handle, client_id);
        listener.wait_for(|p| *p == AuthPhase::WaitPhoneNumber);

        // Drop without shutdown(); the scope release is asynchronous (the pump
        // must observe the client's close), so poll the reacquire.
        drop(session);
        let deadline = Instant::now() + GUARD;
        let reacquired = loop {
            match AuthSession::start_over(
                Arc::clone(&runtime),
                config(&root),
                Arc::new(FakeVault::default()) as Arc<dyn SecretVault>,
                Arc::new(RecordingListener::default()) as Arc<dyn AuthStateListener>,
            ) {
                Ok(session) => break session,
                Err(DriveError::InvalidArgument { .. }) => {
                    assert!(
                        Instant::now() < deadline,
                        "the sign-in slot scope was never freed after drop"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(other) => panic!("unexpected start error: {other:?}"),
            }
        };
        reacquired.shutdown();
    }

    #[tokio::test]
    async fn wait_closed_returns_only_after_the_sign_in_slot_is_free() {
        let fixture = start_session();
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);

        fixture.session.shutdown();
        fixture.session.wait_closed().await;

        // No polling or retry is needed after the explicit barrier: the
        // pump-owned ScopeGuard has already dropped.
        let (runtime, handle) = start_runtime();
        handle.set_responder(sign_in_responder());
        let replacement = AuthSession::start_over(
            runtime,
            config(&fixture.root),
            Arc::new(FakeVault::default()) as Arc<dyn SecretVault>,
            Arc::new(RecordingListener::default()) as Arc<dyn AuthStateListener>,
        )
        .expect("replacement starts after wait_closed");
        replacement.shutdown();
    }

    #[tokio::test]
    async fn finalization_fails_safe_when_the_account_scope_is_held() {
        // A concurrent probe/remove of the same account would hold this scope;
        // finalization must refuse to touch the real account under it rather
        // than race the wipe/rename.
        let fixture = start_session();
        let held = ScopeGuard::acquire(fixture.root.as_str(), AccountId(777000123))
            .expect("the account scope is free before finalize");
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);
        fixture
            .session
            .submit(AuthCommand::SubmitPhoneNumber {
                phone_number: "+9996612222".to_owned(),
            })
            .await
            .expect("submit");
        fixture.listener.wait_for(|p| kind_of(p) == "wait-code");
        fixture
            .session
            .submit(AuthCommand::SubmitCode {
                code: "22222".to_owned(),
            })
            .await
            .expect("submit");
        fixture.listener.wait_for(|p| kind_of(p) == "wait-password");
        fixture
            .session
            .submit(AuthCommand::SubmitPassword {
                password: "hunter2".to_owned(),
            })
            .await
            .expect("submit");

        let failed = fixture.listener.wait_for(|p| kind_of(p) == "failed");
        assert_eq!(
            failed,
            AuthPhase::Failed {
                detail: "finalize-account-busy".to_owned(),
            }
        );
        // The contended account was never mutated: no storage moved into it,
        // no key re-homed.
        let telegram = fixture.root.path.join(TELEGRAM_SUBTREE);
        assert!(!telegram.join("account-777000123").exists());
        assert!(fixture.vault.key(777000123).is_none());
        drop(held);
    }

    #[tokio::test]
    async fn finalization_reports_failed_when_the_identity_read_fails() {
        // The `Failed` finalization path: TDLib reaches Ready but getMe errors;
        // the session reports a stable redacted code, not Complete.
        let fixture = start_session();
        fixture
            .listener
            .wait_for(|p| *p == AuthPhase::WaitPhoneNumber);
        fixture
            .session
            .submit(AuthCommand::SubmitPhoneNumber {
                phone_number: "+9996612222".to_owned(),
            })
            .await
            .expect("submit");
        fixture.listener.wait_for(|p| kind_of(p) == "wait-code");
        fixture
            .session
            .submit(AuthCommand::SubmitCode {
                code: "22222".to_owned(),
            })
            .await
            .expect("submit");
        fixture.listener.wait_for(|p| kind_of(p) == "wait-password");
        // From the password on: reach Ready, then fail getMe.
        fixture.handle.set_responder(|sent: &SentRequest| {
            let extra = sent.extra().expect("extra");
            let cid = sent.client_id;
            match sent.request_type().as_deref() {
                Some("checkAuthenticationPassword") => {
                    vec![ok_response(extra, cid), auth_update(cid, READY)]
                }
                Some("getMe") => vec![error_response(extra, cid, 500, "GETME_FAILED")],
                Some("close") => vec![
                    ok_response(extra, cid),
                    auth_update(cid, CLOSING),
                    auth_update(cid, CLOSED),
                ],
                _ => vec![ok_response(extra, cid)],
            }
        });
        fixture
            .session
            .submit(AuthCommand::SubmitPassword {
                password: "hunter2".to_owned(),
            })
            .await
            .expect("submit");

        let failed = fixture.listener.wait_for(|p| kind_of(p) == "failed");
        assert_eq!(
            failed,
            AuthPhase::Failed {
                detail: "finalize-identity".to_owned(),
            }
        );
        // Nothing was persisted: no real account row, no re-homed key.
        assert!(fixture.vault.key(777000123).is_none());
    }

    #[tokio::test]
    async fn a_second_sign_in_over_the_same_root_is_refused() {
        let fixture = start_session();
        let (runtime2, _handle2) = start_runtime();
        let error = AuthSession::start_over(
            runtime2,
            config(&fixture.root),
            Arc::new(FakeVault::default()) as Arc<dyn SecretVault>,
            Arc::new(RecordingListener::default()) as Arc<dyn AuthStateListener>,
        )
        .expect_err("second sign-in refused");
        assert!(matches!(error, DriveError::InvalidArgument { .. }));
    }

    #[cfg(not(real_tdjson))]
    #[test]
    fn without_the_runtime_start_is_honestly_unavailable() {
        let root = TempRoot::new();
        let error = AuthSession::start(
            config(&root),
            Arc::new(FakeVault::default()) as Arc<dyn SecretVault>,
            Arc::new(RecordingListener::default()) as Arc<dyn AuthStateListener>,
        )
        .expect_err("no runtime in hermetic builds");
        assert!(matches!(error, DriveError::SourceUnavailable { .. }));
    }

    #[test]
    fn probe_reports_authorized_for_a_persisted_session() {
        let (runtime, handle) = start_runtime();
        handle.set_responder(|sent: &SentRequest| {
            let extra = sent.extra().expect("extra");
            let cid = sent.client_id;
            match sent.request_type().as_deref() {
                Some("setTdlibParameters") => {
                    vec![ok_response(extra, cid), auth_update(cid, READY)]
                }
                Some("getMe") => vec![me_response(extra, cid)],
                Some("close") => vec![
                    ok_response(extra, cid),
                    auth_update(cid, CLOSING),
                    auth_update(cid, CLOSED),
                ],
                _ => vec![ok_response(extra, cid)],
            }
        });
        let root = TempRoot::new();
        let vault = Arc::new(FakeVault::default());
        vault
            .store_database_key(777000123, vec![7u8; 32])
            .expect("seed key");
        // The probe activates its client; the mock needs the kick once the
        // activation request shows up.
        let handle_for_kick = handle;
        let kick_thread = std::thread::spawn(move || {
            let cid = client_id_of(&handle_for_kick);
            kick(&handle_for_kick, cid);
        });
        let outcome = probe_over(
            &runtime,
            &config(&root),
            777000123,
            &(vault as Arc<dyn SecretVault>),
        )
        .expect("probe");
        kick_thread.join().expect("kick thread");
        assert_eq!(
            outcome,
            AuthProbeOutcome::Authorized {
                account_id: 777000123,
                display_name: Some("Test User".to_owned()),
            }
        );
    }

    #[test]
    fn probe_without_a_key_is_signed_out_and_creates_nothing() {
        let (runtime, _handle) = start_runtime();
        let root = TempRoot::new();
        let vault = Arc::new(FakeVault::default());
        let outcome = probe_over(
            &runtime,
            &config(&root),
            777000123,
            &(Arc::clone(&vault) as Arc<dyn SecretVault>),
        )
        .expect("probe");
        assert_eq!(
            outcome,
            AuthProbeOutcome::SignedOut {
                kind: "no-database-key".to_owned()
            }
        );
        assert!(vault.key(777000123).is_none());
    }

    #[test]
    fn probe_reports_signed_out_when_sign_in_is_required() {
        let (runtime, handle) = start_runtime();
        handle.set_responder(|sent: &SentRequest| {
            let extra = sent.extra().expect("extra");
            let cid = sent.client_id;
            match sent.request_type().as_deref() {
                Some("setTdlibParameters") => {
                    vec![ok_response(extra, cid), auth_update(cid, WAIT_PHONE)]
                }
                Some("close") => vec![
                    ok_response(extra, cid),
                    auth_update(cid, CLOSING),
                    auth_update(cid, CLOSED),
                ],
                _ => vec![ok_response(extra, cid)],
            }
        });
        let root = TempRoot::new();
        let vault = Arc::new(FakeVault::default());
        vault
            .store_database_key(777000123, vec![7u8; 32])
            .expect("seed key");
        let handle_for_kick = handle;
        let kick_thread = std::thread::spawn(move || {
            let cid = client_id_of(&handle_for_kick);
            kick(&handle_for_kick, cid);
        });
        let outcome = probe_over(
            &runtime,
            &config(&root),
            777000123,
            &(vault as Arc<dyn SecretVault>),
        )
        .expect("probe");
        kick_thread.join().expect("kick thread");
        assert_eq!(
            outcome,
            AuthProbeOutcome::SignedOut {
                kind: "wait-phone-number".to_owned()
            }
        );
    }
}
