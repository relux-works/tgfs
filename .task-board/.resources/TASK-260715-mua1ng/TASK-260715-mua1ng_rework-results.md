# TASK-260715-mua1ng — Citation rework results (doc-only)

Per review `TASK-260715-mua1ng_review-notes.md`. Implementation accepted; **no behavior change**. Only doc comments / schema comments / README / test comments / results.md touched.

## Blocking defect fixed: SYNC-041 pause miscitation dropped

SYNC-041 = byte-range fetch (`.spec/sync-and-filesystem-semantics.md:57`); it has nothing to do with pausing, and no `paus*` clause exists in `.spec/`. Re-grounded the pause switch on **the task AC ("user-pausable") + SYNC-043 (cancellation → resumable state) / SYNC-005 (durable long work)** at all 6 sites plus the results artifact:

| Site | Before | After |
|---|---|---|
| `engine/src/backfill/mod.rs:532` (`set_paused` doc) | SYNC-041 | task AC + SYNC-043/SYNC-005 |
| `state/src/repo/backfill.rs:31` (`paused` field) | SYNC-041 | task AC + SYNC-043/SYNC-005 |
| `state/src/repo/backfill.rs:61` (read doc) | SYNC-041, NFR-033 | SYNC-043/SYNC-005, NFR-033 |
| `state/src/schema/v1.sql:513` (`paused` col) | SYNC-041 | task AC + SYNC-043/SYNC-005 |
| `state/README.md:42` (row) | SYNC-041, POL-8 | SYNC-043/SYNC-005, NFR-031/SYNC-070 |
| `engine/tests/backfill_scheduler.rs:502` (section) | SYNC-041 | task AC + SYNC-043/SYNC-005 |
| `results.md:56` | SYNC-041 | task AC + SYNC-043/SYNC-005 |

Legit ranged-fetch SYNC-041 at `engine/README.md:21` (FetchCoordinator/DriveSource) left untouched.

## Optional tightenings applied

- **SYNC-020 → task description** for visible-item priority (`mod.rs:25,90,200`, `test:133`, `results.md:33`). SYNC-020 kept only for metadata-first / no-eager-media, which is correct.
- **POL-8 restart-durability stretch → NFR-031 (progress survives restart) / SYNC-070 (startup recovery)**: `pace.rs:16`, both module headers, `v1.sql:508`, `state/README.md:42`, `engine/README.md:73,84`, `results.md:48`. The re-hammer/ban-risk clause re-homed on **NFR-033** (flood waits never a tight retry loop).

## Verification

- `make check` **8/8 green** (provenance `.temp/acceptance/local-all`).
- My-scope tests re-run green: 17 backfill integration + 2 query_plans + 4 repo_backfill + 5 pace unit.
- Traceability gate scans only `docs/TRACEABILITY.md` + `.task-board/**/README.md` (excl `.resources`) — none of the edited files — so these citation edits move nothing in the gate either way. All cited IDs (SYNC-043/005/070, NFR-031/033) are real `.spec/` clauses.

## Unrelated pre-existing bug surfaced (flagged, NOT fixed)

First `make check` run failed on `gramdrive-model` `naming_properties::sanitize_is_idempotent`: `sanitize(sanitize(x)) != sanitize(x)` for a combining-mark input. Seed-dependent proptest failure (proptest randomizes per run; reviewer 8/8 run missed it, my run hit it and auto-persisted the seed). **Not caused by this doc change** — zero `gramdrive-model` lines touched; model is the lowest layer. Reverted the auto-generated `naming_properties.proptest-regressions` byproduct (not this task s artifact); the reproducing seed is preserved in `LOGBOOK.md` (1055) and `.temp/acceptance/local-all/test.log`. Needs its own model-crate task/owner. Not fixed here: out of doc-only scope and model is under concurrent editing.