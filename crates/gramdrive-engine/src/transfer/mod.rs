//! The durable transfer state machine (TASK-260715-g4k3zm; SYNC-040..046,
//! domain-model § Transfer).
//!
//! # The durable rows are the machine
//!
//! Nothing in memory is authoritative (`.spec/architecture.md`), so this
//! module keeps no state of its own: [`TransferMachine`] carries policy, and
//! the machine's *state* is the transfer journal that `gramdrive-state`
//! persists. Every operation opens one short transaction, re-reads the row
//! it acts on, applies the row-level transition rules the repository already
//! enforces, and commits — which is why a crash between any two operations
//! loses nothing but the attempt in flight.
//!
//! Layering against the neighbouring tasks:
//!
//! * the **state repository** (`gramdrive_state::repo`) owns row-level
//!   transition legality — a terminal row accepts no further work, claiming
//!   is atomic, promotion re-checks the version pin;
//! * **this module** owns transfer *policy*: which ranges remain, when
//!   incomplete content may never promote, how failures classify against a
//!   bounded retry budget (NFR-033), and how a version race invalidates
//!   partial data deterministically;
//! * the **fetch coordinator** (TASK-260715-22fh09) will drive a
//!   `DriveSource` through claims from this machine — reader coalescing,
//!   chunk alignment, and widening the durable demand union across readers
//!   belong there ([`RequestOutcome::Attached`] reports whether the live
//!   transfer already covers a new request precisely so that layer can act);
//! * **integrity and atomic promotion** (TASK-260715-3s6cpe) layer hashing
//!   and materialization over [`TransferMachine::complete`], which gates on
//!   range coverage and the version pin — the parts provable from durable
//!   state alone.
//!
//! # Claims
//!
//! [`TransferMachine::claim`] moves the highest-priority due transfer to
//! `running` and hands back a [`ClaimedTransfer`] — the capability to
//! record progress, checkpoint, and finish that one transfer. Work-recording
//! operations exist only on claims, so "progress on an unclaimed transfer"
//! is unrepresentable in this API; the durable rules still back the token
//! (a row moved underneath a stale claim answers with
//! [`gramdrive_state::StateError::InvalidTransition`]), because an API type
//! cannot bind the other process.
//!
//! The claim carries the resume plan: [`ClaimedTransfer::remaining`] is the
//! requested set minus the durably staged set, which is what "resumes from
//! persisted ranges after a crash" means in practice — startup
//! reconciliation ([`gramdrive_state::StateStore::reconcile`]) returns
//! interrupted rows to the queue with progress intact, and the next claim
//! continues from exactly the staged bytes.
//!
//! # The promotion gate
//!
//! [`TransferMachine::complete`] refuses ([`EngineError::IncompleteContent`])
//! unless the staged ranges cover the whole target — the requested set, or
//! `[0, size)` for a whole-object transfer. A whole-object transfer whose
//! extent the projection does not know fails closed
//! ([`EngineError::UnknownExtent`]) rather than promoting unprovable
//! completeness. Only after coverage does the repository's version-pin
//! re-check run, inside the same transaction that marks the row `done`
//! (SYNC-042): incomplete or stale content is never observable as valid.
//!
//! # Version races invalidate deterministically
//!
//! The pin is taken at [`TransferMachine::request`] time from the item
//! projection, and every later gate re-derives the item's standing from the
//! same snapshot it writes in: a claim, a checkpoint, a completion, or a
//! source-reported conflict that finds the pinned version gone always
//! resolves the same way — staged progress is wiped, the row ends terminal
//! `failed`/`version_conflict` (or the precise category when the item is
//! gone or no longer fetchable), and the staging handle comes back as a
//! [`StagingDisposal`] for the host to delete. Re-requesting is left to
//! live demand: readers and pins ask again and get a fresh transfer pinned
//! at the current version.
//!
//! # Failures, budget, parking
//!
//! [`TransferMachine::fail`] classifies a [`TransferFault`] exhaustively
//! (SYNC-044): retryable classes go back to the queue with deterministic
//! backoff until the persisted retry count exhausts the
//! [`RetryPolicy::retry_budget`], then finish terminal; unwinnable classes
//! finish terminal immediately; classes waiting on an external precondition
//! (reauthorization, freed disk) *park* — suspend with progress kept — so
//! they neither poll the queue nor burn budget. Parking stores no per-row
//! reason: any resume just re-claims, and an attempt whose precondition
//! still fails parks again, so resume triggers may be as coarse as "after
//! reauthorization, resume everything suspended".
//!
//! # Cancellation
//!
//! Two-phase, per the state crate's cancellation discipline:
//! [`TransferMachine::request_cancel`] durably raises the flag from
//! anywhere; the claim holder observes it at a
//! [`TransferMachine::checkpoint`] and acknowledges with
//! [`TransferMachine::acknowledge_cancel`], which wipes the staging claim
//! and finishes the row `cancelled` (SYNC-043: what remains is safely
//! disposable, and the disposal handle says exactly what to dispose).
//! A cancel requested against a *queued* transfer is acknowledged by the
//! next [`TransferMachine::request`] for the same item and version, which
//! finishes the abandoned row and starts fresh. Completion beats a cancel
//! that arrives after the last byte: promoting finished bytes serves the
//! cache at no further cost, while honoring the flag would discard them.

