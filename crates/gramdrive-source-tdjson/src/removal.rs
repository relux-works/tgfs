//! The account-removal workflow (TASK-260715-wjaux5, SEC-004).
//!
//! Removing an account is a *destructive, crash-resumable* sequence, not one
//! call. [`AccountRemoval`] sequences the SEC-004 cleanup — quiesce transfers,
//! terminate the session, wipe the on-disk database and cached exports, revoke
//! the keychain key, purge the state rows — behind a durable journal so a
//! crash between any two stages resumes instead of stranding an account
//! half-removed.
//!
//! # Telegram logout versus local-only removal
//!
//! The one product-visible choice this workflow encodes is [`RemovalMode`],
//! and it is deliberately explicit (the story's "Telegram logout versus
//! local-only removal is clearly distinguished"):
//!
//! - [`RemovalMode::RevokeSession`] submits `logOut`. Telegram terminates
//!   *this* authorization server-side — the session disappears from the
//!   account's active-sessions list — and TDLib deletes its own local
//!   database as part of the operation before reporting
//!   `authorizationStateClosed`. Re-adding the account is a full new sign-in.
//! - [`RemovalMode::LocalOnly`] submits `close`. The Telegram session is left
//!   **untouched on the server**; only this device's local state is torn down.
//!   It is the offline / "just make this app forget the account" path, and it
//!   knowingly leaves a session Telegram still lists (the local auth key is
//!   wiped with everything else, so this device can no longer drive it). That
//!   tradeoff is the whole reason the two modes are distinct rather than one.
//!
//! Everything after the session step — the on-disk wipe, keychain revocation,
//! state purge — is identical between the two modes. Only the request built by
//! [`RemovalMode::session_request`], and what it means to Telegram, differs.
//!
//! # Layering: what this crate owns, and what it can only direct
//!
//! `gramdrive-source-tdjson` sits at layer 1 and may depend only on
//! `gramdrive-model` and `gramdrive-source` (`crates/README.md`). Two stages
//! of the removal sequence act on crates above it — cancelling in-flight
//! transfers and unregistering provider state is the engine's job
//! (`gramdrive-engine`, layer 2), purging the account's rows is the state
//! store's (`gramdrive-state`, layer 1, composed only at layer 2/3). This
//! crate cannot call either without inverting the dependency direction, so
//! [`RemovalStep::SignalQuiesce`] and [`RemovalStep::PurgeState`] are *typed
//! directives*: the workflow sequences and checkpoints them, and the composing
//! caller (the engine, or the FFI boundary) supplies the effect. The stages
//! this crate does own — the session request, the on-disk wipe, keychain
//! revocation, the journal — it executes directly ([`AccountRemoval`]'s
//! executor methods), the same way [`crate::config::StorageLayout::wipe_account`]
//! already owns real `std::fs`.
//!
//! # Crash-resume: the effect-before-record invariant
//!
//! The [`AccountRemoval`] driver is a loop: read [`AccountRemoval::next_pending`],
//! perform that step's effect, then durably [`AccountRemoval::complete`] it.
//! Every stage is **idempotent** (a missing directory, an absent key, an
//! already-closed client are all success), which makes that ordering
//! crash-safe by construction: a crash *after* an effect but *before* its
//! record simply re-runs the idempotent effect on resume; a crash *after* the
//! record skips it. There is no window in which a stage is neither redone nor
//! skipped — the AC's "every stage is idempotent; partial failure resumes".
//!
//! The journal that carries the checkpoint lives outside the account's own
//! subtree (`crate::config::StorageLayout` root, [`journal`]) precisely so
//! [`RemovalStep::WipeDatabase`] cannot delete the record of its own progress;
//! [`AccountRemoval::finalize`] removes it last, after which no trace of the
//! account remains.
//!
//! # Concurrency fails safe
//!
//! While a removal is in flight the account is being torn down and must not be
//! opened. [`AccountRemoval::guard_open`] consults the journal and refuses
//! ([`RemovalError::InProgress`]) so a concurrent open sees a clean typed
//! failure rather than a half-wiped database — the account-open path calls it
//! before [`crate::config::AccountConfig::resolve`]. Two removal drivers for
//! the same account converge instead of racing: [`AccountRemoval::begin`]
//! adopts an existing journal rather than starting a second one, and the
//! stages are idempotent even if both run.

mod journal;

use std::path::PathBuf;

use gramdrive_model::identity::AccountId;
use serde_json::{Value, json};

use crate::config::{SecretError, SecretStore, StorageLayout};

