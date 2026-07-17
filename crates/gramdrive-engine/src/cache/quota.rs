//! Cache accounting, quota enforcement, and LRU eviction (TASK-260715-11abx8;
//! POL-2, SYNC-050..054). Module-level rationale is in [`super`].

use std::collections::HashSet;

use gramdrive_model::identity::ItemId;
use gramdrive_state::LocalStorage;
use gramdrive_state::StateStore;
use gramdrive_state::repo::{CacheKind, CacheTotals, EvictionCandidate, ReadTxn};

use crate::transfer::EngineError;

/// The POL-2 default device cache quota: 10 GiB. Binary GiB, matching the
/// byte-size convention the rest of the engine uses; every deployment may
/// override it ([`QuotaPolicy::limit_bytes`]).
pub const DEFAULT_QUOTA_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// How many eviction candidates one keyset page holds. The frontier walk
/// pages through the LRU order rather than loading the whole working set, so
/// a reclaim costs a bounded transient allocation — the core runs inside a
/// File Provider extension under a tight heap cap.
const EVICTION_PAGE: u32 = 256;

/// How many evictions share one write transaction. Small enough to keep the
/// lock brief (the crate's short-transaction discipline), large enough that a
/// bulk drain is not one transaction per row.
const EVICTION_BATCH: usize = 64;

/// The device cache budget (POL-2).
///
/// Pure policy the host owns and this engine consumes; the *value's*
/// durability is the embedding app's device configuration, while the durable
/// consequence of a quota — which bytes are dropped — is the `cache_entries`
/// rows the eviction commits. SYNC-054's "immediately produce an actionable
/// status" is [`Evictor::assess`], which turns any quota (old or newly
/// changed) into a concrete plan/status without touching a byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaPolicy {
    /// The ceiling unpinned cache bytes are kept under. Pinned and
    /// Archive-Mode bytes are quota-exempt (POL-2): counted and surfaced,
    /// but never measured against this limit and never evicted.
    pub limit_bytes: u64,
}

impl Default for QuotaPolicy {
    fn default() -> Self {
        Self {
            limit_bytes: DEFAULT_QUOTA_BYTES,
        }
    }
}

/// The SYNC-050 accounting breakdown of device cache use: each category
/// counted separately, plus the pin/verification splits a quota decision
/// reads. Byte figures are `cache_entries` row sizes summed device-wide (the
/// on-disk cache is one budget), except `partial_transfer_bytes`, which is
/// staged transfer bytes not yet promoted to cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheAccounting {
    /// Materialized attachment-blob bytes.
    pub blob_bytes: u64,
    /// Materialized generated-document bytes.
    pub generated_doc_bytes: u64,
    /// Materialized thumbnail bytes (POL-2: thumbnails are always eager).
    pub thumbnail_bytes: u64,
    /// Bytes staged by live transfers — partial content that is not cache
    /// and is reclaimed by cancellation, never by eviction (SYNC-050).
    pub partial_transfer_bytes: u64,
    /// Total materialized cache bytes: `blob + generated_doc + thumbnail`,
    /// equivalently `pinned + unpinned`.
    pub total_cache_bytes: u64,
    /// Bytes an explicit pin or Archive-Mode coverage holds — quota-exempt
    /// but counted (POL-2).
    pub pinned_bytes: u64,
    /// Bytes no pin protects — the figure the quota is measured against.
    pub unpinned_bytes: u64,
    /// Bytes eviction can reclaim right now: unpinned *and* verified
    /// (SYNC-052). A subset of `unpinned_bytes`.
    pub evictable_bytes: u64,
}

