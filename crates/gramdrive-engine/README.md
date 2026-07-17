# gramdrive-engine

Transfer and cache engine — the orchestration layer: hydration, pin/offline
state, resumable ranged downloads, integrity and cache promotion, quota
accounting, LRU eviction of unpinned content (POL-2). Drives any
`DriveSource` through the contract; persists durable transfer state via
`gramdrive-state`.

## Modules

- `transfer` — the durable transfer state machine (TASK-260715-g4k3zm;
  SYNC-040..046). `TransferMachine` is stateless policy over the journal
  `gramdrive-state` persists: request/coalesce with a version pin, claims
  with a resume plan computed from persisted ranges, monotonic progress
  under one staging handle, a promotion gate that never publishes
  incomplete or stale content, deterministic version-race invalidation,
  a bounded retry budget with parking for precondition-blocked faults,
  and two-phase cancellation. Crash-resume composes with
  `StateStore::reconcile`: reconcile first, then claim.
- `fetch` — the ranged fetch coordinator (TASK-260715-22fh09;
  SYNC-041/043/044/045/046). `FetchCoordinator` drives a `DriveSource`
  through the machine's claims: readers coalesce onto live transfers and
  stream from staged bytes, sub-fetches align to a chunk grid with bounded
  fanout per item, stale locators refresh in-attempt with identity
  unchanged, every other failure classifies through the machine's retry
  taxonomy, and cancellation is prompt both by dropped future and by
  durable two-phase cancel. Runtime-agnostic and clock-free: the host
  supplies a `Clock` and a `StagingHost`, and tests drive it on the
  testkit's deterministic executor.
- `cache` — integrity verification and atomic promotion (TASK-260715-3s6cpe;
  SYNC-042, SYNC-050..053). `Promoter` layers over the machine's
  `CompleteOutcome::Promoted`: it hashes the whole staged object with a
  vendored SHA-256 (`gramdrive-model::hash`) and fails closed on truncated or
  unreadable bytes, re-checks the version pin, then promotes the object into
  content-addressed cache through the host `PromotionHost` port and records
  the blob, the `verified` cache entry, and — for an attachment — the blob
  link, all in one transaction. File-before-row ordering makes every crash a
  reconcilable disagreement (orphan object or leaked staging), and the
  content-addressed handle gives idempotent promotion and per-account dedup
  for free. Whole content only.
  The same module's `Evictor` (TASK-260715-11abx8; POL-2, SYNC-050..054) owns
  cache accounting, quota enforcement, and LRU eviction: device-wide
  accounting by category including partial transfers (`accounting`), the
  actionable quota status a change produces (`assess`, SYNC-054), and eviction
  of eligible unpinned-verified content only (`enforce`, `reclaim`). Pinned
  and Archive-Mode content is quota-exempt but counted; eviction never races an
  open read (host-supplied protected set) or a live transfer (durable
  interlock), and deletes an on-disk object only once no surviving entry
  references it (dedup), row-before-file so a crash leaves a reconcilable
  orphan. `cache::pin` / `cache::unpin` fold durable offline intent onto the
  materialized row with directional origin (a user pin is not downgraded by
  Archive-Mode coverage). Quota *value* durability is the host's device
  config; system/provider eviction is reconciled by `StateStore::reconcile`.

## Ownership

STORY-260715-2hs8cf (transfer-and-cache-engine), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-22fh09 (ranged fetch
coordinator), TASK-260715-g4k3zm (durable transfer state), TASK-260715-3s6cpe
(integrity/promotion), TASK-260715-11abx8 (quota/eviction).

## Dependencies

Internal: `gramdrive-model`, `gramdrive-source`, `gramdrive-state`
(`gramdrive-render` allowed, not yet used). Platform-specific code:
forbidden. See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-engine
```
