//! The ranged fetch coordinator (TASK-260715-22fh09; SYNC-041, SYNC-043,
//! SYNC-044, SYNC-045, SYNC-046).
//!
//! # What this layer owns
//!
//! The [`transfer`](crate::transfer) machine owns durable policy — what a
//! transfer *is*, what remains of it, when it may promote. This module owns
//! the live side: it drives a [`DriveSource`] through claims from that
//! machine, and everything in it is reconstructible from the journal plus
//! the readers currently holding open handles, which is why it keeps no
//! durable state of its own.
//!
//! * **Reader coalescing (SYNC-046).** [`FetchCoordinator::open`] turns a
//!   reader's range into journal demand through
//!   [`TransferMachine::request`], which coalesces compatible demand onto
//!   one live transfer; the coordinator keeps the reader subscription and
//!   streams bytes to every attached reader as coverage advances, so N
//!   concurrent opens of the same item cost one set of network fetches.
//!   Demand the live transfer's plan does not cover is re-requested when
//!   that transfer finishes — never fetched twice in parallel.
//! * **Chunk alignment (SYNC-041).** Sub-fetches are widened to a chunk
//!   grid and split per chunk ([`plan`]), so backend work is block-aligned
//!   and a neighbouring reader's bytes are usually already staged.
//! * **Bounded parallelism.** At most [`FetchConfig::fanout`] sub-fetches
//!   of one item are in flight; chunk completion is the scheduling grain.
//! * **Retry taxonomy (SYNC-044).** Every attempt failure is classified by
//!   [`TransferMachine::fail`], which owns budgets, backoff, and source
//!   backoff hints; the coordinator's only private reaction is the
//!   in-attempt locator refresh (SYNC-045): a
//!   [`SourceError::StaleReference`] re-asks the source for the same item
//!   — identity never changes with the refresh — a bounded number of
//!   times before the failure goes through the machine like any other.
//! * **Cancellation (SYNC-043).** Two prompt paths. Dropping the future
//!   returned by [`FetchCoordinator::run_next`] drops every in-flight
//!   source fetch at its next await point (SYNC-005) and loses nothing
//!   durable: startup reconciliation returns the interrupted row to the
//!   queue with its staged progress intact. A durable
//!   [`TransferMachine::request_cancel`] is observed at the next work
//!   boundary — including before the first byte of a fresh claim — and
//!   acknowledged with the staging disposal the host must honor.
//! * **Version races (SYNC-042).** The coordinator never publishes: it
//!   stages bytes and records progress, and completion goes through the
//!   machine's promotion gate, which re-checks coverage and the version
//!   pin atomically. A conflict — reported by the source mid-fetch or
//!   observed as drift at a checkpoint — invalidates staged bytes
//!   deterministically, and attached readers fail rather than ever seeing
//!   bytes of a version they did not open.
//!
//! # Determinism
//!
//! No clock and no entropy: time enters through the caller's [`Clock`] and
//! scheduling through the deterministic poll order of the sub-fetch fleet,
//! so a test driving [`FetchCoordinator::run_next`] on the testkit's
//! single-threaded executor sees the same interleaving on every run. The
//! retry policy's deliberate absence of jitter is inherited from the
//! machine; a host that wants decorrelation adds it when *scheduling*
//! `run_next` calls, where it belongs.
//!
//! # What this layer does not do
//!
//! It does not serve already-promoted cache content (the cache read path
//! belongs to the cache layer, TASK-260715-11abx8/3s6cpe): a reader opened
//! after promotion starts a fresh transfer. It does not verify integrity
//! or materialize (TASK-260715-3s6cpe layers over
//! [`CompleteOutcome::Promoted`]). And it does not decide *when* to run —
//! the host owns the loop, calling [`FetchCoordinator::run_next`] until
//! [`RunOutcome::Idle`] and again when backoff deadlines pass.

mod plan;
mod sink;
mod staging;

pub use staging::{Staging, StagingError, StagingHost};

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::num::{NonZeroU64, NonZeroUsize};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use gramdrive_model::ByteRange;
use gramdrive_model::identity::ItemId;
use gramdrive_model::version::ContentVersion;
use gramdrive_source::SourceError;
use gramdrive_source::{ContentChunk, ContentSink, DriveSource, FetchRequest, SinkControl};
use gramdrive_state::StateStore;
use gramdrive_state::repo::{FailureCategory, TransferId};

use crate::transfer::{
    Checkpoint, ClaimOutcome, ClaimedTransfer, CompleteOutcome, EngineError, FailOutcome,
    ItemStanding, Priority, Remaining, RequestOutcome, StagingDisposal, TransferFault,
    TransferMachine, item_standing, ranges,
};
use sink::{Breakage, ChunkSink, SharedDelivery, lock};

