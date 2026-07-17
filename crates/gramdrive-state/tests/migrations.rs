//! The migration framework through the public surface: what a host and
//! reconciliation actually touch (TASK-260715-18l9xz; SYNC-072, SYNC-071,
//! NFR-041).
//!
//! The runner's own mechanics — chunking, checkpoints, interruption, resume
//! — are unit-tested next to the code in `src/migrate.rs`, where the
//! internal registry is reachable. What is here is the contract everything
//! outside the crate depends on: a v1 file opens, a journal appears on a
//! file that predates the journal, a file from the future is refused without
//! being written to, and repair markers survive and read back.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions and
// `#[cfg(test)]` modules, and the helpers below are neither: they sit at
// module level in an integration-test binary. The rationale still applies in
// full — this file links into no product artifact — so the exemption is
// restated here, matching the established test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use gramdrive_state::{RepairKind, SCHEMA_VERSION, StateError, StateStore};
use rusqlite::Connection;

/// Representative rows of a v1 database — the fixture a v2 migration will be
/// written against. See `fixtures/v1_seed.sql`.
const V1_SEED_SQL: &str = include_str!("../fixtures/v1_seed.sql");

/// A unique database path under the OS temp directory, cleaned by
/// [`TempDb::drop`]. Uniqueness comes from the process id and a counter —
/// no clock, no randomness, so parallel test binaries cannot collide.
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gramdrive-migrations-test-{}-{n}.sqlite3",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }

    /// A v1 database with the fixture rows in it.
    fn seeded(&self) -> StateStore {
        let store = StateStore::open(&self.path).expect("create v1");
        store
            .connection()
            .execute_batch(V1_SEED_SQL)
            .expect("v1 seed rows");
        store
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

fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .expect("sqlite_schema query")
        == 1
}

#[test]
fn the_v1_fixture_opens_at_the_current_version_with_its_rows_intact() {
    let db = TempDb::new();
    let store = db.seeded();
    drop(store);

    // The path a user's existing file takes: nothing to migrate, so nothing
    // is migrated, and nothing is disturbed.
    let store = StateStore::open(&db.path).expect("reopen the fixture");
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);

    let messages: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
        .expect("count");
    assert_eq!(messages, 12);

    let history: Vec<i64> = store
        .connection()
        .prepare("SELECT version FROM schema_history ORDER BY version")
        .expect("prepare")
        .query_map([], |row| row.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        history,
        vec![SCHEMA_VERSION],
        "opening a current file must not record a second application"
    );

    assert!(
        store.repair_markers().expect("markers").is_empty(),
        "a healthy file owes nobody a repair"
    );
    assert!(
        store
            .connection()
            .query_row("SELECT count(*) FROM migration_progress", [], |row| row
                .get::<_, i64>(0))
            .expect("progress count")
            == 0,
        "a current file has no migration in flight"
    );
}

#[test]
fn a_file_written_before_the_journal_existed_gets_one() {
    let db = TempDb::new();
    let store = db.seeded();

    // Exactly what a database created by the build before this task looks
    // like: a valid v1 schema with no runner bookkeeping in it. The journal
    // cannot be introduced by a migration, because the runner needs the
    // journal to run one — so an open has to be able to add it.
    store
        .connection()
        .execute_batch("DROP TABLE migration_progress; DROP TABLE repair_markers;")
        .expect("drop the journal");
    drop(store);

    let store = StateStore::open(&db.path).expect("reopen a journal-less v1 file");

    assert!(table_exists(store.connection(), "migration_progress"));
    assert!(table_exists(store.connection(), "repair_markers"));
    assert_eq!(store.schema_version().expect("version"), SCHEMA_VERSION);
    let messages: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
        .expect("count");
    assert_eq!(messages, 12, "adding the journal must not touch user data");
}

