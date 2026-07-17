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
    /// The database is on an older schema version and this build carries no
    /// migration for the next step out of it.
    ///
    /// A const assertion in [`crate::migrate`] makes this unreachable in a
    /// build whose migration list and `SCHEMA_VERSION` agree — which is
    /// precisely why it stays a typed error. A gap in the sequence must
    /// leave the file alone and say so, not leave it half-upgraded on the
    /// strength of an invariant someone might one day break.
    MigrationRequired {
        /// The version the file declares.
        found: i64,
        /// The version this build expects.
        supported: i64,
    },
    /// A migration failed. The database is untouched by it: everything the
    /// migration had not already committed rolled back, and `user_version`
    /// still names the version whose shape the data actually has.
    ///
    /// For a resumable migration the committed chunks stay, with their
    /// checkpoint and a [`crate::RepairKind::MigrationInterrupted`] marker —
    /// re-opening resumes from there rather than starting over.
    MigrationFailed {
        /// The version the migration was producing.
        version: i64,
        /// The migration's name, for a report a user can send.
        name: &'static str,
        /// What actually went wrong.
        source: Box<StateError>,
    },
    /// A resumable migration's chunk reported that work remained but handed
    /// back the checkpoint it was given.
    ///
    /// That is a migration bug — the runner would call it forever with the
    /// same input and get the same answer — so the runner stops instead of
    /// spinning. The database keeps the last checkpoint that did make
    /// progress.
    MigrationStalled {
        /// The checkpoint the chunk returned unchanged.
        checkpoint: String,
    },
    /// A repair marker in the database names a kind this build does not
    /// know, which means a newer build raised it.
    ///
    /// Reported rather than skipped: a repair request this build cannot
    /// understand is not a repair request it may ignore.
    UnknownRepairKind {
        /// The kind string found in the database.
        kind: String,
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
            Self::MigrationFailed {
                version,
                name,
                source,
            } => write!(
                f,
                "migration to version {version} ({name}) failed: {source}"
            ),
            Self::MigrationStalled { checkpoint } => write!(
                f,
                "resumable migration reported progress without moving its checkpoint ('{checkpoint}')"
            ),
            Self::UnknownRepairKind { kind } => {
                write!(f, "unknown repair marker kind '{kind}' in the database")
            }
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
            Self::MigrationFailed { source, .. } => Some(source),
            Self::UnsupportedSchemaVersion { .. }
            | Self::MigrationRequired { .. }
            | Self::MigrationStalled { .. }
            | Self::UnknownRepairKind { .. }
            | Self::WalUnavailable { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for StateError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