/// The caller's time source (SYNC-073: the core reads no clock).
///
/// `Send + Sync` so a coordinator run can be driven from an async host;
/// deterministic tests implement it over a counter they advance by hand.
pub trait Clock: Send + Sync {
    /// The current time in milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;
}

/// Default chunk grid: 512 KiB, a block size every Telegram transport
/// serves aligned reads at without amplification.
const DEFAULT_CHUNK_BYTES: u64 = 512 * 1024;

/// Coordinator tuning. Every field is engine policy, not durable state —
/// changing it between runs is safe and affects only future attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchConfig {
    /// The chunk grid (SYNC-041): sub-fetches are widened to multiples of
    /// this and never span a grid boundary. Also bounds one reader
    /// delivery chunk.
    pub chunk_bytes: NonZeroU64,
    /// Maximum concurrent sub-fetches within one claimed transfer.
    pub fanout: NonZeroUsize,
    /// In-attempt locator refreshes per chunk (SYNC-045): how many times a
    /// stale file reference is re-asked before the failure goes through
    /// the retry machinery instead.
    pub stale_refresh_limit: u32,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            chunk_bytes: NonZeroU64::new(DEFAULT_CHUNK_BYTES).unwrap_or(NonZeroU64::MIN),
            fanout: NonZeroUsize::new(4).unwrap_or(NonZeroUsize::MIN),
            stale_refresh_limit: 1,
        }
    }
}

/// One registered reader, issued by [`FetchCoordinator::open`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReaderId(pub u64);

/// A live reader subscription: a wanted range, the version it is pinned
/// to, and the sink its bytes stream into.
struct Reader {
    id: ReaderId,
    item: ItemId,
    wanted: ByteRange,
    pinned: ContentVersion,
    /// Bytes already streamed, contiguous from `wanted.start()`.
    delivered: u64,
    priority: Priority,
    sink: Box<dyn ContentSink>,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader")
            .field("id", &self.id)
            .field("item", &self.item)
            .field("wanted", &self.wanted)
            .field("pinned", &self.pinned)
            .field("delivered", &self.delivered)
            .finish_non_exhaustive()
    }
}

/// What [`FetchCoordinator::open`] did with the reader's demand.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "may carry a staging disposal the host must honor"]
pub struct OpenOutcome {
    /// The registered reader.
    pub reader: ReaderId,
    /// The live transfer the reader is attached to.
    pub transfer: TransferId,
    /// Whether the demand coalesced onto an existing transfer (SYNC-046)
    /// rather than creating one.
    pub coalesced: bool,
    /// Whether the attached transfer's plan covers the whole wanted range.
    /// `false` means the remainder is re-requested automatically when the
    /// live transfer finishes.
    pub covers: bool,
    /// A staging area orphaned by acknowledging an abandoned cancel on the
    /// same item and version; the host must delete it.
    pub displaced: Option<StagingDisposal>,
}

/// What [`FetchCoordinator::close`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseOutcome {
    /// The transfer the reader was attached to.
    pub transfer: TransferId,
    /// Readers still attached to that transfer. When this reaches zero the
    /// host decides whether the transfer keeps running (a pin backfill
    /// does) or is durably cancelled via
    /// [`FetchCoordinator::request_cancel`].
    pub remaining_readers: usize,
}

/// How one reader's subscription ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReaderEnd {
    /// Every wanted byte was streamed to the reader's sink.
    Satisfied,
    /// The reader's sink asked to stop; the transfer ran on for the
    /// demand that remains.
    Stopped,
    /// The transfer ended without the bytes; the category says why.
    /// Re-opening is the reader's decision (SYNC-042 leaves re-requesting
    /// to live demand).
    Failed {
        /// Why the bytes will not arrive.
        category: FailureCategory,
    },
    /// The live transfer finished without covering the reader's whole
    /// range; the remainder was re-requested and the subscription moved to
    /// the new transfer.
    Reattached {
        /// The transfer now carrying the reader.
        transfer: TransferId,
    },
}

/// One reader resolution within a [`RunReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReaderReport {
    /// The reader that resolved.
    pub reader: ReaderId,
    /// How it resolved.
    pub end: ReaderEnd,
}