mod error;
pub(crate) mod ranges;
mod retry;

pub use error::EngineError;
pub use retry::{RetryPolicy, TransferFault};

use gramdrive_model::ByteRange;
use gramdrive_model::identity::ItemId;
use gramdrive_model::version::ContentVersion;
use gramdrive_state::repo::{
    FailureCategory, ItemAvailability, ReadTxn, TransferFailure, TransferId, TransferRecord,
    WriteTxn,
};
use gramdrive_state::{StateError, StateStore};

use retry::FaultPlan;

/// Scheduler priority of a transfer — larger claims first.
///
/// The named levels are the engine's vocabulary; any value is legal, and
/// ties break toward the older transfer (journal order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Priority(pub i64);

impl Priority {
    /// A user is waiting on this read right now.
    pub const FOREGROUND: Self = Self(100);
    /// Pinned/offline content being backfilled (POL-2).
    pub const PIN_BACKFILL: Self = Self(50);
    /// Opportunistic prefetch.
    pub const BACKGROUND: Self = Self(0);
}

/// A staging area whose bytes the database no longer claims.
///
/// Returned whenever an operation orphans staged bytes (cancellation,
/// terminal failure, invalidation). The host owns the storage, so the host
/// deletes the object; a disposal dropped on the floor is reclaimed by the
/// next startup reconciliation, which is the backstop, not the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "the host must delete the staging object this names"]
pub struct StagingDisposal {
    /// The opaque staging handle (`temp_ref`) to delete.
    pub staging: String,
}

/// How a version race or departed item resolved (SYNC-042).
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "carries a staging disposal the host must honor"]
pub struct Invalidation {
    /// Why the transfer ended: `VersionConflict` for a moved pin, or the
    /// precise category when the item is gone or no longer fetchable.
    pub category: FailureCategory,
    /// The staging area the wiped progress occupied, if any.
    pub disposal: Option<StagingDisposal>,
}

/// What [`TransferMachine::request`] did with the demand.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "may carry a staging disposal the host must honor"]
pub enum RequestOutcome {
    /// A new transfer was enqueued, pinned at the item's current content
    /// version.
    Created {
        /// The new journal row.
        transfer: TransferId,
        /// Set when the request found a live transfer already flagged for
        /// cancellation, acknowledged it, and displaced its staging.
        displaced: Option<StagingDisposal>,
    },
    /// A live transfer for the same item and version already exists and
    /// the request coalesced onto it (SYNC-046).
    Attached {
        /// The live journal row.
        transfer: TransferId,
        /// Whether that transfer's requested set already covers this
        /// request. `false` means bytes this caller wants are not on the
        /// live transfer's plan — the fetch coordinator re-requests the
        /// remainder once the live transfer finishes.
        covers_request: bool,
    },
}

