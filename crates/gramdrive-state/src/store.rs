//! Opening and configuring the state database.
//!
//! One [`StateStore`] wraps one SQLite connection, configured the only way
//! this crate supports: WAL journaling (the app and the provider extension
//! are separate processes over one file — `.spec/architecture.md`), foreign
//! keys enforced, a busy timeout instead of immediate `SQLITE_BUSY`
//! failures, and the schema at exactly [`crate::schema::SCHEMA_VERSION`].
//! Where the file lives is the embedding host's decision; this module never
//! chooses paths.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::error::StateError;
use crate::schema::ensure_schema;

/// How long a connection waits on a locked database before failing.
///
/// Transactions in this crate are short by design; a hold longer than this
/// is a bug worth surfacing, not worth waiting out.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// One configured connection to the state database.
///
/// Repositories (TASK-260715-1opnb2) build on [`StateStore::connection`];
/// this type owns only what must be true before any query runs: pragmas,
/// journal mode, and schema version.
#[derive(Debug)]
pub struct StateStore {
    conn: Connection,
}

impl StateStore {
    /// Opens (creating if absent) the database at `path`, in WAL mode, with
    /// the schema at [`crate::schema::SCHEMA_VERSION`].
    ///
    /// Fails with a named category when the file is from a newer build
    /// ([`StateError::UnsupportedSchemaVersion`]), needs a migration this
    /// build cannot run ([`StateError::MigrationRequired`]), or refuses WAL
    /// ([`StateError::WalUnavailable`]).
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let conn = Connection::open(path)?;
        Self::configure(conn, JournalRequirement::Wal)
    }

    /// Opens a private in-memory database — schema and pragmas as
    /// [`StateStore::open`], minus WAL, which SQLite does not offer for
    /// memory databases. For tests; a memory database proves schema and
    /// query behavior, never durability.
    pub fn open_in_memory() -> Result<Self, StateError> {
        let conn = Connection::open_in_memory()?;
        Self::configure(conn, JournalRequirement::MemoryDefault)
    }

    fn configure(mut conn: Connection, journal: JournalRequirement) -> Result<Self, StateError> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Per-connection, off by default in SQLite for historical reasons;
        // every invariant in the schema assumes it is on.
        conn.pragma_update(None, "foreign_keys", true)?;
        if matches!(journal, JournalRequirement::Wal) {
            // `PRAGMA journal_mode` answers with the mode actually in
            // effect; anything but "wal" on a file database means the
            // multi-process contract cannot hold.
            let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(StateError::WalUnavailable { mode });
            }
            // NORMAL is the documented WAL pairing: fsync on checkpoint
            // rather than on every commit. Durability of the last commit
            // before an OS crash is traded for not fsyncing every short
            // metadata transaction; application crashes lose nothing.
            conn.pragma_update(None, "synchronous", "NORMAL")?;
        }
        ensure_schema(&mut conn)?;
        Ok(Self { conn })
    }

    /// The configured connection. Repositories and tests issue their SQL
    /// through this; the store guarantees pragmas and schema, not query
    /// vocabulary.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Mutable access, for callers that need `rusqlite` transactions
    /// (which take `&mut Connection`).
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// The schema version of the underlying database. After a successful
    /// open this is [`crate::schema::SCHEMA_VERSION`] by construction; exposed so callers
    /// can record it in diagnostics without knowing the pragma.
    pub fn schema_version(&self) -> Result<i64, StateError> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }
}

/// What journal mode an open must establish.
#[derive(Debug, Clone, Copy)]
enum JournalRequirement {
    /// File database: WAL or refuse.
    Wal,
    /// Memory database: keep SQLite's own mode ("memory").
    MemoryDefault,
}