/// How the claimed transfer's attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptEnd {
    /// Coverage and the version pin held; the row is terminal `done` and
    /// the staging handle holds verified-complete bytes for the promotion
    /// layer (TASK-260715-3s6cpe).
    Promoted {
        /// The staging handle, if the transfer ever staged bytes.
        staging: Option<String>,
    },
    /// A retryable failure within budget: the row is queued again and
    /// becomes claimable at `next_retry_at_ms` (SYNC-044; source backoff
    /// hints are already folded in).
    Requeued {
        /// The recorded failure category.
        category: FailureCategory,
        /// When the transfer may be claimed again.
        next_retry_at_ms: i64,
        /// Failed attempts so far, including this one.
        retries_used: u32,
        /// Whether staged progress was discarded first (integrity
        /// failures re-fetch from scratch).
        progress_wiped: bool,
    },
    /// Suspended with progress kept, awaiting an external precondition
    /// (reauthorization, freed disk); resume with
    /// [`FetchCoordinator::resume`].
    Parked {
        /// Why the transfer parked.
        category: FailureCategory,
    },
    /// Terminal failure; attached readers were failed with the category.
    Failed {
        /// The recorded failure category.
        category: FailureCategory,
    },
    /// A durable cancel was observed and acknowledged; the row is terminal
    /// `cancelled` (SYNC-043).
    Cancelled,
    /// The pinned version departed; staged bytes were wiped and the row is
    /// terminal (SYNC-042 — stale bytes are unpublishable by
    /// construction).
    Invalidated {
        /// How the departure classified.
        category: FailureCategory,
    },
    /// The claimed transfer's item departed while it sat in the queue; it
    /// was invalidated instead of fetched.
    Discarded {
        /// How the departure classified.
        category: FailureCategory,
    },
    /// A whole-object transfer whose extent the projection does not know:
    /// completeness is unprovable, so the transfer suspended until a
    /// metadata refresh records the size and the host resumes it.
    ExtentUnknown,
}

/// Everything one [`FetchCoordinator::run_next`] call did.
#[derive(Debug)]
#[must_use = "carries staging disposals the host must honor"]
pub struct RunReport {
    /// The transfer that was claimed.
    pub transfer: TransferId,
    /// How its attempt ended.
    pub end: AttemptEnd,
    /// Every reader that resolved during this run, in resolution order.
    /// Readers on a requeued or parked transfer stay subscribed and do not
    /// appear here.
    pub readers: Vec<ReaderReport>,
    /// Staging areas the journal no longer claims; the host must delete
    /// each one (a disposal dropped on the floor is reclaimed by the next
    /// startup reconciliation — the backstop, not the plan).
    pub disposals: Vec<StagingDisposal>,
}

/// What [`FetchCoordinator::run_next`] found to do.
#[derive(Debug)]
#[must_use = "an ignored report drops staging disposals"]
pub enum RunOutcome {
    /// Nothing is claimable right now — the queue is empty or every queued
    /// transfer is backing off or suspended.
    Idle,
    /// A transfer was claimed and its attempt ran to a resolution.
    Ran(RunReport),
}

/// The ranged fetch coordinator — see the module docs.
#[derive(Debug)]
pub struct FetchCoordinator {
    machine: TransferMachine,
    config: FetchConfig,
    /// Live reader subscriptions, keyed by the transfer serving them.
    readers: HashMap<TransferId, Vec<Reader>>,
    next_reader: u64,
}

impl FetchCoordinator {
    /// A coordinator applying `machine`'s durable policy and `config`'s
    /// live policy.
    pub fn new(machine: TransferMachine, config: FetchConfig) -> Self {
        Self {
            machine,
            config,
            readers: HashMap::new(),
            next_reader: 0,
        }
    }

    /// The durable policy machine this coordinator drives.
    pub fn machine(&self) -> &TransferMachine {
        &self.machine
    }

    /// The live policy this coordinator applies.
    pub fn config(&self) -> &FetchConfig {
        &self.config
    }

    /// Registers a reader for `wanted` of `item` and turns it into
    /// journal demand, coalescing onto live work for the same item and
    /// version (SYNC-046).
    ///
    /// Bytes stream into `sink` during subsequent
    /// [`run_next`](Self::run_next) calls, contiguously from
    /// `wanted.start()`; the read is pinned to the item's current content
    /// version and never mixes versions (SYNC-042). Demand that can never
    /// be served is refused with the machine's vocabulary
    /// ([`EngineError::NotHydratable`],
    /// [`EngineError::RangeBeyondExtent`]).
    pub fn open(
        &mut self,
        store: &mut StateStore,
        item: &ItemId,
        wanted: ByteRange,
        priority: Priority,
        sink: Box<dyn ContentSink>,
        now_ms: i64,
    ) -> Result<OpenOutcome, EngineError> {
        let outcome = self
            .machine
            .request(store, item, &[wanted], priority, now_ms)?;
        let (transfer, coalesced, covers, displaced) = match outcome {
            RequestOutcome::Created {
                transfer,
                displaced,
            } => (transfer, false, true, displaced),
            RequestOutcome::Attached {
                transfer,
                covers_request,
            } => (transfer, true, covers_request, None),
        };
        let pinned = {
            let read = store.read_txn()?;
            read.transfer(transfer)?
                .ok_or(gramdrive_state::StateError::RowNotFound { entity: "transfer" })?
                .content_version
        };
        let reader = ReaderId(self.next_reader);
        self.next_reader = self.next_reader.wrapping_add(1);
        self.readers.entry(transfer).or_default().push(Reader {
            id: reader,
            item: item.clone(),
            wanted,
            pinned,
            delivered: 0,
            priority,
            sink,
        });
        Ok(OpenOutcome {
            reader,
            transfer,
            coalesced,
            covers,
            displaced,
        })
    }

