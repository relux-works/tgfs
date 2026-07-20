//! The exported account-removal surface: the SEC-004 cleanup sequence,
//! driven end-to-end by the engine host (BUG-260720-3i74u1).
//!
//! Wraps the source crate's journaled [`AccountRemoval`] machine — every
//! stage effect-before-record and idempotent, so an interrupted removal
//! re-runs into a completed one — and owns the two caller stages the crate
//! leaves to its host: quiescing (trivial today: the v1 engine host runs no
//! long-lived client) and the durable-state purge
//! (`WriteTxn::purge_account`). The session-termination stage opens the
//! account's client only when there is a session to terminate (a stored
//! database key and an account directory); a data root with neither — or a
//! removal re-run after the wipe already happened — never touches the
//! Telegram runtime at all, so a local-only removal works even in builds
//! that carry none.
//!
//! Removal is deliberately a free operation rather than a session: it must
//! converge with no user interaction, and its progress protocol is the
//! journal, not a listener.

use std::sync::Arc;
use std::time::Duration;

use gramdrive_model::identity::AccountId;
use gramdrive_source_tdjson::auth::{AuthMachine, AuthState};
use gramdrive_source_tdjson::removal::{
    AccountRemoval, RemovalError, RemovalMode, RemovalRequest, RemovalStep,
};
use gramdrive_source_tdjson::runtime::{TdRuntime, UpdateRecvError};
use serde_json::json;

use crate::api::DriveError;
use crate::auth::{
    AuthSessionConfig, ScopeGuard, SecretVault, VaultSecrets, shared_runtime, shared_state_store,
};

/// How long the terminate stage waits for TDLib to confirm the close.
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(60);

/// Removes an account: server-side logout (when `revoke_session`), the
/// on-disk Telegram-state wipe, the keychain revocation, and the durable
/// account row's purge — the engine half of the SEC-004 sequence. Provider
/// (File Provider domain) deregistration is the app's half and runs after
/// this returns.
///
/// Idempotent end to end: re-running after any interruption resumes the
/// journal, and removing an account that is already gone is success.
#[uniffi::export(async_runtime = "tokio")]
pub async fn remove_account(
    config: AuthSessionConfig,
    account_id: i64,
    revoke_session: bool,
    vault: Arc<dyn SecretVault>,
) -> Result<(), DriveError> {
    tokio::task::spawn_blocking(move || {
        remove_over(shared_runtime, &config, account_id, revoke_session, &vault)
    })
    .await
    .map_err(|error| DriveError::Internal {
        detail: format!("removal task: {error}"),
    })?
}

/// The removal driver over an explicit (lazy) runtime source — the seam the
/// in-crate tests use with the deterministic mock. The runtime is resolved
/// only if a session actually needs terminating.
pub(crate) fn remove_over(
    runtime: impl Fn() -> Result<Arc<TdRuntime>, DriveError>,
    config: &AuthSessionConfig,
    account_id: i64,
    revoke_session: bool,
    vault: &Arc<dyn SecretVault>,
) -> Result<(), DriveError> {
    config.validate()?;
    if account_id <= 0 {
        return Err(DriveError::InvalidArgument {
            detail: "account_id must be a positive Telegram identity".to_owned(),
        });
    }
    let account = AccountId(account_id);
    let _guard = ScopeGuard::acquire(&config.data_dir, account)?;

    let layout = config.storage_layout();
    let mode = if revoke_session {
        RemovalMode::RevokeSession
    } else {
        RemovalMode::LocalOnly
    };
    let mut removal =
        AccountRemoval::begin(layout, RemovalRequest::new(account, mode)).map_err(removal_error)?;

    while let Some(step) = removal.next_pending() {
        match step {
            // The v1 engine host runs no long-lived client for the account,
            // so there is nothing to quiesce in this process.
            RemovalStep::SignalQuiesce => {}
            RemovalStep::TerminateSession => {
                terminate_session(&runtime, config, account, vault, &removal)?;
            }
            RemovalStep::WipeDatabase => removal.wipe_storage().map_err(removal_error)?,
            RemovalStep::WipeExports => removal.wipe_exports().map_err(removal_error)?,
            // The keychain revocation runs through the vault seam directly:
            // the source crate's `SecretStore` cannot be implemented outside
            // it (`DatabaseKey` releases no bytes, by design), and the
            // effect — the account's key is gone — is the same.
            RemovalStep::RevokeKeychain => vault.delete_database_key(account.0)?,
            RemovalStep::PurgeState => {
                let mut store = shared_state_store(&config.data_dir)?;
                let txn = store.write_txn().map_err(state_error)?;
                txn.purge_account(account).map_err(state_error)?;
                txn.commit().map_err(state_error)?;
            }
        }
        removal.complete(step).map_err(removal_error)?;
    }
    removal.finalize().map_err(removal_error)
}

