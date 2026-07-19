//! The forward-only migration runner (TASK-260715-18l9xz; SYNC-072,
//! NFR-013, NFR-041).
//!
//! [`crate::schema`] creates a fresh database from the frozen baseline
//! script. Every version after that is a [`Migration`] in [`MIGRATIONS`],
//! applied in order. Forward-only, and that is a product decision, not a
//! missing feature: a downgrade would have to guess what a newer schema's
//! data means in an older shape, and the honest answer for a cache of
//! re-derivable state is to restore a backup or re-sync. An older build
//! meeting a newer file refuses it ([`StateError::UnsupportedSchemaVersion`])
//! rather than improvising (NFR-013).
//!
//! # Why a crash cannot corrupt a file
//!
//! `PRAGMA user_version` is part of the database header, so it commits with
//! the transaction that sets it. The runner never advances it except in the
//! same transaction as the work that earns it. That single rule is what
//! makes every interruption survivable:
//!
//! * [`MigrationStep::Sql`] is one transaction. A crash rolls it back
//!   whole; the next open sees the old version and starts it over.
//! * [`MigrationStep::Resumable`] cannot fit in one transaction — rewriting
//!   a column across 100k rows would hold a write lock for the duration and
//!   lose everything to one crash at the end. So it commits in chunks, and
//!   each chunk commits its data changes *together with* the checkpoint it
//!   resumes from. A crash leaves the old version and the last committed
//!   checkpoint; the next open hands that checkpoint back to the same chunk
//!   function and it continues.
//!
//! Idempotent resume is therefore a contract between two halves. The runner
//! guarantees a chunk is only ever re-called with a checkpoint it actually
//! committed (never a later one, never a partial one). The chunk function
//! guarantees the work after a given checkpoint is repeatable — which for
//! the usual "process rows after this key" shape it already is.
//!
//! # Writing a migration
//!
//! Add a [`Migration`] to [`MIGRATIONS`] and bump [`crate::SCHEMA_VERSION`]:
//! a const assertion below rejects the build if you do one without the
//! other. Then add `fixtures/v{previous}_seed.sql` — a unit test in this
//! module fails until every migration has a fixture database of the schema
//! it migrates *from*, because a migration tested only against a database
//! this build created has never met the schema it exists for.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::error::StateError;
use crate::repair::{self, RepairKind};
use crate::schema::SCHEMA_VERSION;

/// The version the frozen baseline script (`schema/v1.sql`) creates.
///
/// Every migration in [`MIGRATIONS`] runs *after* this: version 1 is not
/// migrated to, it is created.
pub const BASELINE_VERSION: i64 = 1;

/// The runner's bookkeeping tables — see `schema/journal.sql` for why they
/// are not part of the numbered schema.
const JOURNAL_SQL: &str = include_str!("schema/journal.sql");

/// Every migration this build carries, in application order.
pub(crate) const MIGRATIONS: &[Migration] = &[
    // v2 — the provider-visible item change journal (TASK-260715-rhcnhc):
    // pure DDL plus one seed row, so it fits one transaction. No backfill,
    // deliberately: items that predate the journal have no changes to
    // report — a provider without an anchor performs a full enumeration
    // anyway and takes the journal's current sequence as its first anchor.
    Migration {
        version: 2,
        name: "item_change_journal",
        step: MigrationStep::Sql(include_str!("schema/v2.sql")),
    },
];

/// [`SCHEMA_VERSION`] and [`MIGRATIONS`] are one fact stated twice, so the
/// build refuses to link them out of agreement. A migration added without a
/// version bump would never run; a version bump without a migration would
/// leave every existing file rejected as needing a migration that does not
/// exist. Both are caught here rather than by a user's database.
const _: () = {
    assert!(
        MIGRATIONS.len() == (SCHEMA_VERSION - BASELINE_VERSION) as usize,
        "SCHEMA_VERSION must equal BASELINE_VERSION + MIGRATIONS.len(): \
         adding a migration means bumping the version, and vice versa"
    );
    let mut index = 0;
    while index < MIGRATIONS.len() {
        assert!(
            MIGRATIONS[index].version == BASELINE_VERSION + 1 + index as i64,
            "MIGRATIONS must be contiguous and ascending from BASELINE_VERSION + 1"
        );
        index += 1;
    }
};

/// One forward step: a database at `version - 1` becomes a database at
/// `version`.
#[derive(Debug)]
pub struct Migration {
    /// The version this step produces. The runner applies it only to a
    /// database at `version - 1`.
    pub version: i64,
    /// A stable name for diagnostics and the journal. Never parsed — it
    /// exists so a failure names the step in a report a user can send.
    pub name: &'static str,
    /// How the step runs.
    pub step: MigrationStep,
}

/// How a [`Migration`] does its work.
///
/// The choice is about transaction size, not about how complicated the
/// migration is: everything that fits in one transaction should be
/// [`MigrationStep::Sql`], because rollback is simpler than resume.
#[derive(Debug)]
pub enum MigrationStep {
    /// One SQL script, one transaction. DDL, and data work small and bounded
    /// enough to hold a write lock for. All-or-nothing: an interruption
    /// rolls it back and the next open starts over.
    Sql(&'static str),
    /// Chunked work with a durable checkpoint between chunks, for data too
    /// large for one transaction (SYNC-072).
    Resumable {
        /// DDL the chunks need before they can run — typically the
        /// `ALTER TABLE` whose new column the chunks fill.
        ///
        /// Runs only when there is no checkpoint yet, in the same
        /// transaction as the first chunk's commit. So it is never applied
        /// twice: either that transaction commits, and every later resume
        /// finds a checkpoint and skips this, or it rolls back and the
        /// resumed run applies it again from a clean slate.
        prepare: Option<&'static str>,
        /// The work itself, called in a loop until it reports
        /// [`ChunkOutcome::Done`].
        chunk: ChunkFn,
    },
}

/// One chunk of a [`MigrationStep::Resumable`].
///
/// Receives the transaction its writes must go through, and the checkpoint
/// the last committed chunk returned — `None` on the first chunk of a fresh
/// run. Everything the chunk writes commits atomically with the checkpoint
/// it returns, and after a crash it is called again with exactly the last
/// checkpoint that committed, so the work following any checkpoint it ever
/// produces must be repeatable.
///
/// A chunk must be sized to finish well inside the busy timeout other
/// processes are waiting out; it holds a write lock for its duration.
///
/// It must also eventually return [`ChunkOutcome::Done`], and that is the
/// migration's obligation, not something the runner can check for it. The
/// runner catches the one non-termination it can recognize locally — a chunk
/// handing back the checkpoint it was given ([`StateError::MigrationStalled`])
/// — but a chunk that returns a *fresh* checkpoint forever is
/// indistinguishable from a long migration making progress, and the runner
/// will keep calling it. Any bound the runner could impose would be a guess
/// at what "too many chunks" means for a migration it has never seen. The
/// fixture and interruption tests every migration ships are where that bug
/// is supposed to die.
pub type ChunkFn = fn(&Transaction<'_>, Option<&str>) -> Result<ChunkOutcome, StateError>;

/// What a chunk did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkOutcome {
    /// Work remains, and the next chunk resumes from `checkpoint`.
    More {
        /// Opaque to the runner — only the migration that wrote it reads it.
        /// It must differ from the checkpoint the chunk was given: an
        /// unchanged checkpoint is a chunk that reported progress it did not
        /// make, and the runner stops with [`StateError::MigrationStalled`]
        /// rather than spin on it forever.
        checkpoint: String,
    },
    /// Nothing remains. The runner stamps the new version and clears the
    /// checkpoint in one transaction.
    Done,
}

/// Creates the runner's journal on a database that does not have one.
///
/// Idempotent and version-independent by design: a file written by a build
/// older than the runner has no journal, and the runner needs one to migrate
/// it. Call only once the version is known to be one this build may write to
/// — this is the first write to the file.
pub(crate) fn ensure_journal(conn: &Connection) -> Result<(), StateError> {
    conn.execute_batch(JOURNAL_SQL)?;
    Ok(())
}

/// Applies `migrations` until the database reaches `target`.
///
/// A database already at `target` is not touched. A database below it with
/// no migration for the next step is refused with
/// [`StateError::MigrationRequired`] rather than left silently behind — with
/// the const assertion above holding that is unreachable in a shipped build,
/// which is exactly why it must stay a typed error and not an assumption.
///
/// Assumes the version has already been checked against `target`
/// ([`crate::schema::ensure_schema`] does that) and that the journal exists.
pub(crate) fn run(
    conn: &mut Connection,
    migrations: &[Migration],
    target: i64,
) -> Result<(), StateError> {
    let mut current = current_version(conn)?;
    while current < target {
        let next = current + 1;
        let migration = migrations
            .iter()
            .find(|candidate| candidate.version == next)
            .ok_or(StateError::MigrationRequired {
                found: current,
                supported: target,
            })?;
        apply(conn, migration).map_err(|source| StateError::MigrationFailed {
            version: migration.version,
            name: migration.name,
            source: Box::new(source),
        })?;
        current = migration.version;
    }
    Ok(())
}

fn apply(conn: &mut Connection, migration: &Migration) -> Result<(), StateError> {
    match migration.step {
        MigrationStep::Sql(sql) => {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            finish(&tx, migration)?;
            tx.commit()?;
            Ok(())
        }
        MigrationStep::Resumable { prepare, chunk } => {
            apply_resumable(conn, migration, prepare, chunk)
        }
    }
}

/// The chunk loop. Every path out of it leaves the database consistent with
/// its version: either the version is old and the checkpoint says where to
/// resume, or the version is new and there is no checkpoint.
fn apply_resumable(
    conn: &mut Connection,
    migration: &Migration,
    prepare: Option<&'static str>,
    chunk: ChunkFn,
) -> Result<(), StateError> {
    loop {
        let tx = conn.transaction()?;
        let checkpoint = read_checkpoint(&tx, migration.version)?;
        if checkpoint.is_none() {
            // No committed chunk yet, so the preamble either has never run
            // or was rolled back with the chunk that would have committed it.
            if let Some(sql) = prepare {
                tx.execute_batch(sql)?;
            }
        }

        match chunk(&tx, checkpoint.as_deref())? {
            ChunkOutcome::More { checkpoint: next } => {
                if checkpoint.as_deref() == Some(next.as_str()) {
                    return Err(StateError::MigrationStalled { checkpoint: next });
                }
                save_checkpoint(&tx, migration, &next)?;
                // Raised with the first checkpoint that commits and cleared
                // by the transaction that finishes the migration: the marker
                // is durable for exactly as long as an unfinished tail is.
                repair::raise(
                    &tx,
                    RepairKind::MigrationInterrupted,
                    &interrupted(migration),
                )?;
                tx.commit()?;
            }
            ChunkOutcome::Done => {
                clear_checkpoint(&tx, migration.version)?;
                repair::clear(
                    &tx,
                    RepairKind::MigrationInterrupted,
                    &interrupted(migration),
                )?;
                finish(&tx, migration)?;
                tx.commit()?;
                return Ok(());
            }
        }
    }
}

/// Records the migration and stamps the version. Called inside the
/// transaction that carries the migration's last piece of work — the stamp
/// and the work it describes commit together or not at all.
fn finish(tx: &Transaction<'_>, migration: &Migration) -> Result<(), StateError> {
    tx.execute(
        "INSERT INTO schema_history (version, applied_at_ms) VALUES (?1, unixepoch() * 1000)",
        [migration.version],
    )?;
    tx.pragma_update(None, "user_version", migration.version)?;
    Ok(())
}

/// The `detail` half of a migration's [`RepairKind::MigrationInterrupted`]
/// marker identity. Stable per migration, so a resume re-raises the same
/// marker instead of a second one.
fn interrupted(migration: &Migration) -> String {
    format!("migration {} ({})", migration.version, migration.name)
}

fn current_version(conn: &Connection) -> Result<i64, StateError> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn read_checkpoint(tx: &Transaction<'_>, version: i64) -> Result<Option<String>, StateError> {
    Ok(tx
        .query_row(
            "SELECT checkpoint FROM migration_progress WHERE version = ?1",
            [version],
            |row| row.get(0),
        )
        .optional()?)
}

fn save_checkpoint(
    tx: &Transaction<'_>,
    migration: &Migration,
    checkpoint: &str,
) -> Result<(), StateError> {
    tx.execute(
        "INSERT INTO migration_progress
             (version, name, checkpoint, chunks_done, started_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, 1, unixepoch() * 1000, unixepoch() * 1000)
         ON CONFLICT (version) DO UPDATE SET
             checkpoint    = excluded.checkpoint,
             chunks_done   = chunks_done + 1,
             updated_at_ms = excluded.updated_at_ms",
        params![migration.version, migration.name, checkpoint],
    )?;
    Ok(())
}

fn clear_checkpoint(tx: &Transaction<'_>, version: i64) -> Result<(), StateError> {
    tx.execute(
        "DELETE FROM migration_progress WHERE version = ?1",
        [version],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The runner is exercised against a real v1 fixture database with a
    //! migration that does the thing this framework exists for: a schema
    //! change plus a data backfill too big for one transaction.
    //!
    //! Most migrations here are test-only, targeting the runner's own
    //! mechanics (chunking, checkpoints, interruption, resume) with a
    //! resumable shape the shipped registry does not have yet. The shipped
    //! [`MIGRATIONS`] are applied to the same v1 fixture in their own test
    //! below.

    use std::cell::Cell;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use rusqlite::Connection;

    use super::*;

    /// Representative rows of a v1 database — see the file for what is in it
    /// and why.
    const V1_SEED_SQL: &str = include_str!("../fixtures/v1_seed.sql");

    /// Messages in the fixture. The chunk size below divides it into several
    /// chunks: a "resumable" migration that resumes exactly once proves less
    /// than one that can be interrupted in the middle of a run.
    const FIXTURE_MESSAGES: usize = 12;

    /// Rows one chunk of [`fill_render_hint`] handles.
    const CHUNK_ROWS: usize = 4;

    /// The version the test migrations produce.
    const V2: i64 = 2;

    thread_local! {
        /// How many chunks to let through before injecting a failure, or
        /// `None` to let the migration finish.
        static FAIL_AFTER_CHUNKS: Cell<Option<u32>> = const { Cell::new(None) };
        /// Chunks [`fill_render_hint`] has committed in this test.
        static CHUNKS_RUN: Cell<u32> = const { Cell::new(0) };
    }

    fn arm_failure_after(chunks: u32) {
        FAIL_AFTER_CHUNKS.with(|cell| cell.set(Some(chunks)));
        CHUNKS_RUN.with(|cell| cell.set(0));
    }

    fn disarm_failure() {
        FAIL_AFTER_CHUNKS.with(|cell| cell.set(None));
    }

    /// A realistic v2: add a column to the `messages` projection and fill it
    /// for every existing row. The fill cannot be one transaction on a real
    /// account (110k messages), so it is chunked, and the `ALTER TABLE` its
    /// chunks depend on is the `prepare` preamble.
    const RENDER_HINT: &[Migration] = &[Migration {
        version: V2,
        name: "messages_render_hint",
        step: MigrationStep::Resumable {
            prepare: Some("ALTER TABLE messages ADD COLUMN render_hint TEXT"),
            chunk: fill_render_hint,
        },
    }];

    /// The same version done as one transaction — the shape every migration
    /// small enough to fit should use.
    const RENDER_HINT_ATOMIC: &[Migration] = &[Migration {
        version: V2,
        name: "messages_render_hint_atomic",
        step: MigrationStep::Sql(
            "ALTER TABLE messages ADD COLUMN render_hint TEXT;
             UPDATE messages SET render_hint = 'hint-' || chat_id || '-' || message_id;",
        ),
    }];

    /// Fills `render_hint` for [`CHUNK_ROWS`] messages per chunk, resuming
    /// after the last `latest_event_seq` it committed.
    ///
    /// `latest_event_seq` is the cursor because it is unique per message and
    /// indexed — a chunked migration whose cursor needs a table scan to
    /// resume has only moved the cost around.
    fn fill_render_hint(
        tx: &Transaction<'_>,
        checkpoint: Option<&str>,
    ) -> Result<ChunkOutcome, StateError> {
        let after: i64 = checkpoint.map_or(0, |text| {
            text.parse()
                .expect("the runner returns its own checkpoints")
        });

        if let Some(limit) = FAIL_AFTER_CHUNKS.with(Cell::get)
            && CHUNKS_RUN.with(Cell::get) >= limit
        {
            // A genuine database error inside a chunk. The durable state it
            // leaves is the state a process kill would leave: committed
            // chunks on disk, this transaction rolled back.
            tx.execute_batch("SELECT 1 FROM a_table_that_does_not_exist")?;
        }

        let mut statement = tx.prepare(
            "SELECT chat_id, message_id, latest_event_seq FROM messages
             WHERE latest_event_seq > ?1
             ORDER BY latest_event_seq
             LIMIT ?2",
        )?;
        let rows: Vec<(i64, i64, i64)> = statement
            .query_map(params![after, CHUNK_ROWS as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<_, _>>()?;

        let Some(&(_, _, last_seq)) = rows.last() else {
            return Ok(ChunkOutcome::Done);
        };

        for (chat_id, message_id, _) in &rows {
            tx.execute(
                "UPDATE messages SET render_hint = 'hint-' || chat_id || '-' || message_id
                 WHERE account_id = 7 AND namespace_version = 1
                   AND chat_id = ?1 AND message_id = ?2",
                params![chat_id, message_id],
            )?;
        }

        CHUNKS_RUN.with(|cell| cell.set(cell.get() + 1));
        Ok(ChunkOutcome::More {
            checkpoint: last_seq.to_string(),
        })
    }

    /// A chunk that always asks to be called again with what it was given.
    fn never_progresses(
        _tx: &Transaction<'_>,
        checkpoint: Option<&str>,
    ) -> Result<ChunkOutcome, StateError> {
        Ok(ChunkOutcome::More {
            checkpoint: checkpoint.unwrap_or("stuck").to_owned(),
        })
    }

    const STALLING: &[Migration] = &[Migration {
        version: V2,
        name: "stalls_forever",
        step: MigrationStep::Resumable {
            prepare: None,
            chunk: never_progresses,
        },
    }];

    /// A unique database path under the OS temp directory, cleaned on drop.
    /// Uniqueness from process id and a counter — no clock, no randomness.
    struct TempDb {
        path: PathBuf,
    }

    impl TempDb {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "gramdrive-migrate-test-{}-{n}.sqlite3",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            Self { path }
        }

        /// Opens the file as a v1 fixture database: baseline schema, journal,
        /// representative rows.
        fn open_v1(&self) -> Connection {
            let mut conn = Connection::open(&self.path).expect("open");
            seed_v1(&mut conn);
            conn
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

    /// Brings `conn` to the v1 fixture state: the frozen baseline plus the
    /// runner's journal — exactly what a database created by the v1 build
    /// looks like — then the seed rows, with foreign keys on so the fixture
    /// cannot claim rows the schema would reject. Deliberately *not*
    /// `ensure_schema`, which would migrate the fixture past the version
    /// these tests exist to start from.
    fn seed_v1(conn: &mut Connection) {
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        crate::schema::apply_baseline(conn).expect("baseline schema");
        ensure_journal(conn).expect("journal");
        conn.execute_batch(V1_SEED_SQL).expect("v1 seed rows");
    }

    fn memory_v1() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in memory");
        seed_v1(&mut conn);
        conn
    }

    fn version_of(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version")
    }

    /// Every message and the hint the migration gave it, in a stable order.
    fn render_hints(conn: &Connection) -> Vec<(i64, i64, Option<String>)> {
        let mut statement = conn
            .prepare(
                "SELECT chat_id, message_id, render_hint FROM messages
                 ORDER BY chat_id, message_id",
            )
            .expect("prepare");
        let rows = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query");
        rows.collect::<Result<_, _>>().expect("rows")
    }

    fn checkpoint_row(conn: &Connection) -> Option<(String, i64)> {
        conn.query_row(
            "SELECT checkpoint, chunks_done FROM migration_progress WHERE version = ?1",
            [V2],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .expect("checkpoint query")
    }

    fn marker_details(conn: &Connection) -> Vec<String> {
        repair::list(conn)
            .expect("markers")
            .into_iter()
            .filter(|marker| marker.kind == RepairKind::MigrationInterrupted)
            .map(|marker| marker.detail)
            .collect()
    }

    // --- The registry contract -------------------------------------------

    #[test]
    fn shipped_registry_agrees_with_the_schema_version() {
        // The const assertion above is the real gate — this fails the same
        // way at runtime, and names what the compile error means.
        assert_eq!(
            MIGRATIONS.len() as i64,
            SCHEMA_VERSION - BASELINE_VERSION,
            "every version above the baseline needs exactly one migration"
        );
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                migration.version,
                BASELINE_VERSION + 1 + index as i64,
                "migrations must be contiguous and ascending"
            );
        }
    }

    #[test]
    fn every_migration_ships_a_fixture_of_the_schema_it_migrates_from() {
        // The AC this framework is built around. Vacuous today (no
        // migrations), and deliberately so: it fails the moment someone adds
        // a migration without the fixture database that proves it against
        // the schema it will actually meet in the field.
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        for migration in MIGRATIONS {
            let prior = migration.version - 1;
            let seed = fixtures.join(format!("v{prior}_seed.sql"));
            assert!(
                seed.is_file(),
                "migration {} ({}) has no fixture for the schema it migrates from: \
                 expected {}",
                migration.version,
                migration.name,
                seed.display()
            );
        }
    }

    #[test]
    fn the_v1_fixture_is_a_real_v1_database() {
        let conn = memory_v1();
        assert_eq!(version_of(&conn), BASELINE_VERSION);

        let messages: usize = conn
            .query_row("SELECT count(*) FROM messages", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| count as usize)
            .expect("count");
        assert_eq!(messages, FIXTURE_MESSAGES);

        // Foreign keys were on while it loaded, so this is not a fixture
        // that only looks like a v1 database.
        let violations: usize = conn
            .prepare("PRAGMA foreign_key_check")
            .expect("prepare")
            .query_map([], |_| Ok(()))
            .expect("query")
            .count();
        assert_eq!(violations, 0);
    }

    // --- The shipped registry against the fixture --------------------------

    #[test]
    fn the_shipped_v2_migration_creates_the_item_change_journal() {
        let mut conn = memory_v1();

        run(&mut conn, MIGRATIONS, SCHEMA_VERSION).expect("migrate the v1 fixture");

        assert_eq!(version_of(&conn), SCHEMA_VERSION);
        let instance: String = conn
            .query_row(
                "SELECT instance_id FROM item_change_journal WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .expect("the journal identity row");
        assert_eq!(
            instance.len(),
            32,
            "a 16-byte random identity in lowercase hex"
        );
        let changes: i64 = conn
            .query_row("SELECT count(*) FROM item_changes", [], |row| row.get(0))
            .expect("count");
        assert_eq!(
            changes, 0,
            "no backfill: items that predate the journal have no changes to report"
        );
    }

    // --- Applying a migration ---------------------------------------------

    #[test]
    fn atomic_migration_advances_the_version_and_records_history() {
        let mut conn = memory_v1();

        run(&mut conn, RENDER_HINT_ATOMIC, V2).expect("migrate");

        assert_eq!(version_of(&conn), V2);
        let history: Vec<i64> = conn
            .prepare("SELECT version FROM schema_history ORDER BY version")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(history, vec![BASELINE_VERSION, V2]);
        assert!(
            render_hints(&conn)
                .iter()
                .all(|(_, _, hint)| hint.is_some()),
            "every row should carry a hint"
        );
        assert_eq!(
            checkpoint_row(&conn),
            None,
            "an atomic migration leaves no checkpoint"
        );
    }

    #[test]
    fn a_database_already_at_the_target_is_untouched() {
        let mut conn = memory_v1();
        run(&mut conn, RENDER_HINT_ATOMIC, V2).expect("migrate");
        let before = render_hints(&conn);

        run(&mut conn, RENDER_HINT_ATOMIC, V2).expect("second run");

        assert_eq!(version_of(&conn), V2);
        assert_eq!(render_hints(&conn), before);
        let applications: i64 = conn
            .query_row(
                "SELECT count(*) FROM schema_history WHERE version = ?1",
                [V2],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(applications, 1, "a migration must not be applied twice");
    }

    #[test]
    fn resumable_migration_checkpoints_each_chunk_and_finishes() {
        disarm_failure();
        let mut conn = memory_v1();

        run(&mut conn, RENDER_HINT, V2).expect("migrate");

        assert_eq!(version_of(&conn), V2);
        assert_eq!(
            checkpoint_row(&conn),
            None,
            "a finished migration clears its checkpoint"
        );
        assert_eq!(
            marker_details(&conn),
            Vec::<String>::new(),
            "a finished migration clears its interruption marker"
        );
        assert_eq!(
            CHUNKS_RUN.with(Cell::get) as usize,
            FIXTURE_MESSAGES.div_ceil(CHUNK_ROWS),
            "the fixture should take more than one chunk, or this proves nothing"
        );
        for (chat, message, hint) in render_hints(&conn) {
            assert_eq!(
                hint.as_deref(),
                Some(format!("hint-{chat}-{message}").as_str())
            );
        }
    }

    // --- Interruption and resume (SYNC-072) -------------------------------

    #[test]
    fn an_interrupted_migration_resumes_from_its_checkpoint() {
        let db = TempDb::new();
        let mut conn = db.open_v1();

        // Two chunks commit, the third hits a database error. Everything
        // after this drop is what a fresh process finds on disk.
        arm_failure_after(2);
        let error = run(&mut conn, RENDER_HINT, V2).expect_err("chunk three fails");
        assert!(
            matches!(error, StateError::MigrationFailed { version: V2, .. }),
            "expected a named migration failure, got {error:?}"
        );
        drop(conn);

        // A fresh connection to the file: the version never moved, and the
        // checkpoint says where the committed work stopped.
        let conn = Connection::open(&db.path).expect("reopen");
        assert_eq!(
            version_of(&conn),
            BASELINE_VERSION,
            "an unfinished migration must never advance the version"
        );
        let (checkpoint, chunks_done) = checkpoint_row(&conn).expect("a durable checkpoint");
        assert_eq!(chunks_done, 2);
        let done_before = render_hints(&conn)
            .into_iter()
            .filter(|(_, _, hint)| hint.is_some())
            .count();
        assert_eq!(
            done_before,
            2 * CHUNK_ROWS,
            "exactly the committed chunks survived"
        );
        drop(conn);

        // Resume: the same migration, handed the checkpoint it committed.
        disarm_failure();
        let mut conn = Connection::open(&db.path).expect("reopen");
        run(&mut conn, RENDER_HINT, V2).expect("resume");

        assert_eq!(version_of(&conn), V2);
        assert_eq!(checkpoint_row(&conn), None);
        assert!(
            checkpoint.parse::<i64>().is_ok(),
            "the checkpoint is the migration's own cursor"
        );
        for (chat, message, hint) in render_hints(&conn) {
            assert_eq!(
                hint.as_deref(),
                Some(format!("hint-{chat}-{message}").as_str())
            );
        }
    }

    #[test]
    fn resuming_produces_exactly_what_an_uninterrupted_run_produces() {
        // Idempotent resume, stated as the property that matters: an
        // interruption must not be observable in the result.
        disarm_failure();
        let mut clean = memory_v1();
        run(&mut clean, RENDER_HINT, V2).expect("clean run");
        let expected = render_hints(&clean);

        let db = TempDb::new();
        let mut conn = db.open_v1();
        arm_failure_after(1);
        run(&mut conn, RENDER_HINT, V2).expect_err("interrupted");
        drop(conn);

        // Interrupt the resume too: a migration that survives one crash but
        // not two has not proven anything about crashes.
        let mut conn = Connection::open(&db.path).expect("reopen");
        arm_failure_after(1);
        run(&mut conn, RENDER_HINT, V2).expect_err("interrupted again");
        drop(conn);

        disarm_failure();
        let mut conn = Connection::open(&db.path).expect("reopen");
        run(&mut conn, RENDER_HINT, V2).expect("resume to completion");

        assert_eq!(version_of(&conn), V2);
        assert_eq!(
            render_hints(&conn),
            expected,
            "twice-interrupted and never-interrupted must be indistinguishable"
        );
    }

    #[test]
    fn an_interrupted_migration_leaves_a_repair_marker_until_it_completes() {
        let db = TempDb::new();
        let mut conn = db.open_v1();

        arm_failure_after(2);
        run(&mut conn, RENDER_HINT, V2).expect_err("interrupted");
        drop(conn);

        let conn = Connection::open(&db.path).expect("reopen");
        assert_eq!(
            marker_details(&conn),
            vec!["migration 2 (messages_render_hint)".to_owned()],
            "an interrupted migration is durably recorded, naming itself"
        );
        let raised_at = repair::list(&conn).expect("markers")[0].raised_at_ms;
        drop(conn);

        // Resuming re-raises the same marker rather than a second one, and
        // keeps the timestamp of the interruption.
        disarm_failure();
        let mut conn = Connection::open(&db.path).expect("reopen");
        arm_failure_after(1);
        run(&mut conn, RENDER_HINT, V2).expect_err("interrupted again");
        let markers = repair::list(&conn).expect("markers");
        assert_eq!(markers.len(), 1, "one marker per migration, not per crash");
        assert_eq!(
            markers[0].raised_at_ms, raised_at,
            "the marker dates the interruption, not the last time it was noticed"
        );

        disarm_failure();
        run(&mut conn, RENDER_HINT, V2).expect("resume");
        assert_eq!(
            marker_details(&conn),
            Vec::<String>::new(),
            "completing the migration clears it"
        );
    }

    #[test]
    fn the_preamble_survives_a_rollback_and_is_never_applied_twice() {
        let db = TempDb::new();
        let mut conn = db.open_v1();

        // Fail inside the very first chunk: the ALTER TABLE ran in that
        // transaction and must roll back with it.
        arm_failure_after(0);
        run(&mut conn, RENDER_HINT, V2).expect_err("first chunk fails");
        drop(conn);

        let conn = Connection::open(&db.path).expect("reopen");
        assert!(
            !message_columns(&conn)
                .iter()
                .any(|name| name == "render_hint"),
            "a rolled-back preamble leaves no column behind"
        );
        assert_eq!(checkpoint_row(&conn), None);
        drop(conn);

        // The resumed run applies the preamble from scratch — and once the
        // first chunk commits, no later resume re-applies it (a second
        // ALTER TABLE would fail with 'duplicate column name').
        disarm_failure();
        let mut conn = Connection::open(&db.path).expect("reopen");
        arm_failure_after(1);
        run(&mut conn, RENDER_HINT, V2).expect_err("interrupted after the preamble committed");
        drop(conn);

        disarm_failure();
        let mut conn = Connection::open(&db.path).expect("reopen");
        run(&mut conn, RENDER_HINT, V2).expect("resume past the committed preamble");
        assert_eq!(version_of(&conn), V2);
    }

    fn message_columns(conn: &Connection) -> Vec<String> {
        conn.prepare("SELECT name FROM pragma_table_info('messages')")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    }

    // --- Refusals ---------------------------------------------------------

    #[test]
    fn a_chunk_that_does_not_move_its_checkpoint_is_refused() {
        let mut conn = memory_v1();

        let error = run(&mut conn, STALLING, V2).expect_err("stalled");

        match error {
            StateError::MigrationFailed {
                version, source, ..
            } => {
                assert_eq!(version, V2);
                assert!(
                    matches!(*source, StateError::MigrationStalled { ref checkpoint } if checkpoint == "stuck"),
                    "expected a stall, got {source:?}"
                );
            }
            other => panic!("expected MigrationFailed, got {other:?}"),
        }
        assert_eq!(
            version_of(&conn),
            BASELINE_VERSION,
            "a stalled migration must not claim to have finished"
        );
    }

    #[test]
    fn a_gap_in_the_sequence_is_refused_rather_than_skipped() {
        let mut conn = memory_v1();
        // A registry that jumps to 3: nothing migrates the file out of 1.
        const GAPPED: &[Migration] = &[Migration {
            version: 3,
            name: "unreachable",
            step: MigrationStep::Sql("SELECT 1"),
        }];

        let error = run(&mut conn, GAPPED, 3).expect_err("gap");

        match error {
            StateError::MigrationRequired { found, supported } => {
                assert_eq!(found, BASELINE_VERSION);
                assert_eq!(supported, 3);
            }
            other => panic!("expected MigrationRequired, got {other:?}"),
        }
        assert_eq!(version_of(&conn), BASELINE_VERSION);
    }

    #[test]
    fn a_failing_migration_names_itself() {
        let mut conn = memory_v1();
        const BROKEN: &[Migration] = &[Migration {
            version: V2,
            name: "broken_ddl",
            step: MigrationStep::Sql("ALTER TABLE nope ADD COLUMN x TEXT"),
        }];

        let error = run(&mut conn, BROKEN, V2).expect_err("broken");

        assert!(
            error
                .to_string()
                .starts_with("migration to version 2 (broken_ddl) failed:"),
            "a migration failure must name the migration: {error}"
        );
        assert_eq!(
            version_of(&conn),
            BASELINE_VERSION,
            "a failed migration leaves the version describing the data that is there"
        );
    }
}
