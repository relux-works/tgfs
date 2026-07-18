# TASK-260715-mua1ng — Metadata-first local backfill scheduler

Status: ready for review.

## What was built

A provider-neutral, sans-IO, durable backfill **scheduler** in `gramdrive-engine`
plus the minimal durable substrate it needs in `gramdrive-state`. No scheduler
existed before: the tdjson source ships sans-IO machines (`CrawlMachine`,
`LiveMachine`, `SnapshotMachine`) that emit `Backoff{retry_after_secs, attempt}`
obligations and expect a composing caller to own the clock, pacing, and
flood-wait budget. This task is that missing composing **policy**.

### Placement decision
- Scheduler lives in `gramdrive-engine` (deps: model/source/state/render; no
  `Value`/`TdError`/`CrawlMachine`). Confirmed by tdjson `Cargo.toml`: "product
  code never links state from here — the composing caller (engine/ffi) owns
  that wiring," and the engine's established "engine owns retry/scheduling
  policy; sources only classify" split (`transfer/retry.rs`).
- The tdjson glue mapping `BackfillStep::AdvanceHistory` → `CrawlMachine::set_priority`
  is a thin later host/FFI seam, out of this task's product scope.

## Acceptance criteria → evidence

Scheduler is **durable, bounded, observable, user-pausable, and avoids eager
mobile media mirroring**.

- **Metadata-first / no eager mobile media (SYNC-020, POL-2):** `plan_next`
  yields only history actions, never media. Eager media is a *separate*
  `media_policy` gate that suspends while any history remains
  (`MetadataPending`) — so media never precedes metadata, and is never
  scheduled by default.
- **Visible-item priority (task description):** Visible > Requested > Background
  (least-recently-synced tail of `backfill_backlog`). Foreground runs even on
  metered/power-saving links; only background metadata defers.
- **Flood waits (SEC-031, NFR-033):** account-global pacer honors Telegram
  429 delays against a durable `flood_wait_until_ms`; a later short wait never
  shortens an earlier long one; an unstated wait uses a conservative fallback
  floor; the attempt budget reuses the source machine's own per-request
  `attempt`. Never a tight loop.
- **Device power/network/disk:** host-supplied `HostConditions`
  (Online/Metered/Offline, Unconstrained/Saving, Ample/Low/Critical). No
  platform code in the engine (boundary-clean).
- **Archive-Mode eager backfill honors quota-exemption + disk warnings
  (POL-2/DEC-014):** `media_policy` gates on physical disk (Low/Critical
  suspend) and device conditions, and is quota-exempt **by construction** — it
  never consults the cache quota.
- **Durable (NFR-031, SYNC-070):** pause + flood-wait deadline persist in the new
  `backfill_control` row; a file-backed restart test proves a reopen resumes
  neither paused work nor a violated flood wait. Per-chat progress is the
  existing `chat_sync_state`.
- **Bounded:** one action per `plan_next`; backlog scan capped by
  `BackfillConfig::backlog_scan`.
- **Observable:** `observe` reports pause, pending gate deadline, and bounded
  backlog size.
- **User-pausable (task AC; SYNC-043/SYNC-005):** durable `set_paused`; paused → `BackfillStep::Paused`
  and eager media suspended.

## Files

### gramdrive-state (durable substrate)
- `src/schema/v1.sql` — new `backfill_control` table (per-scope; template =
  `change_cursors`).
- `src/repo/backfill.rs` (new) — `BackfillControlRecord` + `ReadTxn::backfill_control`
  + `WriteTxn::put_backfill_control`.
- `src/repo/mod.rs` — module wiring + re-export.
- `tests/query_plans.rs` — `backfill_control_lookup` required-query (PK point read).
- `tests/repo_backfill.rs` (new) — 4 tests: absent→None, round-trip, upsert, fresh().

### gramdrive-engine (scheduler)
- `src/backfill/mod.rs` (new) — `BackfillScheduler`, `plan_next`, `media_policy`,
  `note_dispatch`, `note_flood_wait`, `set_paused`, `observe`, and the public
  types (`BackfillStep`, `BackfillPriority`, `IdleReason`, `MediaPolicy`,
  `MediaSuspend`, `HostConditions`, `BackfillDemand`, `BackfillObservation`,
  `FloodOutcome`, `BackfillConfig`).
- `src/backfill/pace.rs` (new) — pure account-global pacer (`PaceConfig`, gate
  math, flood/spacer deadlines) + 5 unit tests.
- `src/lib.rs` — `pub mod backfill;` + module docs.
- `tests/backfill_scheduler.rs` (new) — 17 scripted integration tests
  (scheduling order, device gating, offline, spacing, flood-wait honoring +
  budget + non-shortening, pause, media policy across all reasons,
  observability, restart durability over a file-backed store, custom tuning).
- `README.md` — `backfill` module + ownership.

## Verification
- `make check` — 8/8 green (fmt, clippy `-D warnings`, workspace test,
  architecture, cargo-deny, traceability, scripts). Provenance
  `.temp/acceptance/local-all`.
- New tests: 5 pace unit + 17 engine integration + 4 state integration; all
  green. query_plans still index-clean on the 2,048-chat / 110k-message
  synthetic account.

## Scope boundaries (no forced fit)
- The scheduler decides; it does not enumerate media attachments or enqueue
  transfers (that is the transfer/cache layer + host). `media_policy` is a
  conditions gate over archive scope, keeping ownership clean.
- Wiring the neutral `BackfillStep`/`MediaPolicy` onto the concrete tdjson
  `CrawlMachine`/transfer machine is later host/FFI composition.
