//! Startup reconciliation and the repair plan (SYNC-070, SYNC-071,
//! NFR-034).
//!
//! Nothing in memory is authoritative (`.spec/architecture.md`), so what a
//! process believed while it was running is gone the moment it dies. What
//! survives is the database and the bytes on disk, and those two can
//! disagree: a transfer marked `running` that nobody is running, a staging
//! area whose transfer finished, a `cache_entries` row for bytes the OS
//! purged, an object no row claims. Reconciliation is the pass that makes
//! them agree again, from durable evidence only.
//!
//! # The precondition: no engine is running against the file
//!
//! Every check here reads a disagreement between the database and the disk
//! as damage. A *live* engine is a permanent, legitimate source of exactly
//! those disagreements — it is always between two steps of something: bytes
//! staged but the range not yet recorded, an object written but its row not
//! yet committed, a row dropped but the object not yet deleted. Against a
//! live engine every finding here is indistinguishable from work in
//! progress, and "repairing" it would delete the bytes the engine is at that
//! moment writing.
//!
//! So reconciliation requires what `fsck` requires: nothing else may be
//! touching what it repairs. That is a caller contract, not something this
//! crate can check — the same shape as "the host chooses where the file
//! lives". Concretely, on Apple platforms (PLAT-MAC-003):
//!
//! * The **containing app** runs this at startup, before it starts claiming
//!   transfers. TDLib cannot live in a File Provider extension
//!   (`.spec/architecture.md`), so neither can the engine that drives it —
//!   which means at app startup no engine exists anywhere and the pass is
//!   sound.
//! * The **extension** never runs it. It claims nothing and materializes
//!   nothing, so it has nothing to reconcile and no standing to repair the
//!   app's state underneath it.
//! * A **user-triggered** repair (TASK-260715-1nuhxj) quiesces the engine
//!   first, for the same reason it would unmount a volume before checking
//!   it.
//!
//! This precondition is what makes a `running` transfer legible. The row is
//! otherwise ambiguous — a dead claim and a live one look identical, and
//! this crate has no liveness primitive to tell them apart. With the
//! precondition, no claim can be live, so every `running` row is a dead one.
//!
//! # It never talks to Telegram
//!
//! Structurally, not by discipline: the architecture forbids this crate
//! depending on `gramdrive-source` (`crates/README.md`), and [`reconcile`]
//! takes a [`LocalStorage`] and nothing else. There is no source handle here
//! to misuse — which is the whole of the SYNC-071 "without changing Telegram
//! data" guarantee.
//!
//! [`reconcile`]: StateStore::reconcile
//!
//! # It never loses pin intent
//!
//! A cache entry whose bytes vanished is *dropped*, because the row claims
//! bytes that do not exist. Its `pins` row is not: POL-2 intent exists
//! before hydration and survives eviction of everything else, so dropping
//! the materialized row leaves "the user wants this offline" intact and the
//! engine re-hydrates it. Reconciliation reclaims space it can prove is
//! waste; it never decides what the user wants kept.
//!
//! # Two entrypoints, one survey
//!
//! [`StateStore::plan_reconcile`] answers "what is wrong" without writing;
//! [`StateStore::reconcile`] answers it and repairs. The plan is the dry-run
//! the user-triggered repair entrypoint (TASK-260715-1nuhxj) presents before
//! it commits to anything.

use std::collections::{HashMap, HashSet};

use rusqlite::params;

use gramdrive_model::identity::ItemId;

use crate::error::StateError;
use crate::repair::{self, RepairKind};
use crate::repo::{ReadTxn, TransferId};
use crate::store::StateStore;

/// One object the host holds on behalf of this database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    /// The opaque handle the database knows it by — a
    /// `cache_entries.materialization_ref` or a `transfers.temp_ref`. This
    /// crate never interprets it, only matches it.
    pub reference: String,
    /// Bytes the object occupies, for reporting what a repair reclaims.
    pub size: u64,
}

/// Why a [`LocalStorage`] call failed.
///
/// Deliberately opaque: the reason a host could not list or delete an object
/// is the host's vocabulary (a path, an errno, a provider handle), and this
/// crate would only be able to reprint it. It carries the host's own text so
/// an unresolved finding can say precisely what stopped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageError {
    /// The host's description of the failure.
    pub detail: String,
}

impl StorageError {
    /// A failure described by `detail`.
    pub fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for StorageError {}

/// The host's local storage for one database: the materialized cache
/// objects and the transfer staging areas.
///
/// The port exists because the database location — and therefore every path
/// under it — is the embedding host's decision, and this crate is
/// platform-neutral by architecture rule. The host knows where the App Group
/// container is; this crate knows which handles the database still claims.
/// Reconciliation is the join of those two facts.
///
/// Both inventories are keyed by the opaque handles already in the schema,
/// so a host implementation is a directory listing plus whatever mapping it
/// chose when it wrote the handle in the first place.
pub trait LocalStorage: std::fmt::Debug {
    /// Every materialized cache object, by `materialization_ref`.
    fn cache_objects(&self) -> Result<Vec<StoredObject>, StorageError>;

    /// Every transfer staging area, by `temp_ref`.
    fn staging_objects(&self) -> Result<Vec<StoredObject>, StorageError>;

    /// Deletes one materialized cache object. Deleting one that is already
    /// gone is success — the caller wanted it gone, and it is.
    fn remove_cache_object(&self, reference: &str) -> Result<(), StorageError>;

    /// Deletes one transfer staging area, with the same tolerance.
    fn remove_staging_object(&self, reference: &str) -> Result<(), StorageError>;
}

/// One thing reconciliation found wrong.
///
/// Every variant is derived from durable evidence — a row that says
/// something the disk contradicts, or the other way round. Nothing here is a
/// suspicion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finding {
    /// A transfer is `running` and — per the module's precondition, which
    /// says no engine is live — nobody is running it: the process that
    /// claimed it died.
    ///
    /// Repaired by returning it to the queue with its progress intact — the
    /// staged ranges are durable and re-fetching them would be the actual
    /// data loss here.
    InterruptedTransfer {
        /// The journal row.
        transfer: TransferId,
        /// The item it hydrates.
        item: ItemId,
    },
    /// A staging area exists that no live transfer claims — the transfer it
    /// belonged to is terminal, or never recorded the handle.
    ///
    /// Repaired by deleting the object and clearing the stale `temp_ref`
    /// from any terminal row that still names it.
    LeakedStaging {
        /// The staging handle.
        reference: String,
        /// Bytes the deletion reclaims.
        size: u64,
    },
    /// A `cache_entries` row names bytes the host does not have: the OS
    /// purged them, a volume went away, or a previous run died between
    /// writing the row and writing the file (SYNC-053).
    ///
    /// Repaired by dropping the row — never the pin.
    MissingCacheObject {
        /// The item whose materialization is gone.
        item: ItemId,
        /// The handle the row named.
        reference: String,
        /// Whether the entry was pinned. Reported because it is the
        /// difference between "reclaimed some cache" and "the user's offline
        /// copy needs re-hydrating"; the pin itself survives either way.
        pinned: bool,
    },
    /// A `cache_entries` row carries no `materialization_ref`, so there is
    /// nothing to check it against.
    ///
    /// Reported, not repaired: an entry whose handle was never recorded is
    /// either a host that does not use handles or a bug in the code that
    /// wrote it, and dropping user-visible cache state on that ambiguity is
    /// not reconciliation's call.
    UnlocatableCacheEntry {
        /// The item with the unlocatable entry.
        item: ItemId,
    },
    /// The host holds a cache object no row claims: a previous run died
    /// between writing the file and committing the row, or after dropping
    /// the row and before deleting the file.
    ///
    /// Repaired by deleting the object. Safe because the database is the
    /// authority on what is cached — an object no row names can never be
    /// served, so it is waste by definition.
    OrphanCacheObject {
        /// The handle the host holds.
        reference: String,
        /// Bytes the deletion reclaims.
        size: u64,
    },
    /// A `rebuild_projection` marker is open: a projection no longer matches
    /// the canonical tables (SYNC-071).
    ///
    /// Reported, not repaired, and the marker stays raised. Rebuilding
    /// `items` from the canonical tables means running the projection
    /// builder, which is engine-side vocabulary this crate does not have.
    /// Leaving the marker up is the honest answer: the work is still owed.
    ProjectionRebuildPending {
        /// What the marker names.
        detail: String,
    },
    /// A `migration_interrupted` marker is open: a resumable migration has
    /// an uncommitted tail (SYNC-072).
    ///
    /// Reported, not repaired. [`StateStore::open`] is what resumes a
    /// migration, and it does so before any of this runs; a marker still up
    /// here means the file spent time mid-upgrade, which is history worth
    /// surfacing, not damage to fix.
    MigrationInterrupted {
        /// The migration the marker names.
        detail: String,
    },
}