    /// Unsubscribes a reader — the host closed its handle. The transfer
    /// keeps running; see [`CloseOutcome::remaining_readers`] for the
    /// cancel decision that is the host's to make.
    pub fn close(&mut self, reader: ReaderId) -> Option<CloseOutcome> {
        let transfer = *self
            .readers
            .iter()
            .find(|(_, readers)| readers.iter().any(|r| r.id == reader))?
            .0;
        let readers = self.readers.get_mut(&transfer)?;
        readers.retain(|r| r.id != reader);
        let remaining_readers = readers.len();
        if remaining_readers == 0 {
            self.readers.remove(&transfer);
        }
        Some(CloseOutcome {
            transfer,
            remaining_readers,
        })
    }

    /// Sink-less demand — a pin backfill or prefetch. Pure passthrough to
    /// [`TransferMachine::request`]; `requested` empty means the whole
    /// object.
    pub fn hydrate(
        &self,
        store: &mut StateStore,
        item: &ItemId,
        requested: &[ByteRange],
        priority: Priority,
        now_ms: i64,
    ) -> Result<RequestOutcome, EngineError> {
        self.machine
            .request(store, item, requested, priority, now_ms)
    }

    /// Durably requests cancellation (phase one of the two-phase cancel,
    /// SYNC-043); the running attempt observes it at its next work
    /// boundary, a queued transfer before its first byte.
    pub fn request_cancel(
        &self,
        store: &mut StateStore,
        transfer: TransferId,
        now_ms: i64,
    ) -> Result<bool, EngineError> {
        self.machine.request_cancel(store, transfer, now_ms)
    }

    /// Returns a suspended transfer to the queue — after the precondition
    /// a parked transfer waited on has changed, or after a metadata
    /// refresh resolved an [`AttemptEnd::ExtentUnknown`].
    pub fn resume(
        &self,
        store: &mut StateStore,
        transfer: TransferId,
        now_ms: i64,
    ) -> Result<(), EngineError> {
        self.machine.resume(store, transfer, now_ms)
    }

    /// Claims the highest-priority due transfer and runs one attempt of it
    /// to a resolution — the coordinator's whole scheduling loop is the
    /// host calling this until [`RunOutcome::Idle`].
    ///
    /// Dropping the returned future at any await point is the prompt local
    /// cancel (SYNC-005/SYNC-043): in-flight source fetches are dropped
    /// with it, durable state stays resumable, and startup reconciliation
    /// returns the interrupted row to the queue.
    pub async fn run_next(
        &mut self,
        store: &mut StateStore,
        source: &dyn DriveSource,
        staging_host: &mut dyn StagingHost,
        clock: &dyn Clock,
    ) -> Result<RunOutcome, EngineError> {
        match self.machine.claim(store, clock.now_ms())? {
            ClaimOutcome::Empty => Ok(RunOutcome::Idle),
            ClaimOutcome::Discarded {
                transfer,
                invalidation,
            } => {
                let readers = self.end_readers(
                    transfer,
                    &ReaderEnd::Failed {
                        category: invalidation.category,
                    },
                );
                Ok(RunOutcome::Ran(RunReport {
                    transfer,
                    end: AttemptEnd::Discarded {
                        category: invalidation.category,
                    },
                    readers,
                    disposals: invalidation.disposal.into_iter().collect(),
                }))
            }
            ClaimOutcome::Claimed(claim) => {
                self.run_claim(store, source, staging_host, clock, *claim)
                    .await
            }
        }
    }