/// The actionable quota status (SYNC-054): where the unpinned working set
/// sits against a limit, and how much of any overage eviction can actually
/// clear. Produced without dropping a byte, so a quota change surfaces its
/// consequence before it is enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaAssessment {
    /// The limit this status was computed against.
    pub limit_bytes: u64,
    /// Unpinned bytes — what the limit governs.
    pub unpinned_bytes: u64,
    /// Pinned/Archive-Mode bytes — exempt, reported for the app's usage view.
    pub pinned_bytes: u64,
    /// How far unpinned use exceeds the limit; `0` when within budget.
    pub over_by: u64,
    /// Of `over_by`, how much eviction can reclaim now — eligible (verified,
    /// unpinned) content that is neither being read nor hydrated.
    pub reclaimable_bytes: u64,
    /// Overage that would remain after reclaiming everything eligible: bytes
    /// locked by an open read, a live transfer, or awaiting verification.
    /// Non-zero is the honest "over quota, cannot fully reclaim" state —
    /// surfaced, never silent data loss.
    pub residual_bytes: u64,
}

impl QuotaAssessment {
    /// Whether unpinned use is within the limit.
    #[must_use]
    pub fn within_quota(&self) -> bool {
        self.over_by == 0
    }

    /// Whether enforcing would bring use back within the limit (no residual).
    #[must_use]
    pub fn fully_reclaimable(&self) -> bool {
        self.residual_bytes == 0
    }
}

/// A read-only preview of what an eviction would do (POL-2). Deterministic:
/// the victims are the eligible LRU frontier, oldest access first, computed
/// against one read snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use = "a plan describes work; call enforce/reclaim to apply it"]
pub struct EvictionPlan {
    /// The entries eviction would drop, oldest access first. Empty when
    /// within budget or nothing is eligible.
    pub victims: Vec<EvictionCandidate>,
    /// Accounting bytes the victims would reclaim (sum of their row sizes) —
    /// what the quota sees go down.
    pub reclaimable_bytes: u64,
    /// The quota status this plan resolves.
    pub assessment: QuotaAssessment,
}

/// The outcome of an executed eviction (POL-2, SYNC-052/053).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionReport {
    /// Items whose cache rows were dropped.
    pub evicted: Vec<ItemId>,
    /// On-disk objects deleted — only those no surviving entry still
    /// references (content-addressed dedup, SYNC-052).
    pub objects_deleted: Vec<String>,
    /// Physical disk bytes freed: the sizes of the deleted objects. Under
    /// dedup this is below the evicted rows' total, because a shared object
    /// survives until its last referrer is gone.
    pub reclaimed_bytes: u64,
    /// Candidates the plan named but that became ineligible before the delete
    /// — newly pinned, a read opened, or a transfer started. Re-read, never
    /// assumed evicted (SYNC-051).
    pub skipped: Vec<ItemId>,
    /// The quota status after the eviction ran.
    pub assessment: QuotaAssessment,
}

/// Which items the host is actively using, so eviction never races them
/// (SYNC-043/052). The engine cannot see OS file handles; the host that owns
/// open reads supplies the set. A live transfer is detected from durable
/// state and needs no declaration here.
#[derive(Debug, Clone, Default)]
pub struct EvictionRequest {
    /// Items with an open read the host must not have deleted underneath it.
    pub protected: HashSet<ItemId>,
}

impl EvictionRequest {
    /// A request protecting nothing — the common case when no reads are open.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// A request protecting exactly `items`.
    #[must_use]
    pub fn protecting(items: impl IntoIterator<Item = ItemId>) -> Self {
        Self {
            protected: items.into_iter().collect(),
        }
    }
}

/// Cache accounting, quota enforcement, and eviction over a [`StateStore`].
///
/// Stateless over the store, like [`TransferMachine`] and [`Promoter`]: the
/// durable `cache_entries` rows and the host's on-disk objects are the only
/// authority, so a host constructs one per policy and passes the store into
/// each call. Eviction eligibility (unpinned, verified) is enforced *in the
/// state layer's delete statement*, so a stale plan can never remove pinned
/// or in-flight content no matter what this layer believes.
///
/// [`TransferMachine`]: crate::transfer::TransferMachine
/// [`Promoter`]: crate::cache::Promoter
#[derive(Debug, Clone, Copy, Default)]
pub struct Evictor {
    policy: QuotaPolicy,
}

