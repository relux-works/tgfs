# TASK-260715-11abx8 — Review verdict: ACCEPTED

Reviewer: reviewer (claude). Read-only review of `gramdrive-engine::cache`
(`Evictor`, `pin`/`unpin`) + additive `gramdrive-state` read queries.

## Verdict

**ACCEPTED → done.** Implementation matches the AC, fits the project
architecture, and all quality gates are green (`make check` re-run: 8/8).

## AC verification (read code + tests, re-ran gates)

1. **Cache accounting + LRU eviction of unpinned only; 10 GB default
   configurable; pinned/Archive-Mode exempt-but-counted.**
   - `CacheAccounting` splits blob/generated-doc/thumbnail + `partial_transfer_bytes`,
     with pinned/unpinned/evictable totals (`cache_totals` CASE-form aggregate).
     Verified by `accounting_separates_categories_and_splits_pins` and state
     `device_wide_totals_split_pins_and_verification`.
   - Eviction eligibility (`pinned = 0 AND verification = 'verified'`) is enforced
     **in the SQL DELETE itself** (`evict_cache_entry`), not just in the planner —
     a stale plan cannot remove pinned/in-flight content. LRU order via the
     partial `cache_entries_eviction` index + keyset pagination.
   - `QuotaPolicy` default `DEFAULT_QUOTA_BYTES = 10 GiB`, `with_limit`
     configurable. Pinned/Archive counted (`pinned_bytes`) but never measured
     against the limit nor evicted. Verified by
     `pinning_folds_onto_the_entry_and_user_intent_wins_over_archive`.

2. **Eviction never races active transfers / open reads; quota shrink drains
   deterministically; POL-2 fixtures pass.**
   - Interlocks: host-supplied `EvictionRequest::protected` (open reads) +
     durable `has_live_transfer` — both skipped, cursor advances past them so a
     wall of in-use rows never stalls the drain
     (`eviction_never_races_open_reads_or_live_transfers`,
     `eviction_pages_past_many_in_use_rows_to_reach_an_evictable_tail`).
   - Quota shrink: `assess` produces over_by/reclaimable/residual with no
     mutation; `enforce` drains oldest-first
     (`shrinking_the_quota_yields_an_actionable_status_and_drains_deterministically`).
   - Dedup-safe object delete, row-before-file, last-referrer only
     (`a_shared_object_is_deleted_only_when_its_last_referrer_is_evicted`,
     state `materialization_ref_reference_tracks_shared_objects`).
   - POL-2 "media-cache-policy fixtures": no external JSON fixtures exist in this
     repo; the convention is Rust-coded fixtures. `tests/cache_quota.rs` (11 cases,
     `entry` seeds real `Promoter::promote` output) is that fixture suite.
     Reasonable, documented in LOGBOOK.

## Architecture fit

- Stateless-over-store, matching `TransferMachine`/`Promoter`. Additive state
  read queries only — **no schema change** (v1 frozen, NFR-041 respected).
- Closes the tracked LOGBOOK #70 gap: prior review (LOGBOOK 1755) flagged
  `pin_item`'s blind `ON CONFLICT origin = excluded.origin` downgrade and
  assigned the fix here. `cache::pin` resolves it directionally
  (`(Some(User), ArchiveMode) => User`) at the engine layer, no state change.
- Crash-safety delegated to `StateStore::reconcile` (`OrphanCacheObject`),
  consistent with the promotion layer.

## Non-blocking observations (no rework required)

1. **10 GiB vs spec "10 GB".** `.spec/policies.md:19` says "10 GB"; impl uses
   binary 10 GiB (+7.37%), deliberately documented to match the engine's
   byte-size convention. Configurable, so product can pin the exact default
   later; flag to product if decimal was intended.
2. **`enforce` and a mid-execute block.** If an item becomes blocked (new
   live transfer) between plan and execute, that pass may end with a small
   reclaimable residual instead of fully within-limit. Self-corrects on the
   next `enforce`; the post-status reports it honestly (never silent loss).
3. **`staged_transfer_bytes`** assumes canonical non-overlapping
   `completed_ranges` (informational `partial_transfer_bytes` only; not used for
   eviction/quota decisions).
4. **`cache_totals` / `cache_usage_by_kind`** are full-scan aggregates on
   on-demand cold paths (dev acknowledged; excluded from the hot-query plan
   gate). Acceptable.

## Gates

`make check` re-run by reviewer: 8/8 (toolchain, format, lint, test 11.4s,
architecture, supply-chain, traceability, scripts). Zero new dependencies.
