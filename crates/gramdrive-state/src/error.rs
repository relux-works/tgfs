//! Failure vocabulary of the state store.

use gramdrive_model::cursor::{CursorParseError, CursorScopeMismatch};

/// Why the state store could not open, prepare, query, or update its
/// database.
///
/// Structured for the NFR-030 discipline: a category a caller can act on,
/// never a panic. Everything SQLite-shaped stays in [`StateError::Sqlite`];
/// the named variants are the conditions the store itself detects and a
/// host must distinguish (a version-skewed file is user-visible "update the
/// app", a busy database is "try again", a version conflict is "re-read and
/// decide").
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
    /// A repository operation was called with an argument the typed layer
    /// refuses before SQL ever sees it — an empty cursor stream, the folder-0
    /// sentinel used as a real folder, content facts on a directory item.
    ///
    /// Kept distinct from [`StateError::Sqlite`] on purpose: a schema CHECK
    /// firing means the typed layer failed at its one job of making invalid
    /// rows unrepresentable; this variant is that job succeeding.
    InvalidArgument {
        /// What was wrong with the argument.
        what: &'static str,
    },
    /// A repository operation required a row that does not exist — finishing
    /// an unknown transfer, publishing render output for an item with no
    /// render state.
    ///
    /// Deliberately not used by lookups: absence a caller can plan around is
    /// an `Option::None`, absence that invalidates the operation is this.
    RowNotFound {
        /// The entity the operation required.
        entity: &'static str,
    },
    /// A compare-and-set update found the stored version differs from the
    /// version the caller based its work on (DOM-003, SYNC-042).
    ///
    /// The database is unchanged. The caller re-reads and decides: retry
    /// against the current version, or discard work that is now stale —
    /// bytes fetched for version A are never published as version B.
    VersionConflict {
        /// The entity whose version was checked.
        entity: &'static str,
        /// The version the caller expected to find; `None` means the caller
        /// expected no version stored yet (a first publication).
        expected: Option<String>,
        /// The version actually stored; `None` when no version is stored.
        found: Option<String>,
    },
    /// A render publication carried an input watermark below the one already
    /// recorded (SYNC-024, SYNC-030).
    ///
    /// Watermarks only advance: published bytes claiming to reflect *less*
    /// of the event log than the current bytes would silently un-render
    /// observed history. The database is unchanged.
    WatermarkRegression {
        /// The watermark already recorded.
        current: i64,
        /// The lower watermark the publication carried.
        proposed: i64,
    },
    /// A whole-list snapshot would remove members without both declaring
    /// itself complete and carrying a durable chat-departure witness. The
    /// transaction is unchanged so the caller can resume from its previous
    /// checkpoint instead of publishing a false disappearance.
    UnsafeChatListShrink {
        /// Number of members before the proposed replacement.
        before_count: u64,
        /// Number of members in the proposed replacement.
        after_count: u64,
        /// Number of omitted members without `left_at_ms` or `deleted_at_ms`.
        uncorroborated_removals: u64,
    },
    /// A change cursor was presented against a scope it was not minted
    /// under — the wrong account, or a namespace epoch that has since been
    /// retired (SYNC-004, DOM-021).
    ///
    /// Rejected explicitly, never silently applied: the only correct
    /// reaction is re-baselining under the current scope.
    CursorOutOfScope {
        /// The scope mismatch, as the model layer reports it.
        source: CursorScopeMismatch,
    },
    /// A cursor stored in the database no longer parses — corruption, or a
    /// cursor format from a newer build (SYNC-004).
    CursorCorrupt {
        /// Why the stored text failed to parse.
        source: CursorParseError,
    },
    /// A stored row failed to round-trip through the typed layer — an enum
    /// column carries text this build does not know, an identity blob does
    /// not decode, a range list is not the format this crate writes.
    ///
    /// This is corruption or version skew, not a caller mistake; the row is
    /// reported, never silently skipped or coerced.
    CorruptRow {
        /// The table the row lives in.
        table: &'static str,
        /// What failed to round-trip.
        detail: String,
    },
    /// A repository operation found the row in a state the operation is not
    /// valid from — completing a transfer that is already terminal, resuming
    /// one that was never suspended.
    ///
    /// The database is unchanged; the caller re-reads and reconciles its
    /// picture of the lifecycle with the durable one.
    InvalidTransition {
        /// The entity whose lifecycle refused the step.
        entity: &'static str,
        /// The state the row was actually in.
        from: &'static str,
    },
    /// The host's local storage could not be inventoried, so reconciliation
    /// has nothing to compare the database against (SYNC-070).
    ///
    /// Fatal to the pass rather than a finding: a survey against a *partial*
    /// inventory would read every unlisted object as an orphan and delete
    /// live cache. A storage failure on one individual object is the
    /// survivable case, and that one is reported as an unresolved finding.
    LocalStorage {
        /// The host's description of the failure.
        detail: String,
    },
    /// A filesystem operation on the database's own files failed while
    /// quarantining a corrupt database ([`crate::recovery`]) — creating the
    /// quarantine directory, or moving a damaged file into it.
    QuarantineIo {
        /// The quarantine step that failed.
        step: &'static str,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
}

