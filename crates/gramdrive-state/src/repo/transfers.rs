//! The durable transfer journal (domain-model § Transfer, SYNC-040..046).
//!
//! Lifecycle, enforced against the stored state (never against a caller's
//! stale picture):
//!
//! ```text
//! enqueue -> queued -> claim_next -> running -> mark_done ------> done
//!               ^                   |  |  \
//!               |    suspend_transfer  |   mark_transfer_failed (retry) -> queued
//!               |         v            |   mark_transfer_failed (final) -> failed
//!               |     suspended -------+
//!               |         | resume_transfer
//!               +---------+            mark_transfer_cancelled -> cancelled
//! ```
//!
//! Cancellation is two-phase on purpose (the crate's cancellation-boundary
//! discipline): [`WriteTxn::request_transfer_cancel`] durably raises a flag
//! in one short transaction, the engine observes it between work chunks —
//! [`TransferRecord::cancel_requested`] after any read — and acknowledges
//! with [`WriteTxn::mark_transfer_cancelled`]. Nothing interrupts a
//! transaction, because transactions are short enough not to need it.
//!
//! Version discipline (SYNC-042): a transfer pins the [`ContentVersion`]
//! its bytes are fetched for; [`WriteTxn::mark_transfer_done`] re-checks
//! the item's current version inside the promoting transaction and refuses
//! with [`StateError::VersionConflict`] if it moved — bytes fetched for A
//! are never published as B.

use gramdrive_model::ByteRange;
use gramdrive_model::identity::ItemId;
use gramdrive_model::version::ContentVersion;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{ReadTxn, WriteTxn, item_id_from_column, ranges};

/// Durable identity of one transfer journal row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransferId(pub i64);

/// Lifecycle state of a transfer (`transfers.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    /// Waiting for the scheduler.
    Queued,
    /// Claimed by the engine and moving bytes.
    Running,
    /// Paused with progress kept.
    Suspended,
    /// Terminal: bytes verified and promoted.
    Done,
    /// Terminal: gave up, with a [`FailureCategory`].
    Failed,
    /// Terminal: cancelled on request.
    Cancelled,
}

impl TransferState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Suspended => "suspended",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "suspended" => Ok(Self::Suspended),
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(StateError::CorruptRow {
                table: "transfers",
                detail: format!("unknown state '{other}'"),
            }),
        }
    }

    /// Whether the transfer still owns work — queued, running, or
    /// suspended. Terminal rows are history.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Suspended)
    }
}

/// The SYNC-044 failure taxonomy: the `gramdrive-source` error categories
/// plus the two local ones (`DiskFull`, `Integrity`).
///
/// Defined here rather than imported because the architecture forbids this
/// crate depending on `gramdrive-source` (crates/README.md); the engine
/// maps between the two vocabularies at its own layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureCategory {
    /// The request itself was malformed.
    InvalidRequest,
    /// The source no longer knows the object.
    NotFound,
    /// The account needs (re-)authorization.
    AuthRequired,
    /// The source is rate limiting; retry later.
    RateLimited,
    /// POL-4 restricted content.
    Restricted,
    /// The stored locator went stale and needs a refresh (SYNC-045).
    StaleReference,
    /// The content version moved mid-transfer (SYNC-042).
    VersionConflict,
    /// The source is temporarily unavailable.
    Unavailable,
    /// The transfer was cancelled.
    Cancelled,
    /// An internal source failure.
    Internal,
    /// Local disk exhaustion (SYNC-044 local category).
    DiskFull,
    /// Hash verification failed on completed bytes (SYNC-044 local
    /// category).
    Integrity,
}