/// What [`TransferMachine::claim`] found.
#[derive(Debug)]
#[must_use = "a discarded claim carries a staging disposal the host must honor"]
pub enum ClaimOutcome {
    /// A transfer was claimed and is now `running`. Boxed because a claim
    /// carries the full journal record and would otherwise dwarf the other
    /// variants.
    Claimed(Box<ClaimedTransfer>),
    /// The next transfer's item departed while it sat in the queue —
    /// version moved, tombstoned, or no longer fetchable — so it was
    /// invalidated instead of claimed. Claim again for the next one.
    Discarded {
        /// The invalidated journal row.
        transfer: TransferId,
        /// How it resolved.
        invalidation: Invalidation,
    },
    /// Nothing is claimable right now.
    Empty,
}

/// The bytes a claimed transfer still has to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remaining {
    /// Exactly these ranges, in canonical order. Empty means the target is
    /// fully staged and the transfer is ready to complete.
    Ranges(Vec<ByteRange>),
    /// A whole-object transfer whose extent the projection does not know
    /// yet: fetch from the end of the staged prefix to end-of-content, and
    /// record the discovered size on the item before completing —
    /// [`TransferMachine::complete`] fails closed without it.
    UnknownExtent {
        /// What is already durably staged.
        staged: Vec<ByteRange>,
    },
}

/// What a work-boundary check of the durable state answered (SYNC-043).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a checkpoint answer decides whether work may continue"]
pub enum Checkpoint {
    /// Keep working.
    Continue,
    /// Cancellation is durably requested: stop moving bytes and
    /// [`TransferMachine::acknowledge_cancel`].
    CancelRequested,
    /// The item departed from under the pin: stop and
    /// [`TransferMachine::invalidate`].
    Drifted,
}

/// How [`TransferMachine::fail`] resolved a fault (SYNC-044).
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "may carry a staging disposal the host must honor"]
pub enum FailOutcome {
    /// Back in the queue, invisible to claims until the backoff passes.
    Requeued {
        /// The category the journal recorded.
        category: FailureCategory,
        /// When the scheduler may claim it again.
        next_retry_at_ms: i64,
        /// Failed attempts recorded so far, including this one.
        retries_used: u32,
        /// Whether staged progress was discarded first (integrity
        /// failures re-fetch from scratch).
        progress_wiped: bool,
        /// The staging area a wipe orphaned, if any.
        disposal: Option<StagingDisposal>,
    },
    /// Suspended with progress kept, awaiting an external precondition;
    /// resume with [`TransferMachine::resume`] when it changes.
    Parked {
        /// Why the transfer parked. Reported to the caller, not recorded
        /// on the row — see the module docs.
        category: FailureCategory,
    },
    /// Terminal failure; the journal keeps the category.
    Failed {
        /// The category the journal recorded.
        category: FailureCategory,
        /// The staging area the wipe orphaned, if any.
        disposal: Option<StagingDisposal>,
    },
    /// The source observed the durably requested cancel; the transfer is
    /// terminal `cancelled`.
    Cancelled {
        /// The staging area the wipe orphaned, if any.
        disposal: Option<StagingDisposal>,
    },
    /// The pinned version is gone; partial data was invalidated (SYNC-042).
    Invalidated(Invalidation),
}

/// How [`TransferMachine::complete`] resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "carries the staging handle promotion consumes, or a disposal"]
pub enum CompleteOutcome {
    /// Coverage and the version pin held; the row is terminal `done`. The
    /// staging handle holds the verified-complete bytes for the promotion
    /// layer (TASK-260715-3s6cpe) to materialize.
    Promoted {
        /// The staging area holding the complete content, if one was ever
        /// recorded (a zero-byte object needs none).
        staging: Option<String>,
    },
    /// The version pin no longer held; partial data was invalidated
    /// instead (SYNC-042).
    Invalidated(Invalidation),
}

/// The capability to work one claimed, `running` transfer.
///
/// Obtained only from [`TransferMachine::claim`]; consumed by every
/// finishing operation. It caches the item extent observed at claim time
/// for range validation — the durable row stays the authority on
/// everything else.
#[derive(Debug)]
pub struct ClaimedTransfer {
    record: TransferRecord,
    extent: Option<u64>,
}

impl ClaimedTransfer {
    /// The claimed journal row's identity.
    pub fn id(&self) -> TransferId {
        self.record.id
    }

    /// The journal row as claimed, updated by progress recorded through
    /// this claim.
    pub fn record(&self) -> &TransferRecord {
        &self.record
    }

