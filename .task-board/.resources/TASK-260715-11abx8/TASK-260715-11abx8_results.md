# TASK-260715-11abx8 — Cache quota, pinning, and eviction

Engine-layer cache accounting, quota enforcement, LRU eviction, and pin
orchestration over the durable `gramdrive-state` cache repo. POL-2,
SYNC-050..054, DEC-014.

## What landed

### `gramdrive-engine::cache` — `Evictor` (new `src/cache/quota.rs`)
Stateless over the store (like `TransferMachine`/`Promoter`); the host
constructs one per `QuotaPolicy` and passes the store into each call.

- **Accounting (SYNC-050).** `Evictor::accounting` → `CacheAccounting`:
  blob / generated-doc / thumbnail bytes counted separately, plus
  `partial_transfer_bytes` (staged bytes of live transfers, reclaimed by
  cancellation not eviction), the device totals, and the pinned / unpinned /
  evictable split. Device-wide (the on-disk cache is one budget even though
  blob identity is account-scoped).
- **Quota (POL-2).** `QuotaPolicy { limit_bytes }`, default `DEFAULT_QUOTA_BYTES`
  = 10 GiB, configurable. The quota governs **unpinned** bytes; pinned and
  Archive-Mode bytes are quota-exempt but counted (surfaced as `pinned_bytes`).
- **Actionable status (SYNC-054).** `Evictor::assess` → `QuotaAssessment`
  (`over_by`, `reclaimable_bytes`, `residual_bytes`) without dropping a byte,
  so a quota change surfaces its consequence immediately. A non-zero
  `residual_bytes` is the honest "over quota, cannot fully reclaim" state
  (overage locked by open reads, live transfers, or unverified content) — never
  silent data loss.
- **Eviction (SYNC-051/052).** `Evictor::enforce` drains the eligible LRU
  frontier until unpinned use is within the limit; `Evictor::reclaim(target)`
  frees a target for disk-full pressure regardless of the quota. Eligibility
  (unpinned + verified) is enforced in the state layer's delete statement, so a
  stale plan can never remove pinned or in-flight content.
- **No-race interlocks.** A candidate is skipped when the host declares an open
  read (`EvictionRequest::protected`) or a live transfer exists
  (`has_live_transfer`, durable). The keyset frontier walk advances its cursor
  past skipped rows, so a page full of in-use rows never stalls the drain.
- **Dedup-safe object deletion (SYNC-052/053).** An on-disk object is deleted
  only once no surviving cache entry references its `materialization_ref`.
  Ordering is **row-before-file**: the row is committed before the object is
  deleted, so a crash in the window leaves an object no row claims —
  reconciliation's `OrphanCacheObject` reclaims it. Reuses the existing
  `LocalStorage` host port (same one reconcile uses).

### `gramdrive-engine::cache::{pin, unpin}` (new `src/cache/pin.rs`)
Durable offline intent folded onto the materialized row in one transaction,
with **directional origin**: a user pin overwrites Archive-Mode coverage and
survives Archive Mode turning off; Archive-Mode coverage never downgrades a
user pin. Directional release too — archive teardown frees only archive pins.
Closes the `pin_item` blind-overwrite gap flagged in LOGBOOK #70 at the engine
layer (no state change).

### `gramdrive-state` — additive read queries (no schema change; v1 frozen)
- `ReadTxn::cache_totals` → `CacheTotals` (total / pinned / unpinned / evictable,
  one CASE-form aggregate).
- `ReadTxn::cache_usage_by_kind` (device-wide per-category, SYNC-050).
- `ReadTxn::eviction_candidates_after(cursor, limit)` — keyset LRU pagination
  over the partial `cache_entries_eviction` index (index-only, no temp sort).
- `ReadTxn::materialization_ref_referenced` — dedup refcount for object deletion.
- `ReadTxn::has_live_transfer` — eviction/transfer interlock (item-scoped index).
- `ReadTxn::staged_transfer_bytes` — partial-transfer accounting via `json_each`
  over live transfers (driven by the `transfers_queue` partial index).