/// What a repair pass would do about a [`Finding`].
///
/// Lets a caller present or filter a plan without matching every variant —
/// the dry-run of TASK-260715-1nuhxj is exactly "show me the plan, grouped
/// by what it will do".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Return the transfer to the queue, keeping its staged progress.
    RequeueTransfer,
    /// Delete the staging object and clear the stale handle.
    DropStaging,
    /// Drop the cache entry; the pin, if any, survives.
    DropCacheEntry,
    /// Delete the unclaimed object from local storage.
    DeleteObject,
    /// Nothing automatic; the finding is for a human or another subsystem.
    ReportOnly,
}

impl Finding {
    /// What [`StateStore::reconcile`] will do about this finding.
    pub fn resolution(&self) -> Resolution {
        match self {
            Self::InterruptedTransfer { .. } => Resolution::RequeueTransfer,
            Self::LeakedStaging { .. } => Resolution::DropStaging,
            Self::MissingCacheObject { .. } => Resolution::DropCacheEntry,
            Self::OrphanCacheObject { .. } => Resolution::DeleteObject,
            Self::UnlocatableCacheEntry { .. }
            | Self::ProjectionRebuildPending { .. }
            | Self::MigrationInterrupted { .. } => Resolution::ReportOnly,
        }
    }
}

/// What reconciliation found, without having changed anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcilePlan {
    /// Whether the previous run left in-flight work behind.
    ///
    /// True when the file carries state only a live process should have: a
    /// claimed transfer, a staging area no transfer claims, or a migration
    /// with an uncommitted tail. Since the pass runs with no engine live,
    /// each of those is by definition a leftover — which is what makes this
    /// a fact and not an inference.
    pub dirty_shutdown: bool,
    /// Everything wrong, in survey order.
    pub findings: Vec<Finding>,
}

impl ReconcilePlan {
    /// Whether the pass found nothing to do — the normal answer.
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }
}

/// A finding a repair pass could not resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unresolved {
    /// What was found.
    pub finding: Finding,
    /// Why it is still there — a host storage failure, or a repair this
    /// crate does not own.
    pub reason: String,
}

/// What a repair pass found and what it managed to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    /// The survey the pass acted on.
    pub plan: ReconcilePlan,
    /// Findings the pass resolved.
    pub repaired: Vec<Finding>,
    /// Findings still open, each with the reason.
    ///
    /// A pass that cannot delete one object still repairs everything else:
    /// convergence is the point, and a host failure on one handle is not a
    /// reason to leave the rest of the file inconsistent.
    pub unresolved: Vec<Unresolved>,
}

impl ReconcileReport {
    /// Whether the file is fully reconciled — nothing was found, or
    /// everything found was repaired.
    pub fn converged(&self) -> bool {
        self.unresolved.is_empty()
    }
}

impl StateStore {
    /// Surveys the database against local storage and reports what is wrong,
    /// without writing (SYNC-070).
    ///
    /// The dry-run half of the repair entrypoint: same survey as
    /// [`StateStore::reconcile`], same findings, no changes. It carries the
    /// module's precondition too — a survey taken while an engine is live
    /// reports that engine's work in progress as damage.
    pub fn plan_reconcile(
        &mut self,
        storage: &dyn LocalStorage,
    ) -> Result<ReconcilePlan, StateError> {
        let cache_objects = storage.cache_objects().map_err(inventory_failed)?;
        let staging_objects = storage.staging_objects().map_err(inventory_failed)?;
        let read = self.read_txn()?;
        survey(&read, &cache_objects, &staging_objects)
    }