/// Ends the account's Telegram session: nothing to do when the account has
/// no stored key or no on-disk state; otherwise open its client, submit the
/// mode's request (`logOut` / `close`), and wait for TDLib to confirm the
/// client closed.
fn terminate_session(
    runtime: &impl Fn() -> Result<Arc<TdRuntime>, DriveError>,
    config: &AuthSessionConfig,
    account: AccountId,
    vault: &Arc<dyn SecretVault>,
    removal: &AccountRemoval,
) -> Result<(), DriveError> {
    let has_key = vault.database_key(account.0)?.is_some();
    let has_state = config.storage_layout().account_dir(account).exists();
    if !has_key || !has_state {
        return Ok(());
    }

    let secrets = VaultSecrets::read_only(Arc::clone(vault));
    let tdlib_config = config.tdlib_config(account, &secrets)?;
    let runtime = runtime()?;
    let (client, updates) = runtime.create_client().map_err(td_error)?;
    let mut machine = AuthMachine::new(tdlib_config);
    // Activate: TDLib starts reporting authorization state.
    drop(client.request(json!({"@type": "getOption", "name": "version"})));

    let deadline = std::time::Instant::now() + TERMINATE_TIMEOUT;
    let mut terminated = false;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(DriveError::SourceUnavailable {
                detail: "the source did not confirm the session's end".to_owned(),
            });
        }
        let update = match updates.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(update) => update,
            Err(UpdateRecvError::Timeout) => continue,
            // The runtime ends the stream once the client closed — the
            // confirmation itself when the update was consumed internally.
            Err(UpdateRecvError::Closed) => return Ok(()),
        };
        let step = match machine.on_update(&update) {
            Ok(step) => step,
            Err(_error) => continue,
        };
        for request in step.requests {
            if let Ok(pending) = client.request(request) {
                // Bound the plumbing wait by the outer deadline so the whole
                // terminate stage stays within TERMINATE_TIMEOUT rather than
                // ~doubling it across the loop's inner waits.
                let wait = deadline.saturating_duration_since(std::time::Instant::now());
                drop(pending.wait_timeout(wait));
            }
        }
        match step.entered {
            Some(AuthState::Closed) => return Ok(()),
            // The first definitive state is the point to submit the
            // termination request; earlier the client is still configuring.
            Some(
                AuthState::Ready
                | AuthState::WaitPhoneNumber
                | AuthState::WaitCode(_)
                | AuthState::WaitQrConfirmation { .. }
                | AuthState::WaitPassword(_)
                | AuthState::Unsupported { .. },
            ) if !terminated => {
                terminated = true;
                if let Ok(pending) = client.request(removal.session_request()) {
                    let wait = deadline.saturating_duration_since(std::time::Instant::now());
                    drop(pending.wait_timeout(wait));
                }
            }
            _ => {}
        }
    }
}

fn removal_error(error: RemovalError) -> DriveError {
    match error {
        RemovalError::Io { step, source } => DriveError::Storage {
            detail: format!("removal {step:?}: {source}"),
        },
        RemovalError::Secret(error) => DriveError::Storage {
            detail: format!("removal keychain: {error}"),
        },
        RemovalError::Journal { detail } => DriveError::Storage {
            detail: format!("removal journal: {detail}"),
        },
        RemovalError::InProgress { account } => DriveError::InvalidArgument {
            detail: format!("a removal of account {account} is already running"),
        },
        RemovalError::Incomplete { account, next } => DriveError::Internal {
            detail: format!("removal of account {account} stopped before {next:?}"),
        },
    }
}

fn state_error(error: impl std::fmt::Display) -> DriveError {
    DriveError::Storage {
        detail: format!("state purge: {error}"),
    }
}