## Key decisions

- **Quota value durability is the host's, not the core's.** SYNC-054's "durable"
  is satisfied by the durable *consequence* (the `cache_entries` rows eviction
  commits) plus an immediately-actionable `assess`. The quota *scalar* is
  device-global config; persisting it in the state DB would need a v2 migration
  (v1 `schema/v1.sql` is frozen, NFR-041) into another task's territory. Not a
  forced fit — a clean boundary. If product later wants core-owned quota
  persistence, that is an explicit v2 migration.
- **Unpinned-unverified bytes count toward the quota but are never evicted.**
  They occupy the budget (SYNC-050) but are ineligible (SYNC-052), so they can
  force eviction of verified content or produce an honest residual. Deliberate,
  tested (`unverified_unpinned_bytes_count_toward_the_quota_but_are_never_evicted`).
- **Accounting is row-size (quota) vs. physical-disk (report) distinct.** The
  quota sums `cache_entries.size` (so two dedup'd entries count twice); the
  report's `reclaimed_bytes` is physical (a shared object frees once, on its
  last referrer). Under dedup a disk-full `reclaim` may free less physical than
  targeted; the caller retries with the deficit.

## Media-cache-policy fixtures (POL-2)

The POL-2 policy fixtures are the integration suite
`crates/gramdrive-engine/tests/cache_quota.rs` (the repo's convention is
Rust-coded fixtures, not external JSON). Its `entry` helper seeds exactly what
`Promoter::promote` writes, so the scenarios are real promotion output. Cases:
accounting-splits-categories, LRU-eviction-eligible-only,
unverified-counts-not-evicted, within-quota-no-op, no-race-open-reads-and-live-
transfers, quota-shrink-actionable-drain, dedup-shared-object-last-referrer,
storage-pressure-reclaim, pin-user-wins-over-archive, directional-unpin, and
paging-past-a-protected-wall.

## Verification

`make check` → 8/8 (toolchain, format, lint, test, architecture, supply-chain,
traceability, scripts). No new dependencies (`cargo deny` unchanged).

- engine: 31 lib-unit + 11 `cache_quota` + 12 promotion + 17 fetch + 18 transfer.
- state: `repo_cache_render` +3 and `repo_transfers` +2 new tests.

Query plans (EXPLAIN, verified): `eviction_candidates_after` →
`SCAN cache_entries USING INDEX cache_entries_eviction` (no temp b-tree);
`has_live_transfer` → `SEARCH transfers USING INDEX transfers_by_item`;
`staged_transfer_bytes` → `SEARCH t USING INDEX transfers_queue` + json_each.
`cache_totals` / `cache_usage_by_kind` / `materialization_ref_referenced` are
bounded-table aggregates/probes (deliberately not added to the large-account
REQUIRED_QUERIES plan gate).

## Scope

- `crates/gramdrive-engine/src/cache/{mod,quota,pin}.rs`, `src/lib.rs`, `README.md`.
- `crates/gramdrive-engine/tests/cache_quota.rs` (new).
- `crates/gramdrive-state/src/repo/{cache,transfers,mod}.rs`
  (additive read queries + `CacheTotals`).
- `crates/gramdrive-state/tests/{repo_cache_render,repo_transfers}.rs`.

## Handoff notes (downstream: 3s461k macOS pin/eviction surface, u4x93s, 3lofcv)

- `Evictor::assess`/`accounting` are the app's usage/quota surface; `pinned_bytes`
  is the "counted but exempt" figure to display.
- The host passes `EvictionRequest::protected` (its open File-Provider reads);
  the engine adds the live-transfer interlock itself.
- Directory-pin subtree expansion and Archive-Mode scope walks are the caller's
  enumeration; call `cache::pin` per item (directional origin handles the rest).
- System/provider eviction stays reconciled by `StateStore::reconcile`.