impl StateError {
    /// Whether this error reports *file-level* database corruption —
    /// SQLite's `SQLITE_CORRUPT` ("database disk image is malformed") or
    /// `SQLITE_NOTADB` ("file is not a database").
    ///
    /// This is the [`crate::recovery`] trigger: a `true` here is the one
    /// condition under which quarantining the file is the correct reaction.
    /// Row-level decode failures ([`StateError::CorruptRow`],
    /// [`StateError::CursorCorrupt`]) are deliberately *not* included: they
    /// can also mean version skew, and destroying the whole file over one
    /// undecodable row would trade a bounded repair for total loss.
    /// Wrapper variants ([`StateError::MigrationFailed`]) answer for the
    /// error they wrap.
    pub fn is_database_corruption(&self) -> bool {
        match self {
            Self::Sqlite(rusqlite::Error::SqliteFailure(error, _)) => matches!(
                error.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            ),
            Self::MigrationFailed { source, .. } => source.is_database_corruption(),
            _ => false,
        }
    }
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
            Self::InvalidArgument { what } => write!(f, "invalid argument: {what}"),
            Self::RowNotFound { entity } => write!(f, "{entity} not found"),
            Self::VersionConflict {
                entity,
                expected,
                found,
            } => {
                let expected = expected.as_deref().unwrap_or("(none)");
                let found = found.as_deref().unwrap_or("(none)");
                write!(
                    f,
                    "{entity} version conflict: expected '{expected}', found '{found}'"
                )
            }
            Self::WatermarkRegression { current, proposed } => write!(
                f,
                "render watermark regression: proposed {proposed} is below current {current}"
            ),
            Self::UnsafeChatListShrink {
                before_count,
                after_count,
                uncorroborated_removals,
            } => write!(
                f,
                "unsafe chat-list shrink: {before_count} members to {after_count} with \
                 {uncorroborated_removals} uncorroborated removals"
            ),
            Self::CursorOutOfScope { source } => write!(f, "{source}"),
            Self::CursorCorrupt { source } => {
                write!(f, "stored change cursor failed to parse: {source}")
            }
            Self::CorruptRow { table, detail } => {
                write!(f, "corrupt row in {table}: {detail}")
            }
            Self::InvalidTransition { entity, from } => {
                write!(f, "invalid {entity} transition from state '{from}'")
            }
            Self::LocalStorage { detail } => {
                write!(f, "local storage could not be inventoried: {detail}")
            }
            Self::QuarantineIo { step, source } => {
                write!(f, "quarantine step '{step}' failed: {source}")
            }
        }
    }
}

impl std::error::Error for StateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::MigrationFailed { source, .. } => Some(source),
            Self::CursorOutOfScope { source } => Some(source),
            Self::CursorCorrupt { source } => Some(source),
            Self::QuarantineIo { source, .. } => Some(source),
            Self::UnsupportedSchemaVersion { .. }
            | Self::MigrationRequired { .. }
            | Self::MigrationStalled { .. }
            | Self::UnknownRepairKind { .. }
            | Self::WalUnavailable { .. }
            | Self::InvalidArgument { .. }
            | Self::RowNotFound { .. }
            | Self::VersionConflict { .. }
            | Self::WatermarkRegression { .. }
            | Self::UnsafeChatListShrink { .. }
            | Self::CorruptRow { .. }
            | Self::InvalidTransition { .. }
            | Self::LocalStorage { .. } => None,
        }
    }
}

impl From<rusqlite::Error> for StateError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