/// Whether removal revokes the Telegram session or only tears down local
/// state. The workflow's one product-visible choice; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalMode {
    /// Server-side logout (`logOut`): Telegram terminates this authorization
    /// and TDLib deletes its local database before closing. Re-adding is a
    /// full new sign-in.
    RevokeSession,
    /// Local-only removal (`close`): the Telegram session is left intact on
    /// the server; only this device's local state is removed.
    LocalOnly,
}

impl RemovalMode {
    /// The session-termination request for this mode — the one stage where
    /// the two modes differ. `logOut` for [`RemovalMode::RevokeSession`],
    /// `close` for [`RemovalMode::LocalOnly`]; the runtime submits it and the
    /// resulting `authorizationStateClosed` update ends the client
    /// ([`RemovalStep::TerminateSession`]).
    pub fn session_request(self) -> Value {
        match self {
            RemovalMode::RevokeSession => json!({ "@type": "logOut" }),
            RemovalMode::LocalOnly => json!({ "@type": "close" }),
        }
    }

    /// The stable journal token for this mode.
    fn as_str(self) -> &'static str {
        match self {
            RemovalMode::RevokeSession => "revoke_session",
            RemovalMode::LocalOnly => "local_only",
        }
    }

    /// Parse a journal token back into a mode.
    fn parse(text: &str) -> Option<RemovalMode> {
        match text {
            "revoke_session" => Some(RemovalMode::RevokeSession),
            "local_only" => Some(RemovalMode::LocalOnly),
            _ => None,
        }
    }
}

/// Whether the account's cached exports (rendered NDJSON/Markdown and any
/// other host-owned generated files) are discarded or kept — the task's
/// "remove or retain cached exports per explicit user choice".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportPolicy {
    /// Delete the account's export directories along with everything else.
    Discard,
    /// Keep the export directories; the [`RemovalStep::WipeExports`] stage is
    /// omitted from the plan entirely.
    Retain,
}

impl ExportPolicy {
    /// The stable journal token for this policy.
    fn as_str(self) -> &'static str {
        match self {
            ExportPolicy::Discard => "discard",
            ExportPolicy::Retain => "retain",
        }
    }

    /// Parse a journal token back into a policy.
    fn parse(text: &str) -> Option<ExportPolicy> {
        match text {
            "discard" => Some(ExportPolicy::Discard),
            "retain" => Some(ExportPolicy::Retain),
            _ => None,
        }
    }
}

/// One stage of the SEC-004 removal sequence.
///
/// The order is a teardown order: stop the writers before wiping what they
/// write to, terminate the session before deleting the database it opened,
/// and drop the keychain key and state rows last. Which stages this crate
/// executes and which it only directs is in the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalStep {
    /// Caller-owned: cancel the account's in-flight transfers and unregister
    /// its provider state so nothing writes to the storage about to be wiped
    /// (the engine, layer 2).
    SignalQuiesce,
    /// Submit the mode's [`RemovalMode::session_request`] and wait for the
    /// client to reach `authorizationStateClosed`. An already-closed client
    /// (`ClientClosed`) satisfies this stage — it is the state the stage
    /// drives toward, whether this attempt or a prior one reached it.
    TerminateSession,
    /// Remove the account's TDLib database and files subtree
    /// ([`AccountRemoval::wipe_storage`]).
    WipeDatabase,
    /// Remove the account's cached export directories
    /// ([`AccountRemoval::wipe_exports`]). Present only under
    /// [`ExportPolicy::Discard`].
    WipeExports,
    /// Revoke the account's keychain key ([`AccountRemoval::revoke_keychain`]).
    RevokeKeychain,
    /// Caller-owned: purge the account's rows from the state store
    /// (`gramdrive-state`, composed at layer 2/3).
    PurgeState,
}

impl RemovalStep {
    /// The stable journal token for this step.
    fn as_str(self) -> &'static str {
        match self {
            RemovalStep::SignalQuiesce => "signal_quiesce",
            RemovalStep::TerminateSession => "terminate_session",
            RemovalStep::WipeDatabase => "wipe_database",
            RemovalStep::WipeExports => "wipe_exports",
            RemovalStep::RevokeKeychain => "revoke_keychain",
            RemovalStep::PurgeState => "purge_state",
        }
    }

    /// Parse a journal token back into a step.
    fn parse(text: &str) -> Option<RemovalStep> {
        match text {
            "signal_quiesce" => Some(RemovalStep::SignalQuiesce),
            "terminate_session" => Some(RemovalStep::TerminateSession),
            "wipe_database" => Some(RemovalStep::WipeDatabase),
            "wipe_exports" => Some(RemovalStep::WipeExports),
            "revoke_keychain" => Some(RemovalStep::RevokeKeychain),
            "purge_state" => Some(RemovalStep::PurgeState),
            _ => None,
        }
    }
}

