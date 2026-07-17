//! Versioned schema application (TASK-260715-1ceq7h).
//!
//! The schema is a numbered sequence of SQL scripts; `PRAGMA user_version`
//! records how far a database file has advanced through it, and
//! `schema_history` records when each step was applied. This module owns
//! exactly the v1 story: create the whole schema atomically on a fresh
//! file, recognize a current file, and refuse — explicitly, with a category
//! — files from the future or files needing a migration this build cannot
//! run. The resumable migration *runner* is TASK-260715-18l9xz; it extends
//! this module rather than replacing the contract.

use rusqlite::Connection;

use crate::error::StateError;

/// The schema version this build creates and expects.
pub const SCHEMA_VERSION: i64 = 1;

/// The v1 DDL, applied verbatim inside one transaction. The file is frozen:
/// schema changes are new migration scripts, never edits here (NFR-041).
const SCHEMA_V1_SQL: &str = include_str!("schema/v1.sql");

/// Brings a connection's database to [`SCHEMA_VERSION`], or reports why it
/// cannot.
///
/// A fresh database (user_version 0) gets the full v1 schema and its
/// version stamp in a single transaction — a crash mid-apply leaves
/// user_version at 0 and the next open retries from nothing (SYNC-072). A
/// current database passes untouched. Anything else is refused with the
/// matching [`StateError`] category.
///
/// A database whose user_version is 0 but which already contains tables
/// (a torn file from something that is not this code) fails the CREATE
/// statements and surfaces as [`StateError::Sqlite`] — corruption is
/// reported, never repaired silently.
pub(crate) fn ensure_schema(conn: &mut Connection) -> Result<(), StateError> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match found {
        0 => {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA_V1_SQL)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            Ok(())
        }
        v if v == SCHEMA_VERSION => Ok(()),
        v if v > SCHEMA_VERSION => Err(StateError::UnsupportedSchemaVersion {
            found: v,
            supported: SCHEMA_VERSION,
        }),
        v => Err(StateError::MigrationRequired {
            found: v,
            supported: SCHEMA_VERSION,
        }),
    }
}
