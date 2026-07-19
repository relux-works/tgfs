# REVIEW-01 — TASK-260715-3s461k (offline pin & eviction reconciliation)

**Verdict: ACCEPTED → `done`**

Reviewer: reviewer (claude). Scope reviewed: working-tree diff on `main`
(FFI `shared_state.rs` + `api.rs`, Swift `FileProviderItem.swift`, both test
suites, logbook + progress notes). Read-only review; no code modified.

## What the task owns

Per `docs/TRACEABILITY.md`, SYNC-053 splits into **core reconciliation**
(TASK-260715-11abx8, done) and the **macOS reconciliation surface** (this task).
So the deliverable is projecting durable core pin state onto the read-only
File Provider surface — not writing pin state back (the provider opens as
`StateRole::Provider`, reads only).

## AC verification

- **Pinned intent is durable** — `ItemMetadata.pin: Option<PinOrigin>` is read
  from the durable `pins` table via `ReadTxn::pin` (a same-snapshot indexed
  point lookup) and folded into all four read paths (`item`, `children`,
  `child_by_name`, `item_changes_since`). Rust reopen test proves a pin set
  before the Provider handle exists surfaces after "restart"; unpin drops it
  back to `None`. ✓
- **Eligible content evicts only per policy** — `contentPolicy` maps
  pinned(user/archive)+fetchable/dir → `.downloadEagerlyAndKeepDownloaded`
  (SYNC-051 never evicted, POL-2 quota-exempt); unpinned file →
  `.downloadLazily` (SYNC-052 evictable); unpinned dir → `.inherited`;
  restricted/unavailable file never eager (POL-4 bytes never fetched). The
  eager-parent / explicitly-lazy-child model is self-consistent with
  NSFileProviderContentPolicy inheritance and correctly respects engine
  backfill pacing (children flip to eager only as coverage folds onto them). ✓
- **Reported state matches Finder/system state** — policy is a pure, total
  re-derivation of durable metadata on every read. Matches at read/enumeration
  time. ✓ (See cross-boundary caveat below re: *live* refresh.)

## Architecture fit

- Additive contract bump 0.4.0 → 0.5.0 with `#[uniffi(default = None)]`;
  packaging verifier confirms 0.5.0. Correct minor-version discipline.
- No pin *write* path leaked into the read-only provider surface — scope-clean.
- `contentPolicy` is macOS 13+; macOS-14 floor (POL-5) means no `@available`
  gate needed. Correct.

## Gates (independently re-run by reviewer)

- `cargo test -p gramdrive-ffi` → **29/29** (2 new pin tests incl. reopen).
- `swift test` (apple/GramDriveSupport) → **252/252** across 47 suites
  (8 new content-policy tests incl. exhaustive kind×availability×pin matrix).
- `make check` (suite all) → **8/8** (attached log).

## Cross-boundary finding — CONFIRMED, correctly out of scope

`gramdrive-engine/src/cache/pin.rs` `pin`/`unpin` write the `pins` table and
fold the cache row, but do **not** bump `metadata_version` or journal an item
change. Verified by reading the source. Consequence: a pin/unpin never enters
the working-set change feed, so the system re-reads the new `contentPolicy`
only on restart / full re-enumeration, not live. Durability across restart is
fine (re-derived from durable state); only *live* propagation is missing.

This is the **core engine's** (TASK-260715-11abx8) responsibility per the
SYNC-053 core/surface split — a read-only provider cannot and must not bump
`metadata_version`. The developer correctly stopped at the DEC boundary and
flagged it rather than hacking a write into the read-only surface (textbook
no-forced-fit). **Recommendation for the coordinator:** open a follow-up
against the engine owner to journal a metadata_version bump on pin/unpin, so
pin changes enter the change feed and Finder reflects them without a restart.
This does not block acceptance of the surface deliverable.

## Minor non-blocking nit

Logbook/progress notes cite "DEC-006 keeps the extension read-only," but per
TRACEABILITY.md DEC-006 is "No TDLib in iOS extension" (deferred-platform,
iOS). The read-only-provider property actually traces to the `StateRole::
Provider` design and the NFR-014 / DOM-008 read-only lineage. Code is correct;
only the citation is off. Not worth a rework cycle — noted for the record.