fn td_error(error: gramdrive_source_tdjson::error::TdError) -> DriveError {
    DriveError::SourceUnavailable {
        detail: format!("source runtime: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tests_support::{
        FakeVault, TempRoot, auth_update, config, ok_response, start_runtime,
    };
    use crate::shared_state::{SharedStateStore, StateRole};
    use gramdrive_model::identity::{AccountKey, AccountScope, NamespaceVersion};
    use gramdrive_source_tdjson::mock::SentRequest;
    use gramdrive_state::repo::{AccountRecord, RetentionMode, SourceKind};

    const ACCOUNT: i64 = 777000123;

    const READY: &str = r#"{"@type":"authorizationStateReady"}"#;
    const LOGGING_OUT: &str = r#"{"@type":"authorizationStateLoggingOut"}"#;
    const CLOSING: &str = r#"{"@type":"authorizationStateClosing"}"#;
    const CLOSED: &str = r#"{"@type":"authorizationStateClosed"}"#;

    fn seed_account(root: &TempRoot, config: &AuthSessionConfig) {
        // Durable row.
        drop(
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Coordinator)
                .expect("coordinator open"),
        );
        let mut store = shared_state_store(root.as_str()).expect("state store");
        let txn = store.write_txn().expect("write");
        txn.upsert_account(&AccountRecord {
            account: AccountKey {
                account_id: AccountId(ACCOUNT),
            },
            source_kind: SourceKind::LocalTdlib,
            display_name: "Doomed".to_owned(),
            auth_state: "authorized".to_owned(),
            namespace_version: AccountScope {
                account: AccountKey {
                    account_id: AccountId(ACCOUNT),
                },
                namespace_version: NamespaceVersion(1),
            }
            .namespace_version,
            retention_mode: RetentionMode::Mirror,
            archive_mode: false,
            secret_ref: None,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
        })
        .expect("account row");
        txn.commit().expect("commit");
        // On-disk Telegram state.
        let paths = config.storage_layout().account_paths(AccountId(ACCOUNT));
        std::fs::create_dir_all(paths.database_directory()).expect("tdlib dir");
        std::fs::write(paths.database_directory().join("db.sqlite"), b"td").expect("db file");
    }

    fn logout_responder() -> impl FnMut(&SentRequest) -> Vec<String> + Send + 'static {
        |sent: &SentRequest| {
            let extra = sent.extra().expect("extra");
            let cid = sent.client_id;
            match sent.request_type().as_deref() {
                Some("setTdlibParameters") => {
                    vec![ok_response(extra, cid), auth_update(cid, READY)]
                }
                Some("logOut") => vec![
                    ok_response(extra, cid),
                    auth_update(cid, LOGGING_OUT),
                    auth_update(cid, CLOSING),
                    auth_update(cid, CLOSED),
                ],
                Some("close") => vec![
                    ok_response(extra, cid),
                    auth_update(cid, CLOSING),
                    auth_update(cid, CLOSED),
                ],
                _ => vec![ok_response(extra, cid)],
            }
        }
    }

    #[test]
    fn removal_terminates_wipes_revokes_and_purges() {
        let (runtime, handle) = start_runtime();
        handle.set_responder(logout_responder());
        let root = TempRoot::new();
        let cfg = config(&root);
        seed_account(&root, &cfg);
        let vault = Arc::new(FakeVault::default());
        vault
            .store_database_key(ACCOUNT, vec![7u8; 32])
            .expect("seed key");

        // The terminate stage activates a client; kick it like TDLib would.
        let kick_handle = handle;
        let kicker = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(sent) = kick_handle.take_sent().first() {
                    kick_handle.push_event(&auth_update(
                        sent.client_id,
                        r#"{"@type":"authorizationStateWaitTdlibParameters"}"#,
                    ));
                    return;
                }
                assert!(std::time::Instant::now() < deadline, "no activation");
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let vault_dyn = Arc::clone(&vault) as Arc<dyn SecretVault>;
        remove_over(|| Ok(Arc::clone(&runtime)), &cfg, ACCOUNT, true, &vault_dyn).expect("removal");
        kicker.join().expect("kicker");

        // On-disk state gone, key gone, row gone.
        assert!(
            !cfg.storage_layout()
                .account_dir(AccountId(ACCOUNT))
                .exists()
        );
        assert!(vault.key(ACCOUNT).is_none());
        let store = SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider)
            .expect("provider open");
        assert!(store.accounts().expect("accounts").is_empty());
    }

    #[test]
    fn removal_of_an_absent_account_never_touches_the_runtime() {
        let root = TempRoot::new();
        let cfg = config(&root);
        drop(
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Coordinator)
                .expect("coordinator open"),
        );
        let vault = Arc::new(FakeVault::default()) as Arc<dyn SecretVault>;
        remove_over(
            || -> Result<Arc<TdRuntime>, DriveError> {
                panic!("the runtime must not be resolved for an absent account")
            },
            &cfg,
            ACCOUNT,
            false,
            &vault,
        )
        .expect("removal converges");
    }
}
