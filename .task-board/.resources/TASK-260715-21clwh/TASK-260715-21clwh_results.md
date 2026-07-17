# TASK-260715-21clwh — Startup reconciliation and repair

Ready for review. `make check` 8/8 green.

## What landed

| File | What |
|---|---|
| `crates/gramdrive-state/src/reconcile.rs` (new, ~470 lines) | The pass: `StateStore::reconcile` (repair) and `StateStore::plan_reconcile` (dry run), the `LocalStorage` port, `Finding`/`Resolution`/`ReconcilePlan`/`ReconcileReport`/`Unresolved` |
| `crates/gramdrive-state/tests/reconcile.rs` (new, 14 tests) | The NFR-034 fixtures: missing / extra / corruption, plus a real process-crash test |
| `crates/gramdrive-state/src/error.rs` | `StateError::LocalStorage` |
| `crates/gramdrive-state/src/lib.rs`, `README.md`, `docs/TRACEABILITY.md` | Exports, docs, NFR-034 now maps here too |

## Acceptance criteria

> Synthetic corruption/missing/extra fixtures converge without Telegram writes or loss of valid pinned content.

- **Corruption / missing / extra** — one test each, plus `reconciling_a_broken_file_twice_changes_nothing_the_second_time` running all four fixtures at once and asserting the second pass is a fixed point.
- **No Telegram writes** — structural, not disciplinary: the architecture forbids `gramdrive-state` depending on `gramdrive-source`, and the entrypoints take a `LocalStorage` and nothing else. There is no source handle to misuse.
- **No loss of valid pinned content** — a missing cache object drops the `cache_entries` row (it claims bytes that do not exist) but never the `pins` row: POL-2 intent is independent of materialization, so the engine re-hydrates it. Asserted directly, and mutation-verified.

## Two design decisions worth review attention

**1. Reconciliation requires engine quiescence (an `fsck`-style caller contract).**

A `running` transfer is ambiguous: a dead claim and a live peer's claim are the same row, and this crate has no liveness primitive. The first implementation had the host declare its topology (`InFlightPolicy::Reclaim|Leave`, per PLAT-MAC-003). It passed the gate, and it was wrong: under `Leave`, orphan-object and leaked-staging repair race a live engine that has written an object but not yet committed its row. *Every* check here races a live engine, so this is not fixable per-finding.

The pass now requires that nothing else is touching what it repairs. That makes the rule single and coherent, removes API surface instead of adding it, and makes no assumption about engine write ordering — which is good, because `gramdrive-engine` is 32 lines and has no ordering contract yet. Callers: the app runs it at startup before claiming anything; the extension never runs it (it claims and materializes nothing); TASK-260715-1nuhxj quiesces first.

**Rejected:** lease/owner columns on `transfers` (migration v2). Topology-independent, but lease renewal is engine behavior and the engine is empty — the protocol would have had no participant except its own tests. Revisit if a second claimer ever appears.

**2. The `LocalStorage` port.** This crate never chooses paths, so it cannot walk a cache directory. The host implements the port; the pass joins the two inventories on the opaque handles already in the schema (`cache_entries.materialization_ref`, `transfers.temp_ref`). A failed *listing* is fatal (`StateError::LocalStorage`) — a survey against a partial inventory would read every unlisted object as an orphan and delete live cache. A failed *deletion* of one object is survivable and becomes an unresolved finding.

## Findings and repairs

| Finding | Evidence | Repair |
|---|---|---|
| `InterruptedTransfer` | a `running` row | requeued keeping `completed_ranges`/`temp_ref` — a resume, not a restart; `retry_count` untouched (a crash is not a failed attempt, SYNC-044) |
| `LeakedStaging` | staging object no live transfer claims | object deleted, stale `temp_ref` cleared off the terminal row |
| `MissingCacheObject` | `materialization_ref` absent from inventory (SYNC-053) | row dropped, **pin kept**; a generated doc also goes back on the dirty worklist |
| `OrphanCacheObject` | object no row claims | deleted |
| `UnlocatableCacheEntry` | entry with no handle | reported only — one we cannot check is not one we may delete |
| `ProjectionRebuildPending` | `rebuild_projection` marker | reported only, marker left raised: rebuilding `items` needs the engine-side projection builder. Work still owed |
| `MigrationInterrupted` | `migration_interrupted` marker | reported only — `open` resumes migrations, before any of this runs |

## Evidence

- `make check` — 8/8 (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts). Provenance: `.temp/acceptance/local-all`.
- `cargo test -p gramdrive-state --test reconcile` — 14/14.
- **The crash test is not a simulation.** It re-executes the test binary; the child claims a transfer, records durable progress, opens one more transaction and `abort()`s (verified exit 134 = SIGABRT, leaving a 267 KB uncheckpointed WAL). The parent reopens what the dead process actually left: committed progress present, the uncommitted transaction gone, one `InterruptedTransfer`, converged, and a second pass finds nothing.
- **Mutation-verified** (each fails the suite): requeue discarding staged ranges; treating every `temp_ref` as claimed; not protecting a claimed transfer's staging area; dropping the pin with the entry; not re-dirtying a missing generated document.

## Notes for TASK-260715-1nuhxj (repair and diagnostic export)

- `plan_reconcile` is the dry run; `Finding::resolution()` groups a plan by what repair would do, without matching every variant.
- `ReconcileReport::unresolved` carries the host's own words for a storage failure and a precise reason for each report-only finding — that is the "precise unresolved state" the AC asks for.
- It must quiesce the engine before calling either entrypoint.

## Follow-ups (not in scope, not blocking)

- `mark_transfer_done` does not clear `temp_ref` (`repo/transfers.rs:567`); reconciliation cleans up after it, but the engine clearing it at promotion would be cheaper than a pass finding it later.
- Rebuilding the `items` projection (SYNC-071's other half) needs the projection builder in `gramdrive-engine`; markers stay raised until it exists.