impl Evictor {
    /// An evictor applying `policy`.
    #[must_use]
    pub fn new(policy: QuotaPolicy) -> Self {
        Self { policy }
    }

    /// An evictor with an explicit byte limit.
    #[must_use]
    pub fn with_limit(limit_bytes: u64) -> Self {
        Self::new(QuotaPolicy { limit_bytes })
    }

    /// The quota this evictor enforces.
    #[must_use]
    pub fn policy(&self) -> QuotaPolicy {
        self.policy
    }

    /// The SYNC-050 accounting breakdown of device cache use.
    pub fn accounting(&self, store: &mut StateStore) -> Result<CacheAccounting, EngineError> {
        let read = store.read_txn()?;
        let totals = read.cache_totals()?;
        let partial = read.staged_transfer_bytes()?;
        let mut blob = 0;
        let mut generated_doc = 0;
        let mut thumbnail = 0;
        for usage in read.cache_usage_by_kind()? {
            match usage.kind {
                CacheKind::Blob => blob = usage.total_bytes,
                CacheKind::GeneratedDoc => generated_doc = usage.total_bytes,
                CacheKind::Thumbnail => thumbnail = usage.total_bytes,
            }
        }
        Ok(CacheAccounting {
            blob_bytes: blob,
            generated_doc_bytes: generated_doc,
            thumbnail_bytes: thumbnail,
            partial_transfer_bytes: partial,
            total_cache_bytes: totals.total_bytes,
            pinned_bytes: totals.pinned_bytes,
            unpinned_bytes: totals.unpinned_bytes,
            evictable_bytes: totals.evictable_bytes,
        })
    }

    /// The actionable quota status (SYNC-054) — within budget, or over by a
    /// concrete amount with the reclaimable and residual split. Reads only.
    pub fn assess(
        &self,
        store: &mut StateStore,
        request: &EvictionRequest,
    ) -> Result<QuotaAssessment, EngineError> {
        Ok(self.plan(store, request)?.assessment)
    }

    /// The deterministic plan to bring unpinned use within the quota, without
    /// applying it (POL-2). The victims are the eligible LRU frontier.
    pub fn plan(
        &self,
        store: &mut StateStore,
        request: &EvictionRequest,
    ) -> Result<EvictionPlan, EngineError> {
        let read = store.read_txn()?;
        let totals = read.cache_totals()?;
        let over_by = totals
            .unpinned_bytes
            .saturating_sub(self.policy.limit_bytes);
        let (victims, reclaimable) = walk(&read, over_by, request)?;
        let assessment = assessment(&totals, self.policy.limit_bytes, over_by, reclaimable);
        Ok(EvictionPlan {
            victims,
            reclaimable_bytes: reclaimable,
            assessment,
        })
    }

    /// Enforces the quota: drops the eligible LRU frontier until unpinned use
    /// is within the limit, then deletes every on-disk object no surviving
    /// entry still references (POL-2, SYNC-052). Idempotent — running it
    /// within budget evicts nothing.
    pub fn enforce(
        &self,
        store: &mut StateStore,
        storage: &dyn LocalStorage,
        request: &EvictionRequest,
    ) -> Result<EvictionReport, EngineError> {
        let victims = self.plan(store, request)?.victims;
        self.execute(store, storage, &victims, request)
    }

    /// Reclaims at least `target_bytes` of eligible cache for storage
    /// pressure (POL-2 low-disk, SYNC-044 disk-full): evicts the LRU frontier
    /// regardless of the quota, since the disk can be full while under quota.
    /// Under dedup the physical bytes freed may fall short of the target — a
    /// shared object survives its non-last referrers — and the caller retries
    /// with the remaining deficit.
    pub fn reclaim(
        &self,
        store: &mut StateStore,
        storage: &dyn LocalStorage,
        target_bytes: u64,
        request: &EvictionRequest,
    ) -> Result<EvictionReport, EngineError> {
        let victims = {
            let read = store.read_txn()?;
            walk(&read, target_bytes, request)?.0
        };
        self.execute(store, storage, &victims, request)
    }