    /// The item's logical size as of the claim, when the projection knew
    /// it.
    pub fn extent(&self) -> Option<u64> {
        self.extent
    }

    /// The staging handle the transfer claims, once one is recorded.
    pub fn staging(&self) -> Option<&str> {
        self.record.temp_ref.as_deref()
    }

    /// The resume plan: what is still to fetch, given what is durably
    /// staged (SYNC-042 resume; SYNC-041 lets staged exceed requested).
    pub fn remaining(&self) -> Remaining {
        if self.record.requested_ranges.is_empty() {
            match self.extent {
                Some(size) => Remaining::Ranges(ranges::subtract(
                    &ranges::whole_object(size),
                    &self.record.completed_ranges,
                )),
                None => Remaining::UnknownExtent {
                    staged: ranges::normalize(&self.record.completed_ranges),
                },
            }
        } else {
            Remaining::Ranges(ranges::subtract(
                &self.record.requested_ranges,
                &self.record.completed_ranges,
            ))
        }
    }
}

/// The transfer policy machine — see the module docs.
///
/// Stateless over the store on purpose: hosts construct one per policy and
/// pass the [`StateStore`] into each call, because the durable rows are the
/// only authoritative machine state.
#[derive(Debug, Clone, Default)]
pub struct TransferMachine {
    policy: RetryPolicy,
}

impl TransferMachine {
    /// A machine applying `policy`.
    pub fn new(policy: RetryPolicy) -> Self {
        Self { policy }
    }

    /// The retry policy this machine applies.
    pub fn policy(&self) -> &RetryPolicy {
        &self.policy
    }

    /// Requests hydration of `requested` (empty = the whole object),
    /// pinning the item's *current* content version (SYNC-042) and
    /// coalescing onto live work for the same item and version (SYNC-046).
    ///
    /// Refuses demand that can never be served: unknown or tombstoned
    /// items, directories (SYNC-040 — enumeration never hydrates),
    /// restricted or unavailable content (POL-4), items with no version to
    /// pin, and ranges past a known extent.
    pub fn request(
        &self,
        store: &mut StateStore,
        item: &ItemId,
        requested: &[ByteRange],
        priority: Priority,
        now_ms: i64,
    ) -> Result<RequestOutcome, EngineError> {
        let tx = store.write_txn()?;
        let record = tx
            .read()
            .item(item)?
            .ok_or(StateError::RowNotFound { entity: "item" })?;
        if record.deleted_at_ms.is_some() {
            return Err(EngineError::NotHydratable {
                reason: "the item is tombstoned (POL-3)",
            });
        }
        match record.availability {
            ItemAvailability::Fetchable => {}
            ItemAvailability::Restricted => {
                return Err(EngineError::NotHydratable {
                    reason: "content is restricted at the source (POL-4)",
                });
            }
            ItemAvailability::Unavailable => {
                return Err(EngineError::NotHydratable {
                    reason: "content is unavailable at the source",
                });
            }
        }
        let facts = record.content.ok_or(EngineError::NotHydratable {
            reason: "directories are never hydrated (SYNC-040)",
        })?;
        let version = facts.content_version.ok_or(EngineError::NotHydratable {
            reason: "the item records no content version to pin (SYNC-042)",
        })?;
        let normalized = ranges::normalize(requested);
        if let Some(extent) = facts.logical_size {
            check_extent(&normalized, extent)?;
        }
        let mut displaced = None;
        if let Some(live) = tx.read().live_transfer_for(item, &version)? {
            if live.cancel_requested {
                // Nobody will ever claim a cancel-requested row; the demand
                // arriving now is what acknowledges the abandoned cancel.
                displaced = wipe_staging(&tx, &live, now_ms)?;
                tx.mark_transfer_cancelled(live.id, now_ms)?;
            } else {
                let covers_request = if live.requested_ranges.is_empty() {
                    true
                } else if normalized.is_empty() {
                    match facts.logical_size {
                        Some(extent) => {
                            ranges::covers(&live.requested_ranges, &ranges::whole_object(extent))
                        }
                        None => false,
                    }
                } else {
                    ranges::covers(&live.requested_ranges, &normalized)
                };
                let transfer = live.id;
                tx.commit()?;
                return Ok(RequestOutcome::Attached {
                    transfer,
                    covers_request,
                });
            }
        }
        let outcome = tx.enqueue_transfer(item, &version, &normalized, priority.0, now_ms)?;
        tx.commit()?;
        Ok(RequestOutcome::Created {
            transfer: outcome.transfer_id(),
            displaced,
        })
    }