    /// The startup pass: survey, then repair what is unambiguous
    /// (SYNC-070, NFR-034).
    ///
    /// Run it at startup, before this process starts any engine work — see
    /// the module docs for why that is the precondition and not a
    /// suggestion. `now_ms` stamps the rows a repair touches; this crate
    /// reads no clock (SYNC-073).
    ///
    /// Idempotent and convergent, which is what makes it safe to run on
    /// every open: a second pass over a reconciled file finds nothing, and a
    /// pass interrupted halfway leaves each repair either fully applied or
    /// not applied at all — the next one picks up the rest. Repairs run in
    /// short write transactions rather than one long one, so the other
    /// process is never locked out for the length of a scan.
    pub fn reconcile(
        &mut self,
        storage: &dyn LocalStorage,
        now_ms: i64,
    ) -> Result<ReconcileReport, StateError> {
        let plan = self.plan_reconcile(storage)?;
        let mut repaired = Vec::new();
        let mut unresolved = Vec::new();

        for finding in &plan.findings {
            match apply(self, storage, finding, now_ms) {
                Ok(true) => repaired.push(finding.clone()),
                Ok(false) => unresolved.push(Unresolved {
                    finding: finding.clone(),
                    reason: report_only_reason(finding).to_owned(),
                }),
                Err(RepairFailure::Storage(error)) => unresolved.push(Unresolved {
                    finding: finding.clone(),
                    reason: format!("local storage refused: {error}"),
                }),
                Err(RepairFailure::State(error)) => return Err(error),
            }
        }

        Ok(ReconcileReport {
            plan,
            repaired,
            unresolved,
        })
    }
}

/// Why a `ReportOnly` finding is still open. Not a failure — the work is
/// owed by someone else, and saying so precisely is the point (NFR-034).
fn report_only_reason(finding: &Finding) -> &'static str {
    match finding {
        Finding::UnlocatableCacheEntry { .. } => {
            "the entry records no materialization handle; nothing to check it against"
        }
        Finding::ProjectionRebuildPending { .. } => {
            "rebuilding the projection needs the projection builder, which is engine-side"
        }
        Finding::MigrationInterrupted { .. } => "a migration resumes on open, not here",
        _ => "no automatic repair",
    }
}

/// A storage listing this crate cannot work around: without the inventory
/// there is no survey, and a survey against a *partial* inventory would
/// delete live cache as orphaned.
fn inventory_failed(error: StorageError) -> StateError {
    StateError::LocalStorage {
        detail: error.detail,
    }
}

/// A repair that did not happen. Storage failures are per-finding and become
/// `Unresolved`; state failures are the database itself going wrong, which
/// no amount of continuing would improve.
enum RepairFailure {
    Storage(StorageError),
    State(StateError),
}

impl From<StateError> for RepairFailure {
    fn from(error: StateError) -> Self {
        Self::State(error)
    }
}

