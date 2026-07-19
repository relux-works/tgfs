//! Opening, versioning, and journal behavior of [`StateStore`]
//! (TASK-260715-1ceq7h; NFR-041, SYNC-072).

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions and
// `#[cfg(test)]` modules, and the `TempDb` helper is neither: it sits at
// module level in an integration-test binary. The rationale still applies in
// full — this file links into no product artifact — so the exemption is
// restated here, matching the established test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use gramdrive_state::{SCHEMA_VERSION, StateError, StateStore};

/// A unique database path under the OS temp directory, cleaned by
/// [`TempDb::drop`]. Uniqueness comes from the process id and a counter —
/// no clock, no randomness, so parallel test binaries cannot collide with
/// themselves or each other.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gramdrive-state-test-{}-{n}.sqlite3",
            std::process::id()
        ));
        // A leftover from a killed earlier run would corrupt the "fresh
        // file" premise; remove it and its WAL siblings up front.
        let _ = std::fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = self.path.as_os_str().to_owned();
            name.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(name));
        }
    }
}

#[test]
fn fresh_memory_database_reaches_schema_version() {
    let store = StateStore::open_in_memory().expect("open");
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);

    let fk: i64 = store
        .connection()
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .expect("pragma");
    assert_eq!(fk, 1, "foreign keys must be enforced on every connection");
}

#[test]
fn schema_history_records_the_applied_version() {
    let store = StateStore::open_in_memory().expect("open");
    let history: Vec<(i64, i64)> = store
        .connection()
        .prepare("SELECT version, applied_at_ms FROM schema_history ORDER BY version")
        .expect("prepare")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        history
            .iter()
            .map(|(version, _)| *version)
            .collect::<Vec<_>>(),
        (1..=SCHEMA_VERSION).collect::<Vec<_>>(),
        "every version from the baseline to the current one is recorded"
    );
    assert!(
        history.iter().all(|(_, applied_at_ms)| *applied_at_ms > 0),
        "applied_at_ms must be a real timestamp"
    );
}

#[test]
fn file_database_uses_wal_and_reopens_idempotently() {
    let db = TempDb::new();

    let store = StateStore::open(&db.path).expect("first open");
    let mode: String = store
        .connection()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("journal_mode");
    assert_eq!(mode, "wal");
    drop(store);

    // Second open finds user_version already current and touches nothing.
    let store = StateStore::open(&db.path).expect("reopen");
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    let history_rows: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM schema_history", [], |r| r.get(0))
        .expect("count");
    assert_eq!(
        history_rows, SCHEMA_VERSION,
        "reopen must not re-apply the schema: one row per applied version, nothing more"
    );
}

#[test]
fn database_from_a_newer_build_is_refused() {
    let db = TempDb::new();

    let store = StateStore::open(&db.path).expect("create");
    store
        .connection()
        .pragma_update(None, "user_version", SCHEMA_VERSION + 7)
        .expect("bump version");
    drop(store);

    match StateStore::open(&db.path) {
        Err(StateError::UnsupportedSchemaVersion { found, supported }) => {
            assert_eq!(found, SCHEMA_VERSION + 7);
            assert_eq!(supported, SCHEMA_VERSION);
        }
        other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
    }
}

#[test]
fn torn_file_with_tables_but_version_zero_is_reported_not_repaired() {
    let db = TempDb::new();

    let store = StateStore::open(&db.path).expect("create");
    // Simulate a file that has schema objects but lost its version stamp —
    // not something this code produces (the stamp and DDL commit together);
    // the store must fail the re-apply loudly instead of guessing.
    store
        .connection()
        .pragma_update(None, "user_version", 0)
        .expect("clear version");
    drop(store);

    match StateStore::open(&db.path) {
        Err(StateError::Sqlite(_)) => {}
        other => panic!("expected an sqlite error, got {other:?}"),
    }
}

#[test]
fn error_messages_name_the_condition() {
    let err = StateError::UnsupportedSchemaVersion {
        found: 9,
        supported: 1,
    };
    assert_eq!(
        err.to_string(),
        "database schema version 9 is newer than the supported version 1"
    );
    let err = StateError::MigrationRequired {
        found: 1,
        supported: 2,
    };
    assert_eq!(
        err.to_string(),
        "database schema version 1 requires migration to version 2"
    );
    let err = StateError::WalUnavailable {
        mode: "delete".to_owned(),
    };
    assert_eq!(
        err.to_string(),
        "database refused WAL journal mode (got 'delete')"
    );
}
