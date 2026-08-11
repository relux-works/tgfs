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
  SYNC-041/043/044/045/046). `FetchCoordinator` drives a `ContentSource`
  through the machine's claims: ranged readers coalesce onto live transfers
  and stream from staged bytes, while sink-less on-demand subscribers join
  the same close count so one caller cannot cancel work another still owns.
  Sub-fetches align to a chunk grid with bounded fanout per item, stale
  locators refresh in-attempt with identity unchanged, every other failure
  classifies through the machine's retry taxonomy, and cancellation is prompt
  both by dropped future and by durable two-phase cancel. Runtime-agnostic and
  clock-free: the host supplies a `Clock` and a `StagingHost`, and tests drive
  it on the testkit's deterministic executor.
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

- `render_plan` — the incremental render planner (TASK-260715-22l8zy;
  SYNC-024, SYNC-030..033, DOM-006, DOM-023). From the send instants of a
  normalized-change batch and the frozen renderer/schema versions, it computes
  which generated documents went stale: the bounded `Messages.md` and
  `Messages.ndjson` pair in each touched direct `YYYY-MM` partition (only), keyed by a
  `catalog` of document classes that read their identities, versions, and
  content-version tokens straight from `gramdrive-render`. Months come from the
  renderer's own IANA-aware `civil` calendar, so a message never plans into a
  month the renderer would not group it under. `dirty_affected` records the stale set on
  the durable dirty worklist in the change's own transaction (SYNC-022);
  `plan_for_changes` and `plan_worklist` turn stale documents into `RenderJob`s
  against the chat's current event watermark, skipping anything already current
  (idempotent re-planning). `render_pipeline` composes the pair from one pinned
  snapshot, promotes one immutable version directory, and publishes item facts,
  cache locators, watermarks, and provider change signals in one transaction
  (SYNC-033).

- `backfill` — the metadata-first local backfill scheduler
  (TASK-260715-mua1ng; POL-2/DEC-014, SYNC-020/021, SEC-031, NFR-033,
  NFR-031, SYNC-070).
  The provider-neutral *policy* the source's sans-IO history machines were
  built to be driven by: it reads no TDLib type, only the durable projection
  and the source failure taxonomy. `BackfillScheduler::plan_next` yields one
  history action per call, ordered by visible-item priority — a chat on
  screen, then a chat opened into, then the least-recently-synced tail of
  `backfill_backlog`. That tail contains only chats with a current Main,
  Archive, or custom-folder membership; canonical metadata outside the
  provider namespace cannot consume background quanta. It never returns a
  media action: media is not mirrored eagerly (SYNC-020). Foreground work runs
  even on a metered/power-saving
  device; only background metadata yields to those constraints. An
  account-global pacer (`pace`) spaces requests and honors Telegram flood
  waits against a durable deadline that survives restart, so a crash resumes
  neither paused work nor a violated flood wait (NFR-031, SYNC-070); the flood-wait
  attempt budget reuses the source machine's own per-request `attempt`.
  `media_policy` is the separate Archive-Mode eager-media gate — suspended
  while any history remains (metadata-first), on low/critical disk (POL-2
  disk warning), on a metered/offline link, while power-saving, or with
  Archive Mode off — and quota-exempt by construction: it never consults the
  cache quota, only physical disk. Durable pause/pacing live in
  `gramdrive-state`'s `backfill_control` row; `observe` reports the pause,
  the pending gate deadline, and the bounded backlog. Clock-free (`now_ms`
  threaded) and stateless, so scripted tests replay every decision exactly.

## Ownership

STORY-260715-2hs8cf (transfer-and-cache-engine) and STORY-260715-1oq9jg
(deterministic-rendering), EPIC-260715-1poogc (shared-rust-core). Populated by
TASK-260715-22fh09 (ranged fetch coordinator), TASK-260715-g4k3zm (durable
transfer state), TASK-260715-3s6cpe (integrity/promotion), TASK-260715-11abx8
(quota/eviction), TASK-260715-22l8zy (incremental render planner), and
TASK-260715-mua1ng (metadata-first local backfill scheduler,
STORY-260715-3l5jxq under EPIC-260715-2ptb18).

## Dependencies

Internal: `gramdrive-model`, `gramdrive-source`, `gramdrive-state`, and
`gramdrive-render` (used by `render_plan`). Platform-specific code:
forbidden. See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-engine
```