impl From<StorageError> for RepairFailure {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

/// Reads the whole survey from one snapshot, so every finding describes the
/// same database state.
fn survey(
    read: &ReadTxn<'_>,
    cache_objects: &[StoredObject],
    staging_objects: &[StoredObject],
) -> Result<ReconcilePlan, StateError> {
    let mut findings = Vec::new();
    let mut dirty_shutdown = false;

    // Transfers the previous run was moving bytes for. No engine is live
    // (module precondition), so a claim is a dead one.
    for (transfer, item) in running_transfers(read)? {
        dirty_shutdown = true;
        findings.push(Finding::InterruptedTransfer { transfer, item });
    }

    // A live transfer's staging area is *not* garbage: the requeued transfer
    // resumes from exactly those bytes, so the handles the journal still
    // claims are protected and only the rest are leaks.
    let live_staging = live_staging_refs(read)?;
    for object in staging_objects {
        if !live_staging.contains(&object.reference) {
            dirty_shutdown = true;
            findings.push(Finding::LeakedStaging {
                reference: object.reference.clone(),
                size: object.size,
            });
        }
    }

    let held: HashMap<&str, &StoredObject> = cache_objects
        .iter()
        .map(|object| (object.reference.as_str(), object))
        .collect();
    let mut claimed: HashSet<String> = HashSet::new();
    for entry in cache_entry_refs(read)? {
        match entry.reference {
            None => findings.push(Finding::UnlocatableCacheEntry { item: entry.item }),
            Some(reference) => {
                if held.contains_key(reference.as_str()) {
                    claimed.insert(reference);
                } else {
                    findings.push(Finding::MissingCacheObject {
                        item: entry.item,
                        reference,
                        pinned: entry.pinned,
                    });
                }
            }
        }
    }
    for object in cache_objects {
        if !claimed.contains(&object.reference) {
            findings.push(Finding::OrphanCacheObject {
                reference: object.reference.clone(),
                size: object.size,
            });
        }
    }

    for marker in repair::list(read.conn())? {
        match marker.kind {
            RepairKind::RebuildProjection => findings.push(Finding::ProjectionRebuildPending {
                detail: marker.detail,
            }),
            RepairKind::MigrationInterrupted => {
                dirty_shutdown = true;
                findings.push(Finding::MigrationInterrupted {
                    detail: marker.detail,
                });
            }
        }
    }

    Ok(ReconcilePlan {
        dirty_shutdown,
        findings,
    })
}

/// Applies one repair in its own short transaction. `Ok(false)` means the
/// finding is report-only.
fn apply(
    store: &mut StateStore,
    storage: &dyn LocalStorage,
    finding: &Finding,
    now_ms: i64,
) -> Result<bool, RepairFailure> {
    match finding {
        Finding::InterruptedTransfer { transfer, .. } => {
            let tx = store.write_txn()?;
            // Re-checked inside the write transaction rather than trusted
            // from the survey: the row is only requeued if it is still the
            // `running` row the survey saw. completed_ranges, temp_ref and
            // retry_count are untouched on purpose — the staged bytes are
            // durable and a crash is not a failed attempt (SYNC-044).
            tx.conn()
                .execute(
                    "UPDATE transfers SET state = 'queued', updated_at_ms = ?2
                     WHERE transfer_id = ?1 AND state = 'running'",
                    params![transfer.0, now_ms],
                )
                .map_err(StateError::from)?;
            tx.commit()?;
            Ok(true)
        }
        Finding::LeakedStaging { reference, .. } => {
            storage.remove_staging_object(reference)?;
            let tx = store.write_txn()?;
            // Only off rows that are terminal: a live transfer's handle was
            // never a candidate for deletion, so it cannot be cleared here
            // either.
            tx.conn()
                .execute(
                    "UPDATE transfers SET temp_ref = NULL, updated_at_ms = ?2
                     WHERE temp_ref = ?1
                       AND state NOT IN ('queued', 'running', 'suspended')",
                    params![reference, now_ms],
                )
                .map_err(StateError::from)?;
            tx.commit()?;
            Ok(true)
        }
        Finding::MissingCacheObject { item, .. } => {
            let tx = store.write_txn()?;
            // remove_cache_entry, not evict_cache_entry: this is not an
            // eviction decision (POL-2 eligibility would refuse a pinned or
            // unverified row) but the recording of a fact — the bytes are
            // already gone. The `pins` row is untouched, so the user's
            // offline intent survives and the engine re-hydrates it.
            tx.remove_cache_entry(item)?;
            // A generated document whose published bytes vanished has to be
            // re-rendered, so it goes back on the worklist (SYNC-024). A
            // non-document item has no render state and nothing to mark.
            if tx.read().render_state(item)?.is_some() {
                tx.mark_render_dirty(item)?;
            }
            tx.commit()?;
            Ok(true)
        }
        Finding::OrphanCacheObject { reference, .. } => {
            storage.remove_cache_object(reference)?;
            Ok(true)
        }
        Finding::UnlocatableCacheEntry { .. }
        | Finding::ProjectionRebuildPending { .. }
        | Finding::MigrationInterrupted { .. } => Ok(false),
    }
}

fn running_transfers(read: &ReadTxn<'_>) -> Result<Vec<(TransferId, ItemId)>, StateError> {
    let mut statement = read
        .conn()
        .prepare_cached("SELECT transfer_id, item_id FROM transfers WHERE state = 'running'")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut transfers = Vec::new();
    for row in rows {
        let (id, item) = row?;
        transfers.push((
            TransferId(id),
            crate::repo::item_id_from_column("transfers", &item)?,
        ));
    }
    Ok(transfers)
}

fn live_staging_refs(read: &ReadTxn<'_>) -> Result<HashSet<String>, StateError> {
    let mut statement = read.conn().prepare_cached(
        "SELECT temp_ref FROM transfers
         WHERE temp_ref IS NOT NULL
           AND (state = 'queued' OR state = 'running' OR state = 'suspended')",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut refs = HashSet::new();
    for row in rows {
        refs.insert(row?);
    }
    Ok(refs)
}

struct CacheEntryRef {
    item: ItemId,
    reference: Option<String>,
    pinned: bool,
}

fn cache_entry_refs(read: &ReadTxn<'_>) -> Result<Vec<CacheEntryRef>, StateError> {
    let mut statement = read
        .conn()
        .prepare_cached("SELECT item_id, materialization_ref, pinned FROM cache_entries")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, bool>(2)?,
        ))
    })?;
    let mut entries = Vec::new();
    for row in rows {
        let (item, reference, pinned) = row?;
        entries.push(CacheEntryRef {
            item: crate::repo::item_id_from_column("cache_entries", &item)?,
            reference,
            pinned,
        });
    }
    Ok(entries)
}