impl FailureCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::NotFound => "not_found",
            Self::AuthRequired => "auth_required",
            Self::RateLimited => "rate_limited",
            Self::Restricted => "restricted",
            Self::StaleReference => "stale_reference",
            Self::VersionConflict => "version_conflict",
            Self::Unavailable => "unavailable",
            Self::Cancelled => "cancelled",
            Self::Internal => "internal",
            Self::DiskFull => "disk_full",
            Self::Integrity => "integrity",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "invalid_request" => Ok(Self::InvalidRequest),
            "not_found" => Ok(Self::NotFound),
            "auth_required" => Ok(Self::AuthRequired),
            "rate_limited" => Ok(Self::RateLimited),
            "restricted" => Ok(Self::Restricted),
            "stale_reference" => Ok(Self::StaleReference),
            "version_conflict" => Ok(Self::VersionConflict),
            "unavailable" => Ok(Self::Unavailable),
            "cancelled" => Ok(Self::Cancelled),
            "internal" => Ok(Self::Internal),
            "disk_full" => Ok(Self::DiskFull),
            "integrity" => Ok(Self::Integrity),
            other => Err(StateError::CorruptRow {
                table: "transfers",
                detail: format!("unknown failure_category '{other}'"),
            }),
        }
    }
}

/// How a failure should be handled (SYNC-044).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFailure {
    /// Retry: the transfer goes back to the queue, not before
    /// `next_retry_at_ms`.
    Retry {
        /// Earliest time the scheduler may claim it again.
        next_retry_at_ms: i64,
    },
    /// Terminal: the transfer is finished as failed.
    Final,
}

/// What [`WriteTxn::enqueue_transfer`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// No live transfer existed for the (item, version); a new row was
    /// created.
    Created(TransferId),
    /// A live transfer for the same (item, version) already exists and the
    /// request coalesced onto it (SYNC-046).
    Coalesced(TransferId),
}

impl EnqueueOutcome {
    /// The transfer the request ended up on, either way.
    pub fn transfer_id(self) -> TransferId {
        match self {
            Self::Created(id) | Self::Coalesced(id) => id,
        }
    }
}

/// One transfer journal row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRecord {
    /// Journal identity.
    pub id: TransferId,
    /// The provider item being hydrated.
    pub item: ItemId,
    /// The content version the bytes are fetched for (SYNC-042).
    pub content_version: ContentVersion,
    /// Lifecycle state.
    pub state: TransferState,
    /// Scheduler priority — larger first.
    pub priority: i64,
    /// The byte ranges the caller asked for; empty means the whole object.
    pub requested_ranges: Vec<ByteRange>,
    /// The ranges already fetched and durably staged.
    pub completed_ranges: Vec<ByteRange>,
    /// The engine's opaque handle to the staging area, if allocated.
    pub temp_ref: Option<String>,
    /// Failed attempts so far.
    pub retry_count: u32,
    /// Earliest time the scheduler may claim the row again, if backing off.
    pub next_retry_at_ms: Option<i64>,
    /// Why the last attempt failed, if any (SYNC-044).
    pub failure_category: Option<FailureCategory>,
    /// The durable cancel request flag — see the module docs.
    pub cancel_requested: bool,
    /// When the row was created (ms since the Unix epoch).
    pub created_at_ms: i64,
    /// When the row last changed (ms since the Unix epoch).
    pub updated_at_ms: i64,
}

const TRANSFER_COLUMNS: &str = "transfer_id, item_id, content_version, state, priority,
     requested_ranges, completed_ranges, temp_ref, retry_count, next_retry_at_ms,
     failure_category, cancel_requested, created_at_ms, updated_at_ms";

struct RawTransfer {
    transfer_id: i64,
    item_id: Vec<u8>,
    content_version: String,
    state: String,
    priority: i64,
    requested_ranges: String,
    completed_ranges: String,
    temp_ref: Option<String>,
    retry_count: i64,
    next_retry_at_ms: Option<i64>,
    failure_category: Option<String>,
    cancel_requested: bool,
    created_at_ms: i64,
    updated_at_ms: i64,
}

fn read_transfer(row: &Row<'_>) -> Result<RawTransfer, rusqlite::Error> {
    Ok(RawTransfer {
        transfer_id: row.get(0)?,
        item_id: row.get(1)?,
        content_version: row.get(2)?,
        state: row.get(3)?,
        priority: row.get(4)?,
        requested_ranges: row.get(5)?,
        completed_ranges: row.get(6)?,
        temp_ref: row.get(7)?,
        retry_count: row.get(8)?,
        next_retry_at_ms: row.get(9)?,
        failure_category: row.get(10)?,
        cancel_requested: row.get(11)?,
        created_at_ms: row.get(12)?,
        updated_at_ms: row.get(13)?,
    })
}