    /// Applies a victim list: drops rows in bounded batches (short
    /// transactions), then deletes each object no surviving entry references.
    ///
    /// Ordering is row-before-file: the `cache_entries` row is dropped and
    /// committed before its object is deleted, so a crash in the window
    /// leaves an object no row claims — reconciliation's `OrphanCacheObject`
    /// reclaims it (SYNC-053). Every victim is re-validated at execution: a
    /// candidate newly pinned, read, or hydrating is skipped, and the delete
    /// itself refuses ineligible rows (SYNC-051).
    fn execute(
        &self,
        store: &mut StateStore,
        storage: &dyn LocalStorage,
        victims: &[EvictionCandidate],
        request: &EvictionRequest,
    ) -> Result<EvictionReport, EngineError> {
        let mut evicted = Vec::new();
        let mut skipped = Vec::new();
        let mut objects_deleted = Vec::new();
        let mut reclaimed_bytes = 0u64;

        for batch in victims.chunks(EVICTION_BATCH) {
            // One transaction drops the batch's eligible rows and captures the
            // object each named.
            let mut dropped: Vec<(ItemId, u64, Option<String>)> = Vec::new();
            {
                let tx = store.write_txn()?;
                for candidate in batch {
                    if blocked(tx.read(), request, &candidate.item)? {
                        skipped.push(candidate.item.clone());
                        continue;
                    }
                    let reference = tx
                        .read()
                        .cache_entry(&candidate.item)?
                        .and_then(|entry| entry.materialization_ref);
                    if tx.evict_cache_entry(&candidate.item)? {
                        dropped.push((candidate.item.clone(), candidate.size, reference));
                    } else {
                        skipped.push(candidate.item.clone());
                    }
                }
                tx.commit()?;
            }

            // After the rows are gone, delete each distinct object no entry
            // still references. One ref is checked once per batch.
            let mut checked: HashSet<String> = HashSet::new();
            for (item, size, reference) in dropped {
                evicted.push(item);
                let Some(reference) = reference else {
                    continue;
                };
                if !checked.insert(reference.clone()) {
                    continue;
                }
                let still_referenced = {
                    let read = store.read_txn()?;
                    read.materialization_ref_referenced(&reference)?
                };
                if !still_referenced {
                    storage.remove_cache_object(&reference).map_err(|error| {
                        EngineError::Storage {
                            detail: error.detail,
                        }
                    })?;
                    reclaimed_bytes = reclaimed_bytes.saturating_add(size);
                    objects_deleted.push(reference);
                }
            }
        }

        let assessment = self.assess(store, request)?;
        Ok(EvictionReport {
            evicted,
            objects_deleted,
            reclaimed_bytes,
            skipped,
            assessment,
        })
    }
}

/// Walks the eligible LRU frontier over one read snapshot, accumulating
/// victims until their sizes reach `target_free` or the frontier is
/// exhausted (POL-2). Candidates a read or a live transfer holds are skipped
/// and the keyset cursor advances past them, so a page full of in-use rows
/// never stalls the walk.
fn walk(
    read: &ReadTxn<'_>,
    target_free: u64,
    request: &EvictionRequest,
) -> Result<(Vec<EvictionCandidate>, u64), EngineError> {
    let mut victims = Vec::new();
    let mut reclaimable = 0u64;
    if target_free == 0 {
        return Ok((victims, reclaimable));
    }
    let mut cursor: Option<(i64, ItemId)> = None;
    loop {
        let page = read.eviction_candidates_after(
            cursor.as_ref().map(|(access, item)| (*access, item)),
            EVICTION_PAGE,
        )?;
        if page.is_empty() {
            break;
        }
        let full_page = page.len() == EVICTION_PAGE as usize;
        for candidate in page {
            cursor = Some((candidate.last_access_at_ms, candidate.item.clone()));
            if blocked(read, request, &candidate.item)? {
                continue;
            }
            reclaimable = reclaimable.saturating_add(candidate.size);
            victims.push(candidate);
            if reclaimable >= target_free {
                return Ok((victims, reclaimable));
            }
        }
        if !full_page {
            break;
        }
    }
    Ok((victims, reclaimable))
}