    /// One attempt of one claimed transfer, from resume plan to a durable
    /// resolution.
    async fn run_claim(
        &mut self,
        store: &mut StateStore,
        source: &dyn DriveSource,
        staging_host: &mut dyn StagingHost,
        clock: &dyn Clock,
        mut claim: ClaimedTransfer,
    ) -> Result<RunOutcome, EngineError> {
        let transfer = claim.id();
        let mut resolved: Vec<ReaderReport> = Vec::new();

        // A cancel or a drift that landed while the transfer sat in the
        // queue is honored before the first byte of network work.
        match self.machine.checkpoint(store, &claim)? {
            Checkpoint::Continue => {}
            Checkpoint::CancelRequested => {
                let disposal = self
                    .machine
                    .acknowledge_cancel(store, claim, clock.now_ms())?;
                let readers = self.end_readers(
                    transfer,
                    &ReaderEnd::Failed {
                        category: FailureCategory::Cancelled,
                    },
                );
                return Ok(RunOutcome::Ran(RunReport {
                    transfer,
                    end: AttemptEnd::Cancelled,
                    readers,
                    disposals: disposal.into_iter().collect(),
                }));
            }
            Checkpoint::Drifted => {
                let invalidation = self.machine.invalidate(store, claim, clock.now_ms())?;
                let readers = self.end_readers(
                    transfer,
                    &ReaderEnd::Failed {
                        category: invalidation.category,
                    },
                );
                return Ok(RunOutcome::Ran(RunReport {
                    transfer,
                    end: AttemptEnd::Invalidated {
                        category: invalidation.category,
                    },
                    readers,
                    disposals: invalidation.disposal.into_iter().collect(),
                }));
            }
        }

        let staged0 = ranges::normalize(&claim.record().completed_ranges);
        let chunk_plan = match claim.remaining() {
            Remaining::Ranges(remaining) => plan::chunks(
                &remaining,
                &staged0,
                claim.extent(),
                self.config.chunk_bytes.get(),
            ),
            Remaining::UnknownExtent { .. } => {
                // Completeness is unprovable until a metadata refresh
                // records the extent; fail closed like the promotion gate,
                // and leave the queue instead of polling it.
                self.machine.suspend(store, claim, clock.now_ms())?;
                return Ok(RunOutcome::Ran(RunReport {
                    transfer,
                    end: AttemptEnd::ExtentUnknown,
                    readers: Vec::new(),
                    disposals: Vec::new(),
                }));
            }
        };

        let mut staged = staged0;
        let shared = Arc::new(Mutex::new(SharedDelivery::default()));
        let mut handle = claim.staging().map(str::to_owned);
        let has_readers = self
            .readers
            .get(&transfer)
            .is_some_and(|readers| !readers.is_empty());
        if !chunk_plan.is_empty() || (has_readers && !staged.is_empty()) {
            match staging_host.open(transfer, handle.as_deref()) {
                Ok(staging) => {
                    handle = Some(staging.handle().to_owned());
                    lock(&shared).set_staging(staging);
                }
                Err(error) => {
                    let outcome =
                        self.machine
                            .fail(store, claim, error.into_fault(), clock.now_ms())?;
                    return Ok(self.fail_report(transfer, outcome, resolved));
                }
            }
        }

        // Bytes a previous attempt staged serve waiting readers before any
        // new network work (SYNC-042 resume).
        if let Err(fault) = self.deliver(transfer, &staged, &shared, &mut resolved) {
            let outcome = self.machine.fail(store, claim, fault, clock.now_ms())?;
            return Ok(self.fail_report(transfer, outcome, resolved));
        }

        let fanout = self.config.fanout.get();
        let mut queue: VecDeque<(ByteRange, u32)> =
            chunk_plan.into_iter().map(|chunk| (chunk, 0)).collect();
        let mut fleet: Vec<Slot<'_>> = Vec::new();

        let fault = loop {
            while fleet.len() < fanout {
                let Some((chunk, refreshes)) = queue.pop_front() else {
                    break;
                };
                fleet.push(Slot {
                    chunk,
                    refreshes,
                    future: spawn_sub_fetch(
                        source,
                        claim.record().item.clone(),
                        claim.record().content_version.clone(),
                        chunk,
                        Arc::clone(&shared),
                    ),
                });
            }
            if fleet.is_empty() {
                break None;
            }

            let completions = wait_any(&mut fleet).await;

            // Everything the sinks wrote is durably staged: record it
            // before deciding anything else, so a crash right here still
            // resumes from these bytes (SYNC-042), then stream the new
            // coverage to readers.
            let newly = lock(&shared).take_written();
            if !newly.is_empty() {
                staged.extend(newly);
                staged = ranges::normalize(&staged);
                if let Some(handle) = handle.as_deref() {
                    self.machine.record_progress(
                        store,
                        &mut claim,
                        &staged,
                        handle,
                        clock.now_ms(),
                    )?;
                }
                if let Err(fault) = self.deliver(transfer, &staged, &shared, &mut resolved) {
                    break Some(fault);
                }
            }

            // The durable work boundary (SYNC-043): a durable cancel
            // outranks any fault, drift outranks any source error.
            match self.machine.checkpoint(store, &claim)? {
                Checkpoint::Continue => {}
                Checkpoint::CancelRequested => {
                    // Dropping the sub-fetch futures is the prompt cancel
                    // (SYNC-005); the sinks' stop flag is redundant here
                    // but keeps an already-polled source honest.
                    lock(&shared).request_stop();
                    drop(fleet);
                    let disposal = self
                        .machine
                        .acknowledge_cancel(store, claim, clock.now_ms())?;
                    resolved.extend(self.end_readers(
                        transfer,
                        &ReaderEnd::Failed {
                            category: FailureCategory::Cancelled,
                        },
                    ));
                    return Ok(RunOutcome::Ran(RunReport {
                        transfer,
                        end: AttemptEnd::Cancelled,
                        readers: resolved,
                        disposals: disposal.into_iter().collect(),
                    }));
                }
                Checkpoint::Drifted => {
                    lock(&shared).request_stop();
                    drop(fleet);
                    let invalidation = self.machine.invalidate(store, claim, clock.now_ms())?;
                    resolved.extend(self.end_readers(
                        transfer,
                        &ReaderEnd::Failed {
                            category: invalidation.category,
                        },
                    ));
                    return Ok(RunOutcome::Ran(RunReport {
                        transfer,
                        end: AttemptEnd::Invalidated {
                            category: invalidation.category,
                        },
                        readers: resolved,
                        disposals: invalidation.disposal.into_iter().collect(),
                    }));
                }
            }

            let mut broke = None;
            for completion in completions {
                if let Err(fault) = self.settle(completion, &shared, &mut queue) {
                    broke = Some(fault);
                    break;
                }
            }
            if broke.is_some() {
                break broke;
            }
        };

        // Any exit with sub-fetches still in flight cancels them by drop.
        lock(&shared).request_stop();
        drop(fleet);

        if let Some(fault) = fault {
            let outcome = self.machine.fail(store, claim, fault, clock.now_ms())?;
            return Ok(self.fail_report(transfer, outcome, resolved));
        }

        // The plan ran dry; the promotion gate proves coverage and the
        // version pin atomically with the transition to done (SYNC-042) —
        // incomplete or stale content is unpublishable by construction.
        match self.machine.complete(store, &claim, clock.now_ms())? {
            CompleteOutcome::Promoted { staging } => {
                let (reports, disposals) = self.reattach(store, transfer, clock.now_ms())?;
                resolved.extend(reports);
                Ok(RunOutcome::Ran(RunReport {
                    transfer,
                    end: AttemptEnd::Promoted { staging },
                    readers: resolved,
                    disposals,
                }))
            }
            CompleteOutcome::Invalidated(invalidation) => {
                resolved.extend(self.end_readers(
                    transfer,
                    &ReaderEnd::Failed {
                        category: invalidation.category,
                    },
                ));
                Ok(RunOutcome::Ran(RunReport {
                    transfer,
                    end: AttemptEnd::Invalidated {
                        category: invalidation.category,
                    },
                    readers: resolved,
                    disposals: invalidation.disposal.into_iter().collect(),
                }))
            }
        }
    }