/// The immutable parameters of one account's removal.
#[derive(Debug, Clone)]
pub struct RemovalRequest {
    /// The account to remove.
    pub account: AccountId,
    /// Telegram logout versus local-only removal.
    pub mode: RemovalMode,
    /// Whether cached exports are discarded or kept.
    pub exports: ExportPolicy,
    /// The account's cached-export directories, as chosen by the host. Each
    /// must be a directory the host owns for this account alone: the wipe
    /// removes each subtree whole, so a shared or misrooted path would take
    /// unrelated data with it. Ignored entirely under [`ExportPolicy::Retain`].
    pub export_dirs: Vec<PathBuf>,
}

impl RemovalRequest {
    /// A request removing `account` in `mode`, discarding its exports and with
    /// no export directories registered — the common single-account default.
    /// Set [`RemovalRequest::exports`]/[`RemovalRequest::export_dirs`] to
    /// override.
    pub fn new(account: AccountId, mode: RemovalMode) -> RemovalRequest {
        RemovalRequest {
            account,
            mode,
            exports: ExportPolicy::Discard,
            export_dirs: Vec::new(),
        }
    }
}

/// Why an account-removal operation failed.
///
/// Carries no secret material: an [`RemovalError::Io`] source may name the
/// account's own directory path, never key material or chat content.
#[derive(Debug)]
pub enum RemovalError {
    /// A filesystem stage failed for a reason other than a missing target (a
    /// missing target is success — removal must converge).
    Io {
        /// The stage that failed.
        step: RemovalStep,
        /// The underlying filesystem error.
        source: std::io::Error,
    },
    /// The keychain revocation failed.
    Secret(SecretError),
    /// The persisted removal journal could not be read, written, or parsed.
    Journal {
        /// Diagnostic detail; carries no secret material.
        detail: String,
    },
    /// An account cannot be opened because a removal for it is in progress
    /// (the fail-safe guard, [`AccountRemoval::guard_open`]).
    InProgress {
        /// The account under removal.
        account: i64,
    },
    /// [`AccountRemoval::finalize`] was called before every stage completed;
    /// the journal is kept so the removal can still resume.
    Incomplete {
        /// The account whose removal is unfinished.
        account: i64,
        /// The next stage still to run.
        next: RemovalStep,
    },
}

impl std::fmt::Display for RemovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemovalError::Io { step, source } => {
                write!(f, "removal stage {} failed: {source}", step.as_str())
            }
            RemovalError::Secret(err) => write!(f, "keychain revocation failed: {err}"),
            RemovalError::Journal { detail } => write!(f, "removal journal error: {detail}"),
            RemovalError::InProgress { account } => {
                write!(f, "account {account} is being removed")
            }
            RemovalError::Incomplete { account, next } => write!(
                f,
                "removal of account {account} is incomplete; next stage is {}",
                next.as_str()
            ),
        }
    }
}

impl std::error::Error for RemovalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RemovalError::Io { source, .. } => Some(source),
            RemovalError::Secret(err) => Some(err),
            _ => None,
        }
    }
}

/// A resumable, crash-safe removal of one account.
///
/// Construct with [`AccountRemoval::begin`] to start (or adopt) a removal, or
/// with [`AccountRemoval::pending`] to resume every in-progress removal after
/// a restart. Drive it as a loop over [`AccountRemoval::next_pending`]:
/// perform each step's effect (this crate's executor methods for the stages it
/// owns; the caller's own effect for [`RemovalStep::SignalQuiesce`] and
/// [`RemovalStep::PurgeState`]), then durably [`AccountRemoval::complete`] it.
/// When [`AccountRemoval::is_complete`], call [`AccountRemoval::finalize`].
#[derive(Debug)]
pub struct AccountRemoval {
    layout: StorageLayout,
    request: RemovalRequest,
    completed: Vec<RemovalStep>,
}