/// Whether a candidate must be left alone this pass: the host holds an open
/// read of it, or a live transfer is hydrating it (SYNC-043/052).
fn blocked(
    read: &ReadTxn<'_>,
    request: &EvictionRequest,
    item: &ItemId,
) -> Result<bool, EngineError> {
    if request.protected.contains(item) {
        return Ok(true);
    }
    Ok(read.has_live_transfer(item)?)
}

/// Builds the quota status from totals and the walk's reclaimable figure.
fn assessment(
    totals: &CacheTotals,
    limit_bytes: u64,
    over_by: u64,
    reclaimable: u64,
) -> QuotaAssessment {
    QuotaAssessment {
        limit_bytes,
        unpinned_bytes: totals.unpinned_bytes,
        pinned_bytes: totals.pinned_bytes,
        over_by,
        reclaimable_bytes: reclaimable.min(over_by),
        residual_bytes: over_by.saturating_sub(reclaimable),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    fn totals(pinned: u64, unpinned: u64, evictable: u64) -> CacheTotals {
        CacheTotals {
            total_bytes: pinned + unpinned,
            pinned_bytes: pinned,
            unpinned_bytes: unpinned,
            evictable_bytes: evictable,
        }
    }

    #[test]
    fn default_quota_is_ten_binary_gibibytes() {
        assert_eq!(DEFAULT_QUOTA_BYTES, 10 * 1024 * 1024 * 1024);
        assert_eq!(QuotaPolicy::default().limit_bytes, DEFAULT_QUOTA_BYTES);
        assert_eq!(Evictor::default().policy().limit_bytes, DEFAULT_QUOTA_BYTES);
    }

    #[test]
    fn within_quota_reports_no_overage() {
        // Unpinned under the limit; pinned bytes are exempt (POL-2).
        let status = assessment(&totals(5_000, 100, 100), 200, 0, 0);
        assert!(status.within_quota());
        assert!(status.fully_reclaimable());
        assert_eq!(status.over_by, 0);
        assert_eq!(status.residual_bytes, 0);
    }

    #[test]
    fn over_quota_splits_reclaimable_from_locked_residual() {
        // 400 unpinned, limit 150 → over by 250; the walk could free 300 of
        // eligible bytes, so the overage fully clears (reclaimable clamped to
        // the overage, residual zero).
        let status = assessment(&totals(0, 400, 300), 150, 250, 300);
        assert_eq!(status.over_by, 250);
        assert_eq!(status.reclaimable_bytes, 250);
        assert_eq!(status.residual_bytes, 0);
        assert!(!status.within_quota());
        assert!(status.fully_reclaimable());

        // Only 100 eligible against a 250 overage leaves 150 locked.
        let status = assessment(&totals(0, 400, 100), 150, 250, 100);
        assert_eq!(status.reclaimable_bytes, 100);
        assert_eq!(status.residual_bytes, 150);
        assert!(!status.fully_reclaimable());
    }

    #[test]
    fn eviction_request_constructors_build_the_protected_set() {
        use gramdrive_model::identity::{AccountId, AccountKey, CanonicalKey, ItemKey};

        assert!(EvictionRequest::none().protected.is_empty());
        let account = AccountKey {
            account_id: AccountId(1),
        };
        let id = ItemKey::Canonical(CanonicalKey::Account(account)).id();
        let request = EvictionRequest::protecting([id.clone()]);
        assert!(request.protected.contains(&id));
    }
}