    /// Resolves one finished sub-fetch: clean completion, an in-attempt
    /// locator refresh (SYNC-045), or the fault that ends the attempt.
    fn settle(
        &self,
        completion: Completion,
        shared: &Arc<Mutex<SharedDelivery>>,
        queue: &mut VecDeque<(ByteRange, u32)>,
    ) -> Result<(), TransferFault> {
        // A breakage latched by the sink outranks the source's verdict:
        // the source only reports Cancelled because the sink stopped it,
        // and *why* it stopped is the actual fault.
        if let Some(breakage) = lock(shared).take_breakage() {
            return Err(match breakage {
                Breakage::Violation(violation) => TransferFault::Source(SourceError::Internal {
                    detail: format!("source violated the delivery contract: {violation}"),
                }),
                Breakage::Staging(error) => error.into_fault(),
            });
        }
        let Completion {
            chunk,
            refreshes,
            sink,
            result,
        } = completion;
        match result {
            Ok(()) if sink.is_complete() => Ok(()),
            Ok(()) => Err(TransferFault::Source(SourceError::Internal {
                detail: format!(
                    "source resolved after delivering {} of {} bytes",
                    sink.delivered(),
                    chunk.len()
                ),
            })),
            Err(SourceError::StaleReference { .. })
                if refreshes < self.config.stale_refresh_limit =>
            {
                // The refresh is re-asking with the same identity
                // (SYNC-045); only the undelivered tail goes back on the
                // wire, so refreshes never duplicate staged bytes.
                if let Ok(rest) = ByteRange::new(chunk.start() + sink.delivered(), chunk.end()) {
                    queue.push_front((rest, refreshes + 1));
                }
                Ok(())
            }
            Err(error) => Err(TransferFault::Source(error)),
        }
    }

    /// Streams newly covered bytes to every reader of `transfer`,
    /// resolving the ones that finish or stop.
    fn deliver(
        &mut self,
        transfer: TransferId,
        staged: &[ByteRange],
        shared: &Arc<Mutex<SharedDelivery>>,
        resolved: &mut Vec<ReaderReport>,
    ) -> Result<(), TransferFault> {
        let read_cap = self.config.chunk_bytes.get();
        let Some(readers) = self.readers.get_mut(&transfer) else {
            return Ok(());
        };
        let mut index = 0;
        while index < readers.len() {
            match stream_to_reader(&mut readers[index], staged, shared, read_cap)? {
                Some(end) => {
                    let reader = readers.remove(index);
                    resolved.push(ReaderReport {
                        reader: reader.id,
                        end,
                    });
                }
                None => index += 1,
            }
        }
        if readers.is_empty() {
            self.readers.remove(&transfer);
        }
        Ok(())
    }

