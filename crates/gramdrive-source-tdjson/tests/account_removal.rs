//! Account-removal workflow fixtures (TASK-260715-wjaux5, SEC-004): a full
//! removal leaves no trace of the account on disk, a crash at any stage
//! resumes to the same converged end, every stage is idempotent, concurrent
//! access during a removal fails safe, and the two modes build the requests
//! Telegram logout versus local-only removal actually send — proven through
//! the real runtime over the deterministic mock.

// clippy.toml exempts test code; restated for the module-level bodies of this
// integration binary (matching the established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use common::{GUARD, echo_ok_responder, start_runtime, test_config};
use gramdrive_model::identity::AccountId;
use gramdrive_source_tdjson::config::{
    ApiCredentials, DatabaseKey, InMemorySecrets, Secret, SecretError, SecretSource, SecretStore,
    StorageLayout,
};
use gramdrive_source_tdjson::removal::{
    AccountRemoval, ExportPolicy, RemovalError, RemovalMode, RemovalRequest, RemovalStep,
};

const DB_KEY_LEN: usize = 32;

/// A unique-per-call temp directory (process id plus counter, no clock, no
/// randomness) — the crate's established fixture pattern.
fn temp_root() -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "gramdrive-account-removal-test-{}-{n}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn empty_store() -> InMemorySecrets {
    InMemorySecrets::new(ApiCredentials {
        api_id: 424242,
        api_hash: Secret::new("api-hash-sentinel"),
    })
}

fn key(tag: u8) -> DatabaseKey {
    DatabaseKey::from_entropy([tag; DB_KEY_LEN])
}

/// The export directory the host would register for `account`, distinct per
/// account so a wipe of one cannot reach another.
fn export_dir(root: &Path, account: AccountId) -> PathBuf {
    root.join("exports")
        .join(format!("account-{}-export", account.0))
}

/// Materialize a fully-configured account: its TDLib subtree with real files,
/// its export directory with a real file, and its keychain key — the state a
/// live account carries before removal.
fn materialize(layout: &StorageLayout, store: &InMemorySecrets, account: AccountId, tag: u8) {
    let paths = layout.account_paths(account);
    std::fs::create_dir_all(paths.database_directory()).unwrap();
    std::fs::create_dir_all(paths.files_directory()).unwrap();
    std::fs::write(paths.database_directory().join("db.binlog"), b"state").unwrap();
    std::fs::write(paths.files_directory().join("blob.bin"), b"content").unwrap();

    let exports = export_dir(layout.root(), account);
    std::fs::create_dir_all(&exports).unwrap();
    std::fs::write(exports.join("2024-01.md"), b"# rendered export").unwrap();

    store.put_database_key(account, key(tag)).unwrap();
}

/// A removal request that discards exports, wired to `account`'s export
/// directory.
fn request(layout: &StorageLayout, account: AccountId, mode: RemovalMode) -> RemovalRequest {
    RemovalRequest {
        account,
        mode,
        exports: ExportPolicy::Discard,
        export_dirs: vec![export_dir(layout.root(), account)],
    }
}

/// A stand-in for the crates this one only directs: which accounts still have
/// live transfers/provider registration (the engine), and which still have
/// state rows (the state store). The caller-owned removal stages act on these.
#[derive(Default)]
struct HostState {
    active: HashSet<i64>,
    rows: HashSet<i64>,
}

/// Execute one removal stage against real storage and the host stand-in, then
/// durably record it — the effect-before-record loop the module documents.
fn run_step(
    removal: &mut AccountRemoval,
    step: RemovalStep,
    store: &InMemorySecrets,
    host: &mut HostState,
) {
    let account = removal.account().0;
    match step {
        RemovalStep::SignalQuiesce => {
            // The engine cancels the account's transfers and unregisters it.
            host.active.remove(&account);
        }
        RemovalStep::TerminateSession => {
            // The runtime submits this and waits for the closed update; here we
            // only assert the request matches the mode.
            let expected = match removal.mode() {
                RemovalMode::RevokeSession => "logOut",
                RemovalMode::LocalOnly => "close",
            };
            assert_eq!(removal.session_request()["@type"], expected);
        }
        RemovalStep::WipeDatabase => removal.wipe_storage().unwrap(),
        RemovalStep::WipeExports => removal.wipe_exports().unwrap(),
        RemovalStep::RevokeKeychain => removal.revoke_keychain(store).unwrap(),
        RemovalStep::PurgeState => {
            // The state store deletes the account's rows.
            host.rows.remove(&account);
        }
    }
    removal.complete(step).unwrap();
}

/// Drive `removal` to completion, then finalize it.
fn drive(removal: &mut AccountRemoval, store: &InMemorySecrets, host: &mut HostState) {
    while let Some(step) = removal.next_pending() {
        run_step(removal, step, store, host);
    }
}