impl AccountRemoval {
    /// Start removing `request.account`, or adopt the removal already in
    /// progress for it.
    ///
    /// If a journal already exists (a prior, possibly crashed, removal), it is
    /// authoritative: `begin` resumes from its recorded progress and its
    /// original mode/exports, and the freshly passed `request`'s mode and
    /// export choices are ignored — a removal that already ran `logOut` cannot
    /// become local-only. Otherwise a fresh journal is written before any
    /// destructive stage runs, so the account is guarded from the first call.
    pub fn begin(
        layout: StorageLayout,
        request: RemovalRequest,
    ) -> Result<AccountRemoval, RemovalError> {
        if let Some(record) = journal::read(layout.root(), request.account)? {
            return Ok(AccountRemoval {
                layout,
                request: record.request,
                completed: record.completed,
            });
        }
        let removal = AccountRemoval {
            layout,
            request,
            completed: Vec::new(),
        };
        removal.persist()?;
        Ok(removal)
    }

    /// Every in-progress removal under `layout`, in account order — the
    /// crash-recovery entry point. Run each returned handle to completion on
    /// startup before opening any account.
    pub fn pending(layout: &StorageLayout) -> Result<Vec<AccountRemoval>, RemovalError> {
        let mut removals: Vec<AccountRemoval> = journal::list(layout.root())?
            .into_iter()
            .map(|record| AccountRemoval {
                layout: layout.clone(),
                request: record.request,
                completed: record.completed,
            })
            .collect();
        removals.sort_by_key(|removal| removal.request.account.0);
        Ok(removals)
    }

    /// Whether a removal is in progress for `account` under `layout`.
    pub fn is_pending(layout: &StorageLayout, account: AccountId) -> Result<bool, RemovalError> {
        journal::exists(layout.root(), account)
    }

    /// The fail-safe guard the account-open path calls before using an
    /// account: `Err(`[`RemovalError::InProgress`]`)` when a removal is in
    /// flight, `Ok(())` otherwise. Refusing here is what keeps a concurrent
    /// open from ever observing a half-wiped database.
    pub fn guard_open(layout: &StorageLayout, account: AccountId) -> Result<(), RemovalError> {
        if Self::is_pending(layout, account)? {
            return Err(RemovalError::InProgress { account: account.0 });
        }
        Ok(())
    }

    /// The account being removed.
    pub fn account(&self) -> AccountId {
        self.request.account
    }

    /// The removal's mode.
    pub fn mode(&self) -> RemovalMode {
        self.request.mode
    }

    /// The ordered stages for this removal. [`RemovalStep::WipeExports`] is
    /// present only under [`ExportPolicy::Discard`]; every other stage always
    /// runs, in both modes.
    pub fn plan(&self) -> Vec<RemovalStep> {
        let mut steps = vec![
            RemovalStep::SignalQuiesce,
            RemovalStep::TerminateSession,
            RemovalStep::WipeDatabase,
        ];
        if self.request.exports == ExportPolicy::Discard {
            steps.push(RemovalStep::WipeExports);
        }
        steps.push(RemovalStep::RevokeKeychain);
        steps.push(RemovalStep::PurgeState);
        steps
    }

    /// The first stage of [`AccountRemoval::plan`] not yet completed, or
    /// `None` when the removal is done.
    pub fn next_pending(&self) -> Option<RemovalStep> {
        self.plan()
            .into_iter()
            .find(|step| !self.completed.contains(step))
    }

    /// Whether every stage has completed.
    pub fn is_complete(&self) -> bool {
        self.next_pending().is_none()
    }

    /// The session-termination request to submit for
    /// [`RemovalStep::TerminateSession`] — `logOut` or `close` per the mode.
    pub fn session_request(&self) -> Value {
        self.request.mode.session_request()
    }

    /// Execute [`RemovalStep::WipeDatabase`]: remove the account's TDLib
    /// database and files subtree. Idempotent — a missing subtree is success.
    pub fn wipe_storage(&self) -> Result<(), RemovalError> {
        self.layout
            .wipe_account(self.request.account)
            .map_err(|source| RemovalError::Io {
                step: RemovalStep::WipeDatabase,
                source,
            })
    }