    /// Fails every reader of `transfer` with `end`, in subscription order.
    fn end_readers(&mut self, transfer: TransferId, end: &ReaderEnd) -> Vec<ReaderReport> {
        self.readers
            .remove(&transfer)
            .unwrap_or_default()
            .into_iter()
            .map(|reader| ReaderReport {
                reader: reader.id,
                end: end.clone(),
            })
            .collect()
    }

    /// Moves the finished transfer's unsatisfied readers onto fresh demand
    /// — the machine's contract: "the fetch coordinator re-requests the
    /// remainder once the live transfer finishes" (SYNC-046).
    fn reattach(
        &mut self,
        store: &mut StateStore,
        transfer: TransferId,
        now_ms: i64,
    ) -> Result<(Vec<ReaderReport>, Vec<StagingDisposal>), EngineError> {
        let leftovers = self.readers.remove(&transfer).unwrap_or_default();
        let mut reports = Vec::new();
        let mut disposals = Vec::new();
        for reader in leftovers {
            let Ok(rest) = ByteRange::new(
                reader.wanted.start() + reader.delivered,
                reader.wanted.end(),
            ) else {
                // Fully delivered; delivery normally resolves this before
                // the transfer finishes, so this is only a backstop.
                reports.push(ReaderReport {
                    reader: reader.id,
                    end: ReaderEnd::Satisfied,
                });
                continue;
            };
            // The read is pinned (SYNC-042): a reader is never topped up
            // with bytes of a version it did not open.
            let standing = {
                let read = store.read_txn()?;
                item_standing(&read, &reader.item, &reader.pinned)?
            };
            if let ItemStanding::Departed { category } = standing {
                reports.push(ReaderReport {
                    reader: reader.id,
                    end: ReaderEnd::Failed { category },
                });
                continue;
            }
            let outcome =
                self.machine
                    .request(store, &reader.item, &[rest], reader.priority, now_ms)?;
            let next = match outcome {
                RequestOutcome::Created {
                    transfer,
                    displaced,
                } => {
                    disposals.extend(displaced);
                    transfer
                }
                RequestOutcome::Attached { transfer, .. } => transfer,
            };
            reports.push(ReaderReport {
                reader: reader.id,
                end: ReaderEnd::Reattached { transfer: next },
            });
            self.readers.entry(next).or_default().push(reader);
        }
        Ok((reports, disposals))
    }

    /// Folds a [`FailOutcome`] into the run report, resolving readers for
    /// terminal outcomes and keeping them subscribed for recoverable ones.
    fn fail_report(
        &mut self,
        transfer: TransferId,
        outcome: FailOutcome,
        mut resolved: Vec<ReaderReport>,
    ) -> RunOutcome {
        let (end, disposals) = match outcome {
            FailOutcome::Requeued {
                category,
                next_retry_at_ms,
                retries_used,
                progress_wiped,
                disposal,
            } => (
                AttemptEnd::Requeued {
                    category,
                    next_retry_at_ms,
                    retries_used,
                    progress_wiped,
                },
                disposal.into_iter().collect(),
            ),
            FailOutcome::Parked { category } => (AttemptEnd::Parked { category }, Vec::new()),
            FailOutcome::Failed { category, disposal } => {
                resolved.extend(self.end_readers(transfer, &ReaderEnd::Failed { category }));
                (
                    AttemptEnd::Failed { category },
                    disposal.into_iter().collect(),
                )
            }
            FailOutcome::Cancelled { disposal } => {
                resolved.extend(self.end_readers(
                    transfer,
                    &ReaderEnd::Failed {
                        category: FailureCategory::Cancelled,
                    },
                ));
                (AttemptEnd::Cancelled, disposal.into_iter().collect())
            }
            FailOutcome::Invalidated(invalidation) => {
                resolved.extend(self.end_readers(
                    transfer,
                    &ReaderEnd::Failed {
                        category: invalidation.category,
                    },
                ));
                (
                    AttemptEnd::Invalidated {
                        category: invalidation.category,
                    },
                    invalidation.disposal.into_iter().collect(),
                )
            }
        };
        RunOutcome::Ran(RunReport {
            transfer,
            end,
            readers: resolved,
            disposals,
        })
    }
}

/// A sub-fetch future together with what it was fetching.
struct Slot<'a> {
    chunk: ByteRange,
    refreshes: u32,
    future: SubFetchFuture<'a>,
}

