//! Failure vocabulary of the state store.

/// Why the state store could not open, prepare, or query its database.
///
/// Structured for the NFR-030 discipline: a category a caller can act on,
/// never a panic. Everything SQLite-shaped stays in [`StateError::Sqlite`];
/// the named variants are the conditions the store itself detects and a
/// host must distinguish (a version-skewed file is user-visible "update the
/// app", a busy database is "try again").
#[derive(Debug)]
pub enum StateError {
    /// The underlying SQLite call failed.
    Sqlite(rusqlite::Error),
    /// The database carries a schema version newer than this build
    /// understands — most likely a file created by a newer app version.
    /// Refusing loudly is the NFR-041 versioning contract; touching the
    /// file could destroy what the newer schema means.
    UnsupportedSchemaVersion {
        /// The version the file declares.
        found: i64,
        /// The newest version this build supports.
        supported: i64,
    },
    /// The database is on an older schema version and needs a migration
    /// this build does not carry. Migration execution is
    /// TASK-260715-18l9xz; until it lands, an old file is rejected
    /// explicitly rather than half-upgraded.
    MigrationRequired {
        /// The version the file declares.
        found: i64,
        /// The version this build expects.
        supported: i64,
    },
    /// A file-backed database refused WAL mode. WAL is a hard requirement:
    /// the app and the provider extension read and write concurrently
    /// (`.spec/architecture.md`), and rollback-journal locking would
    /// serialize them against each other.
    WalUnavailable {
        /// The journal mode the database reported instead of `wal`.
        mode: String,
    },
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(error) => write!(f, "sqlite error: {error}"),
            Self::UnsupportedSchemaVersion { found, supported } => write!(
                f,
                "database schema version {found} is newer than the supported version {supported}"
            ),
            Self::MigrationRequired { found, supported } => write!(
                f,
                "database schema version {found} requires migration to version {supported}"
            ),
            Self::WalUnavailable { mode } => {
                write!(f, "database refused WAL journal mode (got '{mode}')")
            }
        }
    }
}

impl std::error::Error for StateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::UnsupportedSchemaVersion { .. }
            | Self::MigrationRequired { .. }
            | Self::WalUnavailable { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for StateError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