    /// Execute [`RemovalStep::WipeExports`]: remove each registered export
    /// directory. Idempotent — a missing directory is success. A no-op when no
    /// export directories are registered.
    pub fn wipe_exports(&self) -> Result<(), RemovalError> {
        for dir in &self.request.export_dirs {
            match std::fs::remove_dir_all(dir) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(RemovalError::Io {
                        step: RemovalStep::WipeExports,
                        source,
                    });
                }
            }
        }
        Ok(())
    }

    /// Execute [`RemovalStep::RevokeKeychain`]: drop the account's database
    /// key from secure storage. Idempotent — an absent key is success.
    pub fn revoke_keychain(&self, store: &dyn SecretStore) -> Result<(), RemovalError> {
        store
            .delete_account(self.request.account)
            .map_err(RemovalError::Secret)
    }

    /// Durably record `step` as complete. Call it only after the step's effect
    /// is done (the effect-before-record invariant, module docs); idempotent,
    /// so re-recording a completed step is a no-op write.
    pub fn complete(&mut self, step: RemovalStep) -> Result<(), RemovalError> {
        if !self.completed.contains(&step) {
            self.completed.push(step);
        }
        self.persist()
    }

    /// Remove the journal, finishing the removal and leaving no trace of the
    /// account. Refuses with [`RemovalError::Incomplete`] (keeping the
    /// journal) if any stage is still pending, so a premature call cannot
    /// strand a half-removed account. Idempotent once complete.
    pub fn finalize(self) -> Result<(), RemovalError> {
        if let Some(next) = self.next_pending() {
            return Err(RemovalError::Incomplete {
                account: self.request.account.0,
                next,
            });
        }
        journal::remove(self.layout.root(), self.request.account)
    }

    /// Write the current request and progress to the journal atomically.
    fn persist(&self) -> Result<(), RemovalError> {
        journal::write(
            self.layout.root(),
            &journal::RemovalRecord {
                request: self.request.clone(),
                completed: self.completed.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> StorageLayout {
        StorageLayout::new("/root")
    }

    #[test]
    fn mode_selects_logout_or_close() {
        assert_eq!(
            RemovalMode::RevokeSession.session_request()["@type"],
            "logOut"
        );
        assert_eq!(RemovalMode::LocalOnly.session_request()["@type"], "close");
    }

    #[test]
    fn plan_orders_the_teardown_and_omits_exports_on_retain() {
        let discard = AccountRemoval {
            layout: layout(),
            request: RemovalRequest::new(AccountId(7), RemovalMode::RevokeSession),
            completed: Vec::new(),
        };
        assert_eq!(
            discard.plan(),
            vec![
                RemovalStep::SignalQuiesce,
                RemovalStep::TerminateSession,
                RemovalStep::WipeDatabase,
                RemovalStep::WipeExports,
                RemovalStep::RevokeKeychain,
                RemovalStep::PurgeState,
            ]
        );

        let mut req = RemovalRequest::new(AccountId(7), RemovalMode::LocalOnly);
        req.exports = ExportPolicy::Retain;
        let retain = AccountRemoval {
            layout: layout(),
            request: req,
            completed: Vec::new(),
        };
        assert!(!retain.plan().contains(&RemovalStep::WipeExports));
        // Local-only still tears down everything else.
        assert!(retain.plan().contains(&RemovalStep::WipeDatabase));
        assert!(retain.plan().contains(&RemovalStep::RevokeKeychain));
    }

    #[test]
    fn next_pending_walks_the_plan_in_order() {
        let mut removal = AccountRemoval {
            layout: layout(),
            request: RemovalRequest::new(AccountId(7), RemovalMode::RevokeSession),
            completed: Vec::new(),
        };
        assert_eq!(removal.next_pending(), Some(RemovalStep::SignalQuiesce));
        // Marking out of order still advances to the earliest remaining step.
        removal.completed.push(RemovalStep::SignalQuiesce);
        assert_eq!(removal.next_pending(), Some(RemovalStep::TerminateSession));
        for step in removal.plan() {
            if !removal.completed.contains(&step) {
                removal.completed.push(step);
            }
        }
        assert_eq!(removal.next_pending(), None);
        assert!(removal.is_complete());
    }

    #[test]
    fn token_round_trips_are_stable() {
        for mode in [RemovalMode::RevokeSession, RemovalMode::LocalOnly] {
            assert_eq!(RemovalMode::parse(mode.as_str()), Some(mode));
        }
        for policy in [ExportPolicy::Discard, ExportPolicy::Retain] {
            assert_eq!(ExportPolicy::parse(policy.as_str()), Some(policy));
        }
        for step in [
            RemovalStep::SignalQuiesce,
            RemovalStep::TerminateSession,
            RemovalStep::WipeDatabase,
            RemovalStep::WipeExports,
            RemovalStep::RevokeKeychain,
            RemovalStep::PurgeState,
        ] {
            assert_eq!(RemovalStep::parse(step.as_str()), Some(step));
        }
        assert_eq!(RemovalMode::parse("nonsense"), None);
        assert_eq!(RemovalStep::parse("nonsense"), None);
    }
}