/// One finished sub-fetch.
struct Completion {
    chunk: ByteRange,
    refreshes: u32,
    sink: ChunkSink,
    result: Result<(), SourceError>,
}

type SubFetchFuture<'a> =
    Pin<Box<dyn Future<Output = (ChunkSink, Result<(), SourceError>)> + Send + 'a>>;

/// One chunk's fetch: the async block owns the sink, so the future is
/// self-contained and droppable — dropping it is the prompt per-chunk
/// cancel (SYNC-005).
fn spawn_sub_fetch<'a>(
    source: &'a dyn DriveSource,
    item: ItemId,
    version: ContentVersion,
    chunk: ByteRange,
    shared: Arc<Mutex<SharedDelivery>>,
) -> SubFetchFuture<'a> {
    let request = FetchRequest {
        item,
        version,
        range: chunk,
    };
    Box::pin(async move {
        let mut sink = ChunkSink::new(chunk, shared);
        let result = source.fetch(request, &mut sink).await;
        (sink, result)
    })
}

/// Polls every in-flight sub-fetch and resolves once at least one
/// finishes, removing the finished ones from the fleet.
///
/// Deterministic by construction: futures are polled in fleet order every
/// pass, so a single-threaded executor sees one interleaving forever.
async fn wait_any(fleet: &mut Vec<Slot<'_>>) -> Vec<Completion> {
    std::future::poll_fn(|context| {
        let mut ready = Vec::new();
        let mut index = 0;
        while index < fleet.len() {
            match fleet[index].future.as_mut().poll(context) {
                Poll::Ready((sink, result)) => {
                    let slot = fleet.remove(index);
                    ready.push(Completion {
                        chunk: slot.chunk,
                        refreshes: slot.refreshes,
                        sink,
                        result,
                    });
                }
                Poll::Pending => index += 1,
            }
        }
        if ready.is_empty() {
            Poll::Pending
        } else {
            Poll::Ready(ready)
        }
    })
    .await
}

/// Streams every contiguously covered byte to one reader, returning how
/// the subscription resolved, if it did.
fn stream_to_reader(
    reader: &mut Reader,
    staged: &[ByteRange],
    shared: &Arc<Mutex<SharedDelivery>>,
    read_cap: u64,
) -> Result<Option<ReaderEnd>, TransferFault> {
    loop {
        let cursor = reader.wanted.start() + reader.delivered;
        if cursor >= reader.wanted.end() {
            return Ok(Some(ReaderEnd::Satisfied));
        }
        let Some(cover_end) = covered_end(staged, cursor) else {
            return Ok(None);
        };
        let take = cover_end
            .min(reader.wanted.end())
            .saturating_sub(cursor)
            .min(read_cap);
        if take == 0 {
            return Ok(None);
        }
        let len = usize::try_from(take).map_err(|_| TransferFault::Integrity {
            detail: format!("reader span of {take} bytes exceeds the address space"),
        })?;
        let mut buffer = vec![0u8; len];
        lock(shared)
            .read_at(cursor, &mut buffer)
            .map_err(StagingError::into_fault)?;
        let chunk = match ContentChunk::new(cursor, &buffer) {
            Ok(chunk) => chunk,
            Err(invalid) => {
                return Err(TransferFault::Integrity {
                    detail: format!("reader chunk failed to form: {invalid}"),
                });
            }
        };
        let control = reader.sink.accept(chunk);
        reader.delivered += take;
        if control == SinkControl::Stop {
            let done = reader.wanted.start() + reader.delivered >= reader.wanted.end();
            return Ok(Some(if done {
                ReaderEnd::Satisfied
            } else {
                ReaderEnd::Stopped
            }));
        }
    }
}

/// The exclusive end of the staged range containing `offset`, if any.
/// `staged` is canonical (sorted, disjoint), so the first hit is the only
/// one.
fn covered_end(staged: &[ByteRange], offset: u64) -> Option<u64> {
    staged
        .iter()
        .find(|range| range.start() <= offset && offset < range.end())
        .map(ByteRange::end)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn range(start: u64, end: u64) -> ByteRange {
        ByteRange::new(start, end).expect("valid range")
    }

    #[test]
    fn covered_end_finds_the_containing_range() {
        let staged = [range(0, 16), range(32, 48)];
        assert_eq!(covered_end(&staged, 0), Some(16));
        assert_eq!(covered_end(&staged, 15), Some(16));
        assert_eq!(covered_end(&staged, 16), None);
        assert_eq!(covered_end(&staged, 40), Some(48));
        assert_eq!(covered_end(&staged, 48), None);
    }

    #[test]
    fn default_config_is_sane() {
        let config = FetchConfig::default();
        assert_eq!(config.chunk_bytes.get(), 512 * 1024);
        assert_eq!(config.fanout.get(), 4);
        assert_eq!(config.stale_refresh_limit, 1);
    }
}