fn decode_ranges(text: &str) -> Result<Vec<ByteRange>, StateError> {
    ranges::decode(text).map_err(|error| StateError::CorruptRow {
        table: "transfers",
        detail: format!("range list does not decode: {}", error.detail),
    })
}

fn finish_transfer(raw: RawTransfer) -> Result<TransferRecord, StateError> {
    Ok(TransferRecord {
        id: TransferId(raw.transfer_id),
        item: item_id_from_column("transfers", &raw.item_id)?,
        content_version: ContentVersion::new(raw.content_version).map_err(|error| {
            StateError::CorruptRow {
                table: "transfers",
                detail: format!("content_version does not parse: {error}"),
            }
        })?,
        state: TransferState::parse(&raw.state)?,
        priority: raw.priority,
        requested_ranges: decode_ranges(&raw.requested_ranges)?,
        completed_ranges: decode_ranges(&raw.completed_ranges)?,
        temp_ref: raw.temp_ref,
        retry_count: u32::try_from(raw.retry_count).map_err(|_| StateError::CorruptRow {
            table: "transfers",
            detail: format!("retry_count {} does not fit u32", raw.retry_count),
        })?,
        next_retry_at_ms: raw.next_retry_at_ms,
        failure_category: raw
            .failure_category
            .as_deref()
            .map(FailureCategory::parse)
            .transpose()?,
        cancel_requested: raw.cancel_requested,
        created_at_ms: raw.created_at_ms,
        updated_at_ms: raw.updated_at_ms,
    })
}

fn encode_ranges(ranges_list: &[ByteRange]) -> Result<String, StateError> {
    ranges::encode(ranges_list).map_err(|what| StateError::InvalidArgument { what })
}

impl ReadTxn<'_> {
    /// One transfer by journal identity.
    pub fn transfer(&self, id: TransferId) -> Result<Option<TransferRecord>, StateError> {
        let raw = self
            .conn()
            .prepare_cached(&format!(
                "SELECT {TRANSFER_COLUMNS} FROM transfers WHERE transfer_id = ?1"
            ))?
            .query_row(params![id.0], read_transfer)
            .optional()?;
        raw.map(finish_transfer).transpose()
    }

    /// The live transfer for one (item, content version), if any — the
    /// coalescing lookup (SYNC-046).
    pub fn live_transfer_for(
        &self,
        item: &ItemId,
        version: &ContentVersion,
    ) -> Result<Option<TransferRecord>, StateError> {
        let raw = self
            .conn()
            .prepare_cached(&format!(
                "SELECT {TRANSFER_COLUMNS} FROM transfers
                 WHERE item_id = ?1 AND content_version = ?2
                   AND (state = 'queued' OR state = 'running' OR state = 'suspended')"
            ))?
            .query_row(params![item.as_bytes(), version.as_str()], read_transfer)
            .optional()?;
        raw.map(finish_transfer).transpose()
    }
}