    /// Claims the highest-priority due transfer, re-validating its pin
    /// against the item projection first: a transfer whose item departed
    /// while queued is invalidated here instead of fetched for nothing.
    pub fn claim(&self, store: &mut StateStore, now_ms: i64) -> Result<ClaimOutcome, EngineError> {
        let tx = store.write_txn()?;
        let Some(record) = tx.claim_next_transfer(now_ms)? else {
            tx.commit()?;
            return Ok(ClaimOutcome::Empty);
        };
        match item_standing(tx.read(), &record.item, &record.content_version)? {
            ItemStanding::Pinned { extent } => {
                tx.commit()?;
                Ok(ClaimOutcome::Claimed(Box::new(ClaimedTransfer {
                    record,
                    extent,
                })))
            }
            ItemStanding::Departed { category } => {
                let invalidation = discard(&tx, &record, category, now_ms)?;
                tx.commit()?;
                Ok(ClaimOutcome::Discarded {
                    transfer: record.id,
                    invalidation,
                })
            }
        }
    }

    /// Records durable staging progress: the full staged set so far and
    /// the staging handle (SYNC-041).
    ///
    /// Progress is monotonic — staged bytes never un-stage
    /// ([`EngineError::ProgressRegression`]) — and the staging handle is
    /// fixed for the transfer's life ([`EngineError::StagingChanged`]);
    /// discarding staged data is a *failure* decision
    /// ([`TransferMachine::fail`] with an integrity fault), never a
    /// progress report.
    pub fn record_progress(
        &self,
        store: &mut StateStore,
        claim: &mut ClaimedTransfer,
        completed: &[ByteRange],
        staging: &str,
        now_ms: i64,
    ) -> Result<(), EngineError> {
        let tx = store.write_txn()?;
        let fresh = require_transfer(tx.read(), claim.record.id)?;
        if let Some(previous) = fresh.temp_ref.as_deref()
            && previous != staging
        {
            return Err(EngineError::StagingChanged);
        }
        let normalized = ranges::normalize(completed);
        if !ranges::covers(&normalized, &fresh.completed_ranges) {
            return Err(EngineError::ProgressRegression);
        }
        if let Some(extent) = claim.extent {
            check_extent(&normalized, extent)?;
        }
        tx.record_transfer_progress(claim.record.id, &normalized, Some(staging), now_ms)?;
        tx.commit()?;
        claim.record.completed_ranges = normalized;
        claim.record.temp_ref = Some(staging.to_owned());
        claim.record.updated_at_ms = now_ms;
        Ok(())
    }

    /// The work-boundary check (SYNC-043): reads the durable cancel flag
    /// and the item's standing. Call it between work chunks; a durable
    /// cancel outranks drift because it is the intent to stop everything,
    /// drift only the intent to stop *this version*.
    pub fn checkpoint(
        &self,
        store: &mut StateStore,
        claim: &ClaimedTransfer,
    ) -> Result<Checkpoint, EngineError> {
        let read = store.read_txn()?;
        let fresh = require_transfer(&read, claim.record.id)?;
        if fresh.cancel_requested {
            return Ok(Checkpoint::CancelRequested);
        }
        match item_standing(&read, &fresh.item, &fresh.content_version)? {
            ItemStanding::Pinned { .. } => Ok(Checkpoint::Continue),
            ItemStanding::Departed { .. } => Ok(Checkpoint::Drifted),
        }
    }

    /// Durably requests cancellation of a transfer — phase one of the
    /// two-phase cancel. Callable from anywhere with an id; returns
    /// whether a live transfer was flagged.
    pub fn request_cancel(
        &self,
        store: &mut StateStore,
        id: TransferId,
        now_ms: i64,
    ) -> Result<bool, EngineError> {
        let tx = store.write_txn()?;
        let flagged = tx.request_transfer_cancel(id, now_ms)?;
        tx.commit()?;
        Ok(flagged)
    }

