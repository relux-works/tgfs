//! Typed repositories over the state schema (TASK-260715-1opnb2).
//!
//! This module is the sanctioned way other crates touch the database: typed
//! operations over snapshots, change application, versions, transfers,
//! cache state, and render watermarks. No SQL, no `rusqlite` type, and no
//! JSON encoding crosses this boundary — callers speak the `gramdrive-model`
//! vocabulary plus the record types defined here, and every stored enum is a
//! stable string this module maps both ways (an unknown string on read is a
//! [`StateError::CorruptRow`], never a silent skip).
//!
//! # Transactions are short and explicit
//!
//! Every operation runs inside a transaction the caller opened:
//!
//! * [`StateStore::read_txn`] — a read snapshot. Under WAL a reader sees one
//!   consistent database state for the whole transaction and never blocks a
//!   writer in the other process (`.spec/architecture.md`: the app and the
//!   File Provider extension share this file).
//! * [`StateStore::write_txn`] — a write transaction, `BEGIN IMMEDIATE`. The
//!   write lock is taken up front, so a transaction never fails a lock
//!   upgrade halfway through its work; a concurrent writer waits in the
//!   busy handler instead.
//!
//! Transactions are meant to stay *short*: one change batch, one scheduler
//! decision, one publication. What must be atomic goes in one transaction —
//! SYNC-022's "cursor advances with the normalized state it witnessed" is
//! literally `apply_message_changes` and `put_cursor` under the same
//! [`WriteTxn`] — and everything else goes in its own.
//!
//! # Cancellation boundaries
//!
//! Dropping a [`WriteTxn`] without [`WriteTxn::commit`] rolls it back, so a
//! caller that stops early — task cancelled, provider callback abandoned —
//! leaves the database exactly as the last commit left it. Long-running work
//! (hydration, rendering, backfill) is structured as a sequence of short
//! transactions with checks between them; the durable cancel request of a
//! transfer ([`WriteTxn::request_transfer_cancel`]) is observed at those
//! boundaries, never mid-transaction.

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::error::StateError;
use crate::store::StateStore;

use gramdrive_model::identity::AccountScope;

mod accounts;
mod attachments;
mod auth_finalization;
mod backfill;
mod cache;
mod changes;
mod chats;
mod content_progress;
mod cursors;
mod folders;
mod item_changes;
mod items;
mod namespace_readiness;
mod provider_health;
mod ranges;
mod render;
mod stories;
mod transfers;

pub use accounts::AccountRecord;
pub use accounts::RetentionMode;
pub use accounts::SourceKind;
pub use accounts::{
    ArchiveModeChange, AuditToMirrorConfirmation, DisplayTimezoneChange, RetentionChange,
};
pub use attachments::{
    AttachmentAvailability, AttachmentFacts, AttachmentFidelity, AttachmentLogicalKind,
    AttachmentProjection, AttachmentState, BlobRecord, RetainedAttachmentVersion,
    TelegramRepresentation,
};
pub use auth_finalization::{AuthFinalizationPhase, AuthFinalizationRecord};
pub use backfill::BackfillControlRecord;
pub use cache::{
    ArchiveBackfillProgressRecord, CacheEntryRecord, CacheKind, CacheTotals, CacheUsage,
    CacheVerification, EvictionCandidate, PinOrigin, PinRecord, RetentionPurgeRecord,
};
pub use changes::{
    AppliedChanges, ChatSyncRecord, MessageChange, MessageEventKind, MessageEventRecord,
    MessagePayload, MessageRevision, MessageState, SyncWindow,
};
pub use chats::{ChatListCommitAudit, ChatListEntry, ChatRecord, ChatType};
pub use content_progress::{ChatContentPhase, ChatContentProgressRecord};
pub use folders::{FolderRecord, NamespaceBootstrapRecord};
pub use item_changes::{ChangeJournalState, ItemChangeRecord};
pub use items::{
    FileFacts, ItemAvailability, ItemKind, ItemRecord, TombstoneProvenance, item_kind,
};
pub use namespace_readiness::NamespaceReadinessRecord;
pub use provider_health::{ProviderFetchHealthCounters, ProviderFetchHealthObservation};
pub use render::{
    MonthRenderSnapshot, RenderCatalogEntry, RenderEventInput, RenderMessageInput, RenderOutput,
    RenderPublish, RenderSkipReason, RenderStateRecord,
};
pub use stories::{
    StoryAppearanceRecord, StoryArchiveEligibility, StoryContentLocatorRecord, StoryContentState,
    StoryFacts, StoryListProgressRecord, StoryLocatorFileType, StoryState, StorySyncPhase,
    StorySyncProgressRecord, StoryTombstone,
};
pub use transfers::{
    EnqueueOutcome, FailureCategory, TransferFailure, TransferId, TransferRecord, TransferState,
};

/// A read snapshot of the database.
///
/// Wraps a deferred SQLite transaction: the first read pins one consistent
/// database state, and every query until drop sees exactly that state,
/// regardless of what the other process commits meanwhile (WAL snapshot
/// isolation). Dropping it releases the snapshot; there is nothing to
/// commit.
///
/// Obtained from [`StateStore::read_txn`], or borrowed from a write
/// transaction via [`WriteTxn::read`].
#[derive(Debug)]
pub struct ReadTxn<'store> {
    tx: Transaction<'store>,
}

impl<'store> ReadTxn<'store> {
    pub(crate) fn new(tx: Transaction<'store>) -> Self {
        Self { tx }
    }