impl WriteTxn<'_> {
    /// Requests hydration of `item` at `version`, coalescing onto an
    /// existing live transfer for the same (item, version) when one exists
    /// (SYNC-046).
    ///
    /// `requested_ranges` empty means the whole object. The item must
    /// already be projected ([`WriteTxn::upsert_item`]).
    pub fn enqueue_transfer(
        &self,
        item: &ItemId,
        version: &ContentVersion,
        requested_ranges: &[ByteRange],
        priority: i64,
        now_ms: i64,
    ) -> Result<EnqueueOutcome, StateError> {
        if let Some(live) = self.read().live_transfer_for(item, version)? {
            return Ok(EnqueueOutcome::Coalesced(live.id));
        }
        let item_exists: Option<i64> = self
            .conn()
            .prepare_cached("SELECT 1 FROM items WHERE item_id = ?1")?
            .query_row(params![item.as_bytes()], |row| row.get(0))
            .optional()?;
        if item_exists.is_none() {
            return Err(StateError::RowNotFound { entity: "item" });
        }
        self.conn()
            .prepare_cached(
                "INSERT INTO transfers (item_id, content_version, state, priority,
                                        requested_ranges, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?5)",
            )?
            .execute(params![
                item.as_bytes(),
                version.as_str(),
                priority,
                encode_ranges(requested_ranges)?,
                now_ms,
            ])?;
        Ok(EnqueueOutcome::Created(TransferId(
            self.conn().last_insert_rowid(),
        )))
    }

    /// Claims the highest-priority claimable transfer — queued, past its
    /// retry backoff, not cancel-requested — and moves it to running.
    ///
    /// `None` means the queue has nothing claimable right now. Claiming
    /// happens inside this write transaction, so two processes cannot claim
    /// the same row (the second sees `running`).
    pub fn claim_next_transfer(&self, now_ms: i64) -> Result<Option<TransferRecord>, StateError> {
        let raw = self
            .conn()
            .prepare_cached(&format!(
                "SELECT {TRANSFER_COLUMNS} FROM transfers
                 WHERE state = 'queued' AND cancel_requested = 0
                   AND (next_retry_at_ms IS NULL OR next_retry_at_ms <= ?1)
                 ORDER BY priority DESC, transfer_id LIMIT 1"
            ))?
            .query_row(params![now_ms], read_transfer)
            .optional()?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let mut record = finish_transfer(raw)?;
        self.conn()
            .prepare_cached(
                "UPDATE transfers SET state = 'running', updated_at_ms = ?2
                 WHERE transfer_id = ?1",
            )?
            .execute(params![record.id.0, now_ms])?;
        record.state = TransferState::Running;
        record.updated_at_ms = now_ms;
        Ok(Some(record))
    }

    /// Records durable staging progress: the ranges fetched so far and the
    /// staging handle (SYNC-041). Valid while the transfer is live.
    pub fn record_transfer_progress(
        &self,
        id: TransferId,
        completed_ranges: &[ByteRange],
        temp_ref: Option<&str>,
        now_ms: i64,
    ) -> Result<(), StateError> {
        if temp_ref == Some("") {
            return Err(StateError::InvalidArgument {
                what: "transfer temp_ref must not be empty text",
            });
        }
        let record = self.require_transfer(id)?;
        if !record.state.is_live() {
            return Err(invalid_transition(record.state));
        }
        self.conn()
            .prepare_cached(
                "UPDATE transfers
                 SET completed_ranges = ?2, temp_ref = ?3, updated_at_ms = ?4
                 WHERE transfer_id = ?1",
            )?
            .execute(params![
                id.0,
                encode_ranges(completed_ranges)?,
                temp_ref,
                now_ms
            ])?;
        Ok(())
    }

    /// Pauses a running transfer, keeping its progress.
    pub fn suspend_transfer(&self, id: TransferId, now_ms: i64) -> Result<(), StateError> {
        self.transition(id, &[TransferState::Running], "suspended", now_ms)
    }

    /// Returns a suspended transfer to the queue.
    pub fn resume_transfer(&self, id: TransferId, now_ms: i64) -> Result<(), StateError> {
        self.transition(id, &[TransferState::Suspended], "queued", now_ms)
    }

    /// Durably requests cancellation of a live transfer — phase one of the
    /// two-phase cancel (module docs). Returns whether a live transfer was
    /// flagged; flagging a terminal or unknown transfer is a `false`
    /// no-op, because the work it would stop no longer exists.
    pub fn request_transfer_cancel(&self, id: TransferId, now_ms: i64) -> Result<bool, StateError> {
        let changed = self
            .conn()
            .prepare_cached(
                "UPDATE transfers SET cancel_requested = 1, updated_at_ms = ?2
                 WHERE transfer_id = ?1
                   AND (state = 'queued' OR state = 'running' OR state = 'suspended')",
            )?
            .execute(params![id.0, now_ms])?;
        Ok(changed > 0)
    }

    /// Acknowledges a cancel at a work boundary: the transfer becomes
    /// terminal `cancelled` with the matching failure category.
    pub fn mark_transfer_cancelled(&self, id: TransferId, now_ms: i64) -> Result<(), StateError> {
        let record = self.require_transfer(id)?;
        if !record.state.is_live() {
            return Err(invalid_transition(record.state));
        }
        self.conn()
            .prepare_cached(
                "UPDATE transfers
                 SET state = 'cancelled', failure_category = 'cancelled', updated_at_ms = ?2
                 WHERE transfer_id = ?1",
            )?
            .execute(params![id.0, now_ms])?;
        Ok(())
    }

    /// Records a failed attempt (SYNC-044): back to the queue with backoff
    /// for [`TransferFailure::Retry`], terminal for
    /// [`TransferFailure::Final`]. Either way the category is durable and
    /// the retry count advances.
    pub fn mark_transfer_failed(
        &self,
        id: TransferId,
        category: FailureCategory,
        failure: TransferFailure,
        now_ms: i64,
    ) -> Result<(), StateError> {
        let record = self.require_transfer(id)?;
        if !record.state.is_live() {
            return Err(invalid_transition(record.state));
        }
        let (state, next_retry_at_ms) = match failure {
            TransferFailure::Retry { next_retry_at_ms } => ("queued", Some(next_retry_at_ms)),
            TransferFailure::Final => ("failed", None),
        };
        self.conn()
            .prepare_cached(
                "UPDATE transfers
                 SET state = ?2, failure_category = ?3, retry_count = retry_count + 1,
                     next_retry_at_ms = ?4, updated_at_ms = ?5
                 WHERE transfer_id = ?1",
            )?
            .execute(params![
                id.0,
                state,
                category.as_str(),
                next_retry_at_ms,
                now_ms
            ])?;
        Ok(())
    }

    /// Promotes a finished transfer (SYNC-042): re-checks that the item's
    /// current content version is still the one the bytes were fetched for,
    /// then marks the transfer terminal `done`.
    ///
    /// On [`StateError::VersionConflict`] nothing changes; the caller
    /// typically records the attempt with
    /// [`WriteTxn::mark_transfer_failed`] and
    /// [`FailureCategory::VersionConflict`], then re-enqueues for the new
    /// version. The caller's own promotion writes (blob row, cache entry,
    /// item facts) belong in this same transaction.
    pub fn mark_transfer_done(&self, id: TransferId, now_ms: i64) -> Result<(), StateError> {
        let record = self.require_transfer(id)?;
        if !matches!(
            record.state,
            TransferState::Running | TransferState::Suspended
        ) {
            return Err(invalid_transition(record.state));
        }
        let current: Option<Option<String>> = self
            .conn()
            .prepare_cached("SELECT content_version FROM items WHERE item_id = ?1")?
            .query_row(params![record.item.as_bytes()], |row| row.get(0))
            .optional()?;
        let current = current.ok_or(StateError::RowNotFound { entity: "item" })?;
        if current.as_deref() != Some(record.content_version.as_str()) {
            return Err(StateError::VersionConflict {
                entity: "transfer content",
                expected: Some(record.content_version.as_str().to_owned()),
                found: current,
            });
        }
        self.conn()
            .prepare_cached(
                "UPDATE transfers
                 SET state = 'done', failure_category = NULL, updated_at_ms = ?2
                 WHERE transfer_id = ?1",
            )?
            .execute(params![id.0, now_ms])?;
        Ok(())
    }

    fn require_transfer(&self, id: TransferId) -> Result<TransferRecord, StateError> {
        self.read()
            .transfer(id)?
            .ok_or(StateError::RowNotFound { entity: "transfer" })
    }

    fn transition(
        &self,
        id: TransferId,
        from: &[TransferState],
        to: &'static str,
        now_ms: i64,
    ) -> Result<(), StateError> {
        let record = self.require_transfer(id)?;
        if !from.contains(&record.state) {
            return Err(invalid_transition(record.state));
        }
        self.conn()
            .prepare_cached(
                "UPDATE transfers SET state = ?2, updated_at_ms = ?3 WHERE transfer_id = ?1",
            )?
            .execute(params![id.0, to, now_ms])?;
        Ok(())
    }
}

fn invalid_transition(from: TransferState) -> StateError {
    StateError::InvalidTransition {
        entity: "transfer",
        from: from.as_str(),
    }
}