    /// Acknowledges an observed cancel at a work boundary: wipes the
    /// staging claim and finishes the row `cancelled` (SYNC-043 — what
    /// remains is safely disposable, and the return value names it).
    pub fn acknowledge_cancel(
        &self,
        store: &mut StateStore,
        claim: ClaimedTransfer,
        now_ms: i64,
    ) -> Result<Option<StagingDisposal>, EngineError> {
        let tx = store.write_txn()?;
        let fresh = require_transfer(tx.read(), claim.record.id)?;
        let disposal = wipe_staging(&tx, &fresh, now_ms)?;
        tx.mark_transfer_cancelled(claim.record.id, now_ms)?;
        tx.commit()?;
        Ok(disposal)
    }

    /// Acknowledges a requested cancellation after the host has stopped the
    /// in-process source future that held the claim.
    ///
    /// This is the crash-safe counterpart to dropping an async fetch: the
    /// caller first raises the durable cancel flag, stops the source future,
    /// then uses this operation to clear progress and make the row terminal.
    /// It refuses an unflagged row so it cannot be used as an implicit
    /// cancellation or bypass the two-phase protocol.
    pub fn acknowledge_requested_cancel(
        &self,
        store: &mut StateStore,
        id: TransferId,
        now_ms: i64,
    ) -> Result<Option<StagingDisposal>, EngineError> {
        let tx = store.write_txn()?;
        let fresh = require_transfer(tx.read(), id)?;
        if !fresh.cancel_requested {
            return Err(StateError::InvalidArgument {
                what: "transfer cancellation was not requested",
            }
            .into());
        }
        let disposal = wipe_staging(&tx, &fresh, now_ms)?;
        tx.mark_transfer_cancelled(id, now_ms)?;
        tx.commit()?;
        Ok(disposal)
    }

    /// Invalidates a claimed transfer's partial data (SYNC-042): wipes
    /// staged progress and finishes the row terminal, with the category
    /// re-derived from the item's current standing (`VersionConflict` when
    /// the caller invalidates ahead of the projection).
    pub fn invalidate(
        &self,
        store: &mut StateStore,
        claim: ClaimedTransfer,
        now_ms: i64,
    ) -> Result<Invalidation, EngineError> {
        let tx = store.write_txn()?;
        let fresh = require_transfer(tx.read(), claim.record.id)?;
        let category = departed_category(tx.read(), &fresh)?;
        let invalidation = discard(&tx, &fresh, category, now_ms)?;
        tx.commit()?;
        Ok(invalidation)
    }

    /// Suspends a claimed transfer with its progress kept — the local
    /// pause (host shutdown, scheduling). Resume with
    /// [`TransferMachine::resume`].
    pub fn suspend(
        &self,
        store: &mut StateStore,
        claim: ClaimedTransfer,
        now_ms: i64,
    ) -> Result<(), EngineError> {
        let tx = store.write_txn()?;
        tx.suspend_transfer(claim.record.id, now_ms)?;
        tx.commit()?;
        Ok(())
    }

    /// Returns a suspended transfer to the queue — after a pause, or when
    /// the precondition a parked transfer waits on has changed.
    pub fn resume(
        &self,
        store: &mut StateStore,
        id: TransferId,
        now_ms: i64,
    ) -> Result<(), EngineError> {
        let tx = store.write_txn()?;
        tx.resume_transfer(id, now_ms)?;
        tx.commit()?;
        Ok(())
    }