#[test]
fn a_file_from_a_newer_build_is_refused_before_anything_is_written_to_it() {
    let db = TempDb::new();
    let store = db.seeded();
    store
        .connection()
        .execute_batch("DROP TABLE migration_progress; DROP TABLE repair_markers;")
        .expect("drop the journal");
    store
        .connection()
        .pragma_update(None, "user_version", SCHEMA_VERSION + 3)
        .expect("bump version");
    drop(store);

    match StateStore::open(&db.path) {
        Err(StateError::UnsupportedSchemaVersion { found, supported }) => {
            assert_eq!(found, SCHEMA_VERSION + 3);
            assert_eq!(supported, SCHEMA_VERSION);
        }
        other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
    }

    // The point of the check is that it happens *first*. If the refusal came
    // after the journal bootstrap, this build would have written to a file
    // whose schema it admits it does not understand.
    let conn = Connection::open(&db.path).expect("inspect");
    assert!(
        !table_exists(&conn, "migration_progress"),
        "a refused file must not be written to at all"
    );
    assert!(!table_exists(&conn, "repair_markers"));
}

#[test]
fn repair_markers_round_trip_and_survive_reopen() {
    let db = TempDb::new();
    let store = db.seeded();

    store
        .raise_repair_marker(RepairKind::RebuildProjection, "items/account:7")
        .expect("raise");
    let raised_at = {
        let markers = store.repair_markers().expect("markers");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].kind, RepairKind::RebuildProjection);
        assert_eq!(markers[0].detail, "items/account:7");
        markers[0].raised_at_ms
    };

    // Raising the same marker again is the same marker: reconciliation that
    // notices the same damage on every pass must not grow the list.
    store
        .raise_repair_marker(RepairKind::RebuildProjection, "items/account:7")
        .expect("raise again");
    let markers = store.repair_markers().expect("markers");
    assert_eq!(markers.len(), 1);
    assert_eq!(
        markers[0].raised_at_ms, raised_at,
        "a re-raise keeps the moment the problem started"
    );

    // A different detail is a different marker.
    store
        .raise_repair_marker(RepairKind::RebuildProjection, "items/account:9")
        .expect("raise other");
    assert_eq!(store.repair_markers().expect("markers").len(), 2);
    drop(store);

    let store = StateStore::open(&db.path).expect("reopen");
    assert_eq!(
        store.repair_markers().expect("markers").len(),
        2,
        "markers are durable: the repair is still owed after a restart"
    );

    store
        .clear_repair_marker(RepairKind::RebuildProjection, "items/account:7")
        .expect("clear");
    let markers = store.repair_markers().expect("markers");
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0].detail, "items/account:9");

    // Clearing what is not raised is not a failure.
    store
        .clear_repair_marker(RepairKind::MigrationInterrupted, "nothing like this")
        .expect("clear absent");
    assert_eq!(store.repair_markers().expect("markers").len(), 1);
}

#[test]
fn a_repair_marker_from_a_newer_build_is_reported_not_ignored() {
    let db = TempDb::new();
    let store = db.seeded();

    // A kind this build has never heard of, as a newer one would leave it.
    store
        .connection()
        .execute(
            "INSERT INTO repair_markers (kind, detail, raised_at_ms)
             VALUES ('rebuild_the_flux_capacitor', 'chat:100', 1704067200000)",
            [],
        )
        .expect("insert unknown kind");

    match store.repair_markers() {
        Err(StateError::UnknownRepairKind { kind }) => {
            assert_eq!(kind, "rebuild_the_flux_capacitor");
        }
        other => panic!("expected UnknownRepairKind, got {other:?}"),
    }
}

#[test]
fn error_messages_name_the_condition() {
    let err = StateError::MigrationFailed {
        version: 4,
        name: "backfill_render_hints",
        source: Box::new(StateError::MigrationStalled {
            checkpoint: "seq:9000".to_owned(),
        }),
    };
    assert_eq!(
        err.to_string(),
        "migration to version 4 (backfill_render_hints) failed: resumable migration reported \
         progress without moving its checkpoint ('seq:9000')"
    );

    let err = StateError::UnknownRepairKind {
        kind: "whatever".to_owned(),
    };
    assert_eq!(
        err.to_string(),
        "unknown repair marker kind 'whatever' in the database"
    );
}