/// Every path under `root`, as forward-slashed strings — for asserting that
/// nothing referencing an account survives.
fn scan(root: &Path) -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            out.push(path.to_string_lossy().into_owned());
            if path.is_dir() {
                walk(&path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

#[test]
fn full_removal_leaves_no_trace_of_the_account_on_disk() {
    let root = temp_root();
    let layout = StorageLayout::new(&root);
    let store = empty_store();
    let victim = AccountId(7);
    let bystander = AccountId(8);
    let mut host = HostState::default();

    for (account, tag) in [(victim, 0x11u8), (bystander, 0x22u8)] {
        materialize(&layout, &store, account, tag);
        host.active.insert(account.0);
        host.rows.insert(account.0);
    }

    let mut removal = AccountRemoval::begin(
        layout.clone(),
        request(&layout, victim, RemovalMode::RevokeSession),
    )
    .unwrap();
    drive(&mut removal, &store, &mut host);
    removal.finalize().unwrap();

    // Nothing referencing the victim survives anywhere under the root: not the
    // subtree, not the exports, not the removal journal.
    let survivors = scan(&root);
    for path in &survivors {
        assert!(
            !path.contains("account-7"),
            "victim trace left on disk: {path}"
        );
    }
    // The keychain key, the transfers, and the state rows are gone too.
    assert!(matches!(
        store.database_key(victim),
        Err(SecretError::NotFound { .. })
    ));
    assert!(!host.active.contains(&victim.0));
    assert!(!host.rows.contains(&victim.0));

    // The bystander is untouched in every store (per-account isolation).
    assert!(layout.account_dir(bystander).exists());
    assert!(export_dir(&root, bystander).exists());
    assert!(store.database_key(bystander).is_ok());
    assert!(host.rows.contains(&bystander.0));

    // The account can be opened again — the guard no longer refuses it.
    AccountRemoval::guard_open(&layout, victim).unwrap();

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn removal_resumes_from_a_crash_at_every_stage() {
    let victim = AccountId(7);
    // A fresh fixture per crash point: complete `crash_after` stages, drop the
    // driver mid-removal (the journal persists), then recover through the
    // crash-recovery entry point and finish.
    let plan_len = 6; // SignalQuiesce..PurgeState, exports discarded.
    for crash_after in 0..=plan_len {
        let root = temp_root();
        let layout = StorageLayout::new(&root);
        let store = empty_store();
        let mut host = HostState::default();
        materialize(&layout, &store, victim, 0x11);
        host.active.insert(victim.0);
        host.rows.insert(victim.0);

        {
            let mut removal = AccountRemoval::begin(
                layout.clone(),
                request(&layout, victim, RemovalMode::RevokeSession),
            )
            .unwrap();
            for _ in 0..crash_after {
                let step = removal.next_pending().expect("stage remains");
                run_step(&mut removal, step, &store, &mut host);
            }
            // Simulated crash: the driver is dropped, only the journal remains.
        }

        // Recovery: on restart every in-progress removal is resumed. Even a
        // crash after the last stage recorded but before finalize still shows
        // up here, so the journal is never orphaned.
        let mut resumed = AccountRemoval::pending(&layout).unwrap();
        assert_eq!(resumed.len(), 1, "crash_after={crash_after}");
        let mut removal = resumed.remove(0);
        assert_eq!(removal.account(), victim);
        assert_eq!(removal.mode(), RemovalMode::RevokeSession);
        drive(&mut removal, &store, &mut host);
        removal.finalize().unwrap();

        // Converged identically regardless of where the crash landed.
        assert!(!layout.account_dir(victim).exists());
        assert!(!export_dir(&root, victim).exists());
        assert!(matches!(
            store.database_key(victim),
            Err(SecretError::NotFound { .. })
        ));
        assert!(!AccountRemoval::is_pending(&layout, victim).unwrap());

        std::fs::remove_dir_all(&root).ok();
    }
}

#[test]
fn owned_stages_are_idempotent_under_repeat() {
    let root = temp_root();
    let layout = StorageLayout::new(&root);
    let store = empty_store();
    let victim = AccountId(7);
    materialize(&layout, &store, victim, 0x11);

    let removal = AccountRemoval::begin(
        layout.clone(),
        request(&layout, victim, RemovalMode::LocalOnly),
    )
    .unwrap();

    // Each owned executor run twice: the second run acts on already-wiped
    // state and must still succeed (the crash-after-effect-before-record
    // window re-runs exactly this way).
    for _ in 0..2 {
        removal.wipe_storage().unwrap();
        removal.wipe_exports().unwrap();
        removal.revoke_keychain(&store).unwrap();
    }
    assert!(!layout.account_dir(victim).exists());
    assert!(!export_dir(&root, victim).exists());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn concurrent_access_during_removal_fails_safe() {
    let root = temp_root();
    let layout = StorageLayout::new(&root);
    let store = empty_store();
    let victim = AccountId(7);
    let mut host = HostState::default();
    materialize(&layout, &store, victim, 0x11);
    host.active.insert(victim.0);
    host.rows.insert(victim.0);

    let mut removal = AccountRemoval::begin(
        layout.clone(),
        request(&layout, victim, RemovalMode::RevokeSession),
    )
    .unwrap();

    // A reader spins on the open guard while the destructive stages run in
    // parallel. The barrier only bounds the reader's lifetime; the removal is
    // deliberately not finalized until the reader has joined, so every sample
    // it takes lands inside the destructive window.
    std::thread::scope(|scope| {
        let reader_layout = layout.clone();
        let reader = scope.spawn(move || {
            let mut samples = Vec::new();
            for _ in 0..200 {
                samples.push(AccountRemoval::guard_open(&reader_layout, victim));
            }
            samples
        });

        // Run the wipe stages while the reader samples.
        drive(&mut removal, &store, &mut host);

        let samples = reader.join().unwrap();
        // Every concurrent open during the removal was refused — no reader ever
        // saw a half-wiped account as usable.
        assert!(!samples.is_empty());
        for sample in samples {
            assert!(
                matches!(sample, Err(RemovalError::InProgress { account: 7 })),
                "a concurrent open was not refused during removal"
            );
        }
    });

    // Only now, after the reader has observed the whole window, is the removal
    // finalized — and the guard opens.
    removal.finalize().unwrap();
    AccountRemoval::guard_open(&layout, victim).unwrap();

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn begin_adopts_an_in_progress_removal_instead_of_starting_a_second() {
    let root = temp_root();
    let layout = StorageLayout::new(&root);
    let store = empty_store();
    let victim = AccountId(7);
    let mut host = HostState::default();
    materialize(&layout, &store, victim, 0x11);
    host.active.insert(victim.0);

    // Start a Telegram logout and get one stage in.
    let mut first = AccountRemoval::begin(
        layout.clone(),
        request(&layout, victim, RemovalMode::RevokeSession),
    )
    .unwrap();
    let step = first.next_pending().unwrap();
    run_step(&mut first, step, &store, &mut host);

    // A second begin — even asking for a different mode — adopts the original:
    // a removal that already committed to revoking the session cannot silently
    // downgrade to local-only, and progress is preserved.
    let second = AccountRemoval::begin(
        layout.clone(),
        request(&layout, victim, RemovalMode::LocalOnly),
    )
    .unwrap();
    assert_eq!(second.mode(), RemovalMode::RevokeSession);
    assert_eq!(second.next_pending(), Some(RemovalStep::TerminateSession));

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn retain_keeps_the_exports_but_removes_everything_else() {
    let root = temp_root();
    let layout = StorageLayout::new(&root);
    let store = empty_store();
    let victim = AccountId(7);
    let mut host = HostState::default();
    materialize(&layout, &store, victim, 0x11);
    host.rows.insert(victim.0);

    let mut req = request(&layout, victim, RemovalMode::LocalOnly);
    req.exports = ExportPolicy::Retain;
    let mut removal = AccountRemoval::begin(layout.clone(), req).unwrap();

    // The plan omits the export wipe entirely.
    assert!(!removal.plan().contains(&RemovalStep::WipeExports));
    drive(&mut removal, &store, &mut host);
    removal.finalize().unwrap();

    // Local state is gone; the retained exports survive.
    assert!(!layout.account_dir(victim).exists());
    assert!(matches!(
        store.database_key(victim),
        Err(SecretError::NotFound { .. })
    ));
    assert!(
        export_dir(&root, victim).join("2024-01.md").exists(),
        "retained export was deleted"
    );
    assert!(!AccountRemoval::is_pending(&layout, victim).unwrap());

    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn each_mode_builds_the_session_request_the_runtime_accepts() {
    // logOut for a Telegram logout, close for a local-only removal — proven to
    // be well-formed JSON the runtime correlates and answers, the same way the
    // startup sequence is proven. The request is a pure product of the mode, so
    // it needs no on-disk removal to build.
    for (mode, expected) in [
        (RemovalMode::RevokeSession, "logOut"),
        (RemovalMode::LocalOnly, "close"),
    ] {
        assert_eq!(mode.session_request()["@type"], expected);

        let (runtime, mock) = start_runtime(test_config());
        mock.set_responder(echo_ok_responder());
        let (client, _updates) = runtime.create_client().unwrap();
        let answer = client
            .request(mode.session_request())
            .expect("request accepted")
            .wait_timeout(GUARD)
            .expect("resolves")
            .unwrap();
        assert_eq!(answer["@type"], "ok");

        let sent = mock.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].request_type().unwrap(), expected);
    }
}