    /// Resolves a failed attempt (SYNC-044, NFR-033): classifies the
    /// fault, applies the retry budget, and returns what happened — see
    /// [`FailOutcome`] and the module docs.
    pub fn fail(
        &self,
        store: &mut StateStore,
        claim: ClaimedTransfer,
        fault: TransferFault,
        now_ms: i64,
    ) -> Result<FailOutcome, EngineError> {
        let tx = store.write_txn()?;
        let fresh = require_transfer(tx.read(), claim.record.id)?;
        let outcome = match retry::classify(&fault) {
            FaultPlan::Final { category } => finish_failed(&tx, &fresh, category, now_ms)?,
            FaultPlan::Retry {
                category,
                source_minimum_ms,
                wipe_progress,
            } => {
                if fresh.retry_count >= self.policy.retry_budget {
                    finish_failed(&tx, &fresh, category, now_ms)?
                } else {
                    let disposal = if wipe_progress {
                        wipe_staging(&tx, &fresh, now_ms)?
                    } else {
                        None
                    };
                    let delay = self
                        .policy
                        .backoff_ms(fresh.retry_count)
                        .max(source_minimum_ms.unwrap_or(0));
                    let next_retry_at_ms = now_ms.saturating_add(delay);
                    tx.mark_transfer_failed(
                        fresh.id,
                        category,
                        TransferFailure::Retry { next_retry_at_ms },
                        now_ms,
                    )?;
                    FailOutcome::Requeued {
                        category,
                        next_retry_at_ms,
                        retries_used: fresh.retry_count.saturating_add(1),
                        progress_wiped: wipe_progress,
                        disposal,
                    }
                }
            }
            FaultPlan::Park { category } => {
                tx.suspend_transfer(fresh.id, now_ms)?;
                FailOutcome::Parked { category }
            }
            FaultPlan::CancelObserved => {
                if fresh.cancel_requested {
                    let disposal = wipe_staging(&tx, &fresh, now_ms)?;
                    tx.mark_transfer_cancelled(fresh.id, now_ms)?;
                    FailOutcome::Cancelled { disposal }
                } else {
                    // The fetch stopped locally with no durable request
                    // behind it: park, progress intact, and let the caller
                    // resume or cancel deliberately.
                    tx.suspend_transfer(fresh.id, now_ms)?;
                    FailOutcome::Parked {
                        category: FailureCategory::Cancelled,
                    }
                }
            }
            FaultPlan::Invalidate => {
                let category = departed_category(tx.read(), &fresh)?;
                FailOutcome::Invalidated(discard(&tx, &fresh, category, now_ms)?)
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// The promotion gate (SYNC-042): staged ranges must cover the whole
    /// target and the version pin must still hold, atomically with the
    /// transition to `done` — see the module docs.
    ///
    /// Borrows the claim rather than consuming it: a refused gate
    /// ([`EngineError::IncompleteContent`], [`EngineError::UnknownExtent`])
    /// changes nothing durable and leaves the claim fully usable — the
    /// caller stages the missing bytes and tries again. After a promoted or
    /// invalidated outcome the transfer is terminal and the claim is spent;
    /// anything further through it answers with the durable
    /// [`gramdrive_state::StateError::InvalidTransition`].
    pub fn complete(
        &self,
        store: &mut StateStore,
        claim: &ClaimedTransfer,
        now_ms: i64,
    ) -> Result<CompleteOutcome, EngineError> {
        let tx = store.write_txn()?;
        let fresh = require_transfer(tx.read(), claim.record.id)?;
        let extent = match item_standing(tx.read(), &fresh.item, &fresh.content_version)? {
            ItemStanding::Pinned { extent } => extent,
            ItemStanding::Departed { category } => {
                let invalidation = discard(&tx, &fresh, category, now_ms)?;
                tx.commit()?;
                return Ok(CompleteOutcome::Invalidated(invalidation));
            }
        };
        let target = if fresh.requested_ranges.is_empty() {
            let Some(extent) = extent else {
                return Err(EngineError::UnknownExtent);
            };
            ranges::whole_object(extent)
        } else {
            ranges::normalize(&fresh.requested_ranges)
        };
        let missing = ranges::subtract(&target, &fresh.completed_ranges);
        if !missing.is_empty() {
            return Err(EngineError::IncompleteContent { missing });
        }
        match tx.mark_transfer_done(fresh.id, now_ms) {
            Ok(()) => {
                tx.commit()?;
                Ok(CompleteOutcome::Promoted {
                    staging: fresh.temp_ref,
                })
            }
            // Unreachable while the standing check above shares this
            // transaction's snapshot; kept because the repository owns the
            // authoritative pin check and its answer must never be dropped.
            Err(StateError::VersionConflict { .. }) => {
                let invalidation = discard(&tx, &fresh, FailureCategory::VersionConflict, now_ms)?;
                tx.commit()?;
                Ok(CompleteOutcome::Invalidated(invalidation))
            }
            Err(error) => Err(error.into()),
        }
    }
}

/// The item projection's answer to "does the pin still hold".
pub(crate) enum ItemStanding {
    /// The pinned version is what the projection serves.
    Pinned {
        /// The item's logical size, when known.
        extent: Option<u64>,
    },
    /// The item left the pin behind: moved version, tombstoned, gone, or
    /// no longer fetchable.
    Departed {
        /// The category that describes the departure.
        category: FailureCategory,
    },
}

pub(crate) fn item_standing(
    read: &ReadTxn<'_>,
    item: &ItemId,
    pinned: &ContentVersion,
) -> Result<ItemStanding, StateError> {
    let Some(record) = read.item(item)? else {
        return Ok(ItemStanding::Departed {
            category: FailureCategory::NotFound,
        });
    };
    if record.deleted_at_ms.is_some() {
        return Ok(ItemStanding::Departed {
            category: FailureCategory::NotFound,
        });
    }
    match record.availability {
        ItemAvailability::Fetchable => {}
        ItemAvailability::Restricted => {
            return Ok(ItemStanding::Departed {
                category: FailureCategory::Restricted,
            });
        }
        ItemAvailability::Unavailable => {
            return Ok(ItemStanding::Departed {
                category: FailureCategory::Unavailable,
            });
        }
    }
    let current = record.content.and_then(|facts| {
        facts
            .content_version
            .filter(|version| version == pinned)
            .map(|_| facts.logical_size)
    });
    match current {
        Some(extent) => Ok(ItemStanding::Pinned { extent }),
        None => Ok(ItemStanding::Departed {
            category: FailureCategory::VersionConflict,
        }),
    }
}

/// The category [`ItemStanding`] assigns a departure, with
/// `VersionConflict` for a caller invalidating ahead of the projection.
fn departed_category(
    read: &ReadTxn<'_>,
    record: &TransferRecord,
) -> Result<FailureCategory, StateError> {
    Ok(
        match item_standing(read, &record.item, &record.content_version)? {
            ItemStanding::Pinned { .. } => FailureCategory::VersionConflict,
            ItemStanding::Departed { category } => category,
        },
    )
}

fn require_transfer(read: &ReadTxn<'_>, id: TransferId) -> Result<TransferRecord, StateError> {
    read.transfer(id)?
        .ok_or(StateError::RowNotFound { entity: "transfer" })
}

/// Clears a live transfer's staged ranges and staging handle, returning
/// the handle as the host's disposal duty. No-op for a transfer that never
/// staged anything.
fn wipe_staging(
    tx: &WriteTxn<'_>,
    record: &TransferRecord,
    now_ms: i64,
) -> Result<Option<StagingDisposal>, StateError> {
    if record.temp_ref.is_some() || !record.completed_ranges.is_empty() {
        tx.record_transfer_progress(record.id, &[], None, now_ms)?;
    }
    Ok(record
        .temp_ref
        .clone()
        .map(|staging| StagingDisposal { staging }))
}

/// Wipe, then finish terminal `failed` under `category` — the one shape
/// every invalidation and terminal failure shares, so a terminal
/// non-`done` row never claims staging.
fn discard(
    tx: &WriteTxn<'_>,
    record: &TransferRecord,
    category: FailureCategory,
    now_ms: i64,
) -> Result<Invalidation, StateError> {
    let disposal = wipe_staging(tx, record, now_ms)?;
    tx.mark_transfer_failed(record.id, category, TransferFailure::Final, now_ms)?;
    Ok(Invalidation { category, disposal })
}

fn finish_failed(
    tx: &WriteTxn<'_>,
    record: &TransferRecord,
    category: FailureCategory,
    now_ms: i64,
) -> Result<FailOutcome, StateError> {
    let Invalidation { category, disposal } = discard(tx, record, category, now_ms)?;
    Ok(FailOutcome::Failed { category, disposal })
}

fn check_extent(normalized: &[ByteRange], extent: u64) -> Result<(), EngineError> {
    match normalized.iter().find(|range| range.end() > extent) {
        Some(range) => Err(EngineError::RangeBeyondExtent {
            end: range.end(),
            extent,
        }),
        None => Ok(()),
    }
}
