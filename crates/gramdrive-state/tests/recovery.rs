//! Corruption probe and quarantine against real files
//! (TASK-260715-gnsa2s; `src/recovery.rs`).
//!
//! Corruption fixtures are deterministic: a file of non-SQLite bytes (the
//! `SQLITE_NOTADB` path) and a real database whose 16-byte header magic is
//! overwritten (the damaged-file path). Random mid-file damage would be
//! flakier, not stronger — it can land in free pages `quick_check` never
//! visits.

// clippy.toml exempts test code from unwrap/expect/panic lints; the
// exemption keys on `#[test]` fns, and these helpers sit at module level in
// an integration-test binary that links into no product artifact.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use gramdrive_state::{ProbeOutcome, StateStore, probe_database, quarantine_if_corrupt};

/// A unique *directory* under the OS temp dir with the database inside it,
/// so each test owns the whole tree quarantine writes into (`quarantine/`
/// is created next to the database). Removed on drop. Uniqueness comes from
/// the process id and a counter — no clock, no randomness.
struct StateDir {
    dir: PathBuf,
    db: PathBuf,
}

impl StateDir {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gramdrive-recovery-test-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create state dir");
        let db = dir.join("state.sqlite3");
        Self { dir, db }
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Creates a real database at the path.
fn create_database(state: &StateDir) {
    let mut store = StateStore::open(&state.db).expect("open");
    let tx = store.write_txn().expect("write txn");
    tx.upsert_account(&common::account_record())
        .expect("account");
    tx.commit().expect("commit");
}

/// Overwrites the 16-byte SQLite header magic with garbage, turning a real
/// database into a file SQLite refuses as `SQLITE_NOTADB`.
fn corrupt_header(state: &StateDir) {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(&state.db)
        .expect("open db file");
    file.seek(SeekFrom::Start(0)).expect("seek");
    file.write_all(b"definitely not db")
        .expect("overwrite header");
    file.sync_all().expect("sync");
}

#[test]
fn probe_reports_missing_for_a_path_with_no_file() {
    let state = StateDir::new();
    assert_eq!(
        probe_database(&state.db).expect("probe"),
        ProbeOutcome::Missing
    );
}

#[test]
fn probe_reports_healthy_for_a_real_database() {
    let state = StateDir::new();
    create_database(&state);
    assert_eq!(
        probe_database(&state.db).expect("probe"),
        ProbeOutcome::Healthy
    );
}

#[test]
fn probe_reports_corrupt_for_a_file_of_garbage_bytes() {
    let state = StateDir::new();
    fs::write(&state.db, b"this is not a sqlite database, not even close").expect("write");
    let outcome = probe_database(&state.db).expect("probe");
    assert!(
        matches!(outcome, ProbeOutcome::Corrupt { .. }),
        "garbage bytes must probe corrupt, got {outcome:?}"
    );
}

#[test]
fn probe_reports_corrupt_for_a_database_with_a_damaged_header() {
    let state = StateDir::new();
    create_database(&state);
    corrupt_header(&state);
    let outcome = probe_database(&state.db).expect("probe");
    assert!(
        matches!(outcome, ProbeOutcome::Corrupt { .. }),
        "damaged header must probe corrupt, got {outcome:?}"
    );
}

#[test]
fn quarantine_declines_missing_and_healthy_files() {
    let state = StateDir::new();
    assert!(
        quarantine_if_corrupt(&state.db)
            .expect("quarantine missing")
            .is_none(),
        "nothing to quarantine at an empty path"
    );
    create_database(&state);
    assert!(
        quarantine_if_corrupt(&state.db)
            .expect("quarantine healthy")
            .is_none(),
        "a healthy database must never be quarantined"
    );
    // And it stayed put.
    assert_eq!(
        probe_database(&state.db).expect("probe"),
        ProbeOutcome::Healthy
    );
}

#[test]
fn quarantine_moves_the_damaged_files_and_clears_the_path_for_a_fresh_open() {
    let state = StateDir::new();
    create_database(&state);
    corrupt_header(&state);

    let report = quarantine_if_corrupt(&state.db)
        .expect("quarantine")
        .expect("corrupt file must be quarantined");

    // The path is clear and the damaged bytes are preserved in quarantine.
    assert!(!state.db.exists(), "main file must be moved aside");
    assert!(report.moved.contains(&state.db));
    let quarantined_main = report
        .quarantine_dir
        .join(state.db.file_name().expect("file name"));
    assert!(quarantined_main.exists(), "damaged file must be preserved");
    assert!(!report.detail.is_empty(), "probe detail must be carried");

    // No stale -wal/-shm may remain next to the (future) fresh database.
    for suffix in ["-wal", "-shm"] {
        let mut name = state.db.as_os_str().to_owned();
        name.push(suffix);
        assert!(
            !PathBuf::from(name).exists(),
            "sidecar {suffix} must not survive quarantine"
        );
    }

    // A fresh open on the cleared path succeeds and is healthy.
    create_database(&state);
    assert_eq!(
        probe_database(&state.db).expect("probe"),
        ProbeOutcome::Healthy
    );
}

#[test]
fn repeated_quarantines_land_in_distinct_directories() {
    let state = StateDir::new();
    create_database(&state);
    corrupt_header(&state);
    let first = quarantine_if_corrupt(&state.db)
        .expect("first quarantine")
        .expect("corrupt");

    create_database(&state);
    corrupt_header(&state);
    let second = quarantine_if_corrupt(&state.db)
        .expect("second quarantine")
        .expect("corrupt");

    assert_ne!(
        first.quarantine_dir, second.quarantine_dir,
        "each quarantine must get its own directory"
    );
}