    /// The transaction's connection, for the repository modules.
    pub(crate) fn conn(&self) -> &Connection {
        &self.tx
    }
}

/// One short write transaction (`BEGIN IMMEDIATE`).
///
/// Everything written through it commits atomically on [`WriteTxn::commit`]
/// and rolls back on drop — the cancellation boundary of this crate. Read
/// operations are available through [`WriteTxn::read`] and see the
/// transaction's own uncommitted writes.
///
/// Obtained from [`StateStore::write_txn`].
#[derive(Debug)]
pub struct WriteTxn<'store> {
    read: ReadTxn<'store>,
}

impl<'store> WriteTxn<'store> {
    pub(crate) fn new(tx: Transaction<'store>) -> Self {
        Self {
            read: ReadTxn::new(tx),
        }
    }

    /// Read access within this transaction, including its own uncommitted
    /// writes.
    pub fn read(&self) -> &ReadTxn<'store> {
        &self.read
    }

    /// The transaction's connection, for the repository modules.
    pub(crate) fn conn(&self) -> &Connection {
        self.read.conn()
    }

    /// Commits everything written through this transaction, atomically.
    ///
    /// Consumes the transaction; on error the transaction is already rolled
    /// back and the database is as the previous commit left it.
    pub fn commit(self) -> Result<(), StateError> {
        Ok(self.read.tx.commit()?)
    }
}

impl StateStore {
    /// Opens a read snapshot — see [`ReadTxn`].
    ///
    /// Takes `&mut self` because one connection carries one transaction at a
    /// time; concurrent readers are separate [`StateStore`] instances over
    /// the same file, which is exactly the multi-process shape WAL exists
    /// for.
    pub fn read_txn(&mut self) -> Result<ReadTxn<'_>, StateError> {
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        Ok(ReadTxn::new(tx))
    }

    /// Opens a short write transaction — see [`WriteTxn`].
    ///
    /// `BEGIN IMMEDIATE`: the write lock is acquired now, waiting in the
    /// busy handler (up to the store's busy timeout) if the other process
    /// holds it, so the transaction cannot fail a lock upgrade after doing
    /// half its work.
    pub fn write_txn(&mut self) -> Result<WriteTxn<'_>, StateError> {
        let tx = self
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Ok(WriteTxn::new(tx))
    }
}

/// The `(account_id, namespace_version)` column pair of a scope, in SQL
/// types.
pub(crate) fn scope_columns(scope: &AccountScope) -> (i64, i64) {
    (
        scope.account.account_id.0,
        i64::from(scope.namespace_version.0),
    )
}

/// Reads a `namespace_version` column back into the model type.
pub(crate) fn namespace_from_column(
    table: &'static str,
    value: i64,
) -> Result<gramdrive_model::identity::NamespaceVersion, StateError> {
    u32::try_from(value)
        .map(gramdrive_model::identity::NamespaceVersion)
        .map_err(|_| StateError::CorruptRow {
            table,
            detail: format!("namespace_version {value} does not fit u32"),
        })
}

/// Converts a stored non-negative size column into `u64`.
pub(crate) fn size_from_column(table: &'static str, value: i64) -> Result<u64, StateError> {
    u64::try_from(value).map_err(|_| StateError::CorruptRow {
        table,
        detail: format!("negative size {value}"),
    })
}

/// Converts a caller-provided `u64` size into the INTEGER column range.
pub(crate) fn size_to_column(value: u64) -> Result<i64, StateError> {
    i64::try_from(value).map_err(|_| StateError::InvalidArgument {
        what: "size exceeds the SQLite INTEGER range",
    })
}

/// The `(hash_algo, hash)` column pair of a content hash.
pub(crate) fn hash_columns(hash: &gramdrive_model::identity::ContentHash) -> (&'static str, &[u8]) {
    match hash {
        gramdrive_model::identity::ContentHash::Sha256(digest) => ("sha256", digest.as_slice()),
    }
}

/// Reads an optional `(hash_algo, hash)` column pair back into the model
/// type. The schema CHECKs keep the pair NULL or present together; anything
/// else — or an algorithm this build does not know — is a corrupt row.
pub(crate) fn hash_from_columns(
    table: &'static str,
    algo: Option<String>,
    bytes: Option<Vec<u8>>,
) -> Result<Option<gramdrive_model::identity::ContentHash>, StateError> {
    match (algo, bytes) {
        (None, None) => Ok(None),
        (Some(algo), Some(bytes)) => {
            if algo != "sha256" {
                return Err(StateError::CorruptRow {
                    table,
                    detail: format!("unknown hash algorithm '{algo}'"),
                });
            }
            let digest: [u8; 32] =
                bytes
                    .try_into()
                    .map_err(|bytes: Vec<u8>| StateError::CorruptRow {
                        table,
                        detail: format!("sha256 hash of {} bytes", bytes.len()),
                    })?;
            Ok(Some(gramdrive_model::identity::ContentHash::Sha256(digest)))
        }
        _ => Err(StateError::CorruptRow {
            table,
            detail: "hash algorithm and hash bytes must be present together".to_owned(),
        }),
    }
}

/// Parses stored `ItemId` bytes, reporting failure as row corruption.
pub(crate) fn item_id_from_column(
    table: &'static str,
    bytes: &[u8],
) -> Result<gramdrive_model::identity::ItemId, StateError> {
    gramdrive_model::identity::ItemId::parse_bytes(bytes).map_err(|error| StateError::CorruptRow {
        table,
        detail: format!("item id does not decode: {error}"),
    })
}
