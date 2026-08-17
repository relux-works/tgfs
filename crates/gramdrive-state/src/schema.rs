//! Versioned schema application (TASK-260715-1ceq7h, TASK-260715-18l9xz).
//!
//! The schema is a numbered sequence: `schema/v1.sql` creates version 1, and
//! every version after it is a migration ([`crate::migrate`]).
//! `PRAGMA user_version` records how far a database file has advanced
//! through that sequence, and `schema_history` records when each step was
//! applied.
//!
//! This module owns the order of operations on open, which is the whole
//! safety argument:
//!
//! 1. A fresh file (user_version 0) gets the baseline, atomically.
//! 2. A file from the future is refused *before the first write* — the
//!    version check is what stands between a newer schema and a build that
//!    would misread it.
//! 3. Only then does the runner's journal get created and the migrations
//!    run.

use rusqlite::Connection;

use crate::error::StateError;
use crate::migrate::{self, BASELINE_VERSION};

/// The schema version this build creates and expects.
///
/// Tied to [`crate::migrate::MIGRATIONS`] by a const assertion there: this
/// number and that list are one fact, and the build fails if they disagree.
pub const SCHEMA_VERSION: i64 = 22;

/// The v1 DDL, applied verbatim inside one transaction. The file is frozen:
/// schema changes are new migration scripts, never edits here (NFR-041).
const SCHEMA_V1_SQL: &str = include_str!("schema/v1.sql");

/// Brings a connection's database to [`SCHEMA_VERSION`], or reports why it
/// cannot.
///
/// A fresh database is created at [`BASELINE_VERSION`] and then migrated
/// forward like any other; a current database passes untouched. A database
/// from a newer build is refused ([`StateError::UnsupportedSchemaVersion`])
/// without being written to at all — touching it could destroy what the
/// newer schema means (NFR-041).
///
/// A database whose user_version is 0 but which already contains tables
/// (a torn file from something that is not this code) fails the CREATE
/// statements and surfaces as [`StateError::Sqlite`] — corruption is
/// reported, never repaired silently.
pub(crate) fn ensure_schema(conn: &mut Connection) -> Result<(), StateError> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let found = if found == 0 {
        apply_baseline(conn)?;
        BASELINE_VERSION
    } else {
        found
    };

    if found > SCHEMA_VERSION {
        return Err(StateError::UnsupportedSchemaVersion {
            found,
            supported: SCHEMA_VERSION,
        });
    }

    // Every write below this line is one this build is now known to be
    // entitled to make. The journal comes first because the runner needs it,
    // and it is created even when no migration is pending: repair markers
    // are read and raised on a current database too.
    migrate::ensure_journal(conn)?;
    migrate::run(conn, migrate::MIGRATIONS, SCHEMA_VERSION)
}

/// Creates the whole v1 schema and its version stamp in one transaction — a
/// crash mid-apply leaves user_version at 0 and the next open retries from
/// nothing (SYNC-072).
///
/// `pub(crate)` for the migration runner's unit tests, which need genuine
/// *baseline* databases to migrate forward — [`ensure_schema`] would carry
/// them all the way to [`SCHEMA_VERSION`].
pub(crate) fn apply_baseline(conn: &mut Connection) -> Result<(), StateError> {
    let tx = conn.transaction()?;
    tx.execute_batch(SCHEMA_V1_SQL)?;
    tx.pragma_update(None, "user_version", BASELINE_VERSION)?;
    tx.commit()?;
    Ok(())
}
