# TASK-260715-mua1ng — Review: metadata-first local backfill scheduler

**Verdict: CHANGES REQUESTED → to-dev.** One verified defect (systematic spec
miscitation) that this project's traceability culture makes worth fixing. The
implementation itself is functionally correct, well-tested, and architecturally
clean — the rework is small, localized, and doc/citation-only.

## What was independently verified GREEN (reviewer re-run, not taken on trust)

- `make check` — 8/8 (toolchain, fmt, clippy `-D warnings`, workspace test,
  architecture boundary, cargo-deny, traceability, scripts). Fresh run,
  provenance `.temp/acceptance/local-all`, exit 0.
- Test counts match the claim exactly: **5 pace unit + 17 engine integration +
  4 state integration**, all passing (re-ran each binary).
- State APIs used correctly: `backfill_backlog` (history_complete=0, ORDER BY
  last_sync_at_ms), `chat_sync_state`, `account().archive_mode` — signatures and
  semantics match `changes.rs` / `accounts.rs`.
- Error seam present: `EngineError: From<StateError>` (`transfer/error.rs:106`).
- Boundary-clean: no TDLib types, no `cfg(target_os)`; architecture gate passes.
- Scheduler logic reviewed for correctness — no bug found. The `foreground`
  filter in the background branch is redundant-but-harmless (foreground chats
  needing history are always returned before the background stage, and the
  backlog only holds incomplete chats). No starvation, no off-by-one.
- Durability proven: `pause_and_flood_wait_survive_a_restart` reopens a real
  file-backed sqlite store and confirms a restart resumes neither paused work
  nor a violated flood wait.

## AC assessment: MET at the policy layer

Durable ✓  Bounded ✓ (one action/call, backlog_scan cap)  Observable ✓
(`observe`)  User-pausable ✓ (`set_paused`)  Avoids eager mobile media ✓
(`plan_next` structurally cannot emit media; `media_policy` is a separate gate
that suspends on `Metered`/`MetadataPending`/disk/etc.). Visible>Requested>
Background priority ✓. Flood-wait honoring (durable, non-shortening, budgeted,
fallback floor) ✓. Host-supplied device/network/disk gating ✓.

Takeout correctly excluded (task scope: "Normal TDLib API only"). Deep
desktop backfill represented via Archive Mode + `HostConditions::UNCONSTRAINED`.

## DEFECT (blocking rework) — SYNC-041 miscited as "pausability"

**Verified against spec text.** `.spec/sync-and-filesystem-semantics.md:57`:

> SYNC-041 (V1): Fetch accepts byte ranges even if a source internally
> downloads larger aligned chunks.

SYNC-041 is about **byte-range fetch**. It has nothing to do with pausing.
There is **no `paus*` string anywhere in `.spec/`** — user-pause is not a
numbered spec clause at all. Yet this task cites SYNC-041 as the basis for the
pause switch in **6 places**:

- `crates/gramdrive-engine/src/backfill/mod.rs:532` (`set_paused` doc)
- `crates/gramdrive-state/src/repo/backfill.rs:31` (`paused` field doc)
- `crates/gramdrive-state/src/repo/backfill.rs:61` (`backfill_control` doc — "SYNC-041, NFR-033")
- `crates/gramdrive-state/src/schema/v1.sql:513` ("paused — user pause switch (SYNC-041 pausability)")
- `crates/gramdrive-state/README.md:42` (backfill_control row — "(SYNC-041, ...)")
- `crates/gramdrive-engine/tests/backfill_scheduler.rs:502` ("User pause (SYNC-041)")
- (and the artifact `TASK-260715-mua1ng_results.md:56`)

**Why this matters in THIS project:** the whole repo uses SYNC-041 correctly and
consistently for ranged fetch (`transfer/ranges.rs`, `fetch/plan.rs`,
`transfers.rs:463`, `fetch_coordinator.rs`, engine `README.md:21`). This task is
the *only* place that repurposes it. The dedicated traceability gate PASSES
because it only checks that an ID exists — it cannot catch semantic misuse. So a
false requirement→implementation edge (SYNC-041 "implemented by" a pause switch)
is now baked into source, schema, tests, and docs, and will mislead any future
spec-coverage audit.

**Recommended fix (small, doc-only):** re-ground the pause feature on its real
source — the task AC itself ("Scheduler is ... user-pausable") plus the
cancellation/durable-state clauses that actually cover it:
- SYNC-043 — "Cancellation stops network and disk work promptly ... leaves
  resumable or safely disposable state"
- SYNC-005 — "long work is cancellable or converted into durable background/
  transfer state"

i.e. replace the `(SYNC-041)` pause citations with `(SYNC-043/SYNC-005 + task
AC)`, or drop the numbered ID and cite the AC. Then re-run `make check` (the
traceability gate stays green either way).

## Minor over-attributions (optional tightening, NOT blocking)

- **SYNC-020 conflation** — SYNC-020 covers metadata-first / no-eager-media
  only (that half is correct). "Visible-item priority" is **task-description**
  grounded, not SYNC-020. Suggest attributing the priority ladder to the task
  description rather than folding it under SYNC-020.
- **POL-8 for restart-durability** is stretched — POL-8 is the human-approval-
  gate policy; its ToS/account-safety-risk exception connects loosely, but the
  restart-must-not-re-hammer requirement's real homes are SEC-031
  ("without retry storms"), NFR-033 ("flood waits never become tight retry
  loops"), NFR-031 ("progress survive process restart"), SYNC-070 (startup
  recovery). Consider swapping POL-8 → NFR-031/SYNC-070 where durability is the
  point.
- SEC-031 "request spacing" (gloss over "bound request concurrency") and POL-2
  continuous disk-suspend (spirit-aligned + SYNC-044 disk-full handling) are
  **defensible** — no change needed.

## Not gaps (checked, out of scope for this neutral policy layer)

- NFR-032 health-data wiring and SYNC-023 gap-recovery ordering live in the
  host/FFI seam and the source CrawlMachine respectively — correctly out of
  this task's scope. `observe()` exposes the observability surface the host
  will feed into NFR-032.
