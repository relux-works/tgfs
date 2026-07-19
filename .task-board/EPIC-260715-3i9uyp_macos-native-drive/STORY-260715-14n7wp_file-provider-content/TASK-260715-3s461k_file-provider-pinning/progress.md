## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-15T00:52:47Z

## Last Update
2026-07-19T16:19:58Z

## Blocked By
- TASK-260715-11abx8
- TASK-260715-kkglhx

## Blocks
- TASK-260715-3oe2nr

## Checklist
- [x] Pin/unpin (available offline) through provider API mapped to core pin state; Archive Mode surfacing; evictions dehydrate placeholders per POL-2 without touching pinned content
- [x] Tests: pin survives restart, eviction respects pins, dehydrate on quota pressure signals the system correctly
- [x] All quality gates green (make check + swift test)
- [x] Code written per task description and AC
- [x] Relevant tests written for new or changed behavior and passing
- [x] Lint clean
- [x] Relevant build/validation commands run after changes and build not broken
- [x] New outcome artifact attached on the board with a task-scoped name when the work produces notes, logs, screenshots, or other deliverables
- [x] Important findings, decisions, anomalies, or regressions recorded in logbook when relevant
- [ ] Implementation matches AC
- [ ] Solution fits project architecture
- [ ] Tests green
- [ ] If review does not accept the work — verdict evidence added and status routed by the explicit verdict branches

## Notes
spawn queued: [implementer] developer (claude) (run=RUN-260719-12a20b, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260719-12a20b)
Design (macOS reconciliation surface, SYNC-053): (1) FFI read surface gains ItemMetadata.pin: Option<PinOrigin> folded from txn.pin across item/children/child_by_name/item_changes_since; contract 0.4.0->0.5.0 additive. (2) Swift GramDriveFileProviderItem.contentPolicy maps core pin->NSFileProviderContentPolicy: pinned(user/archive)+fetchable/dir -> downloadEagerlyAndKeepDownloaded (POL-2 quota-exempt, SYNC-051 never evicted); unpinned file -> downloadLazily (evictable on pressure, SYNC-052); unpinned dir -> inherited (archive subtree coverage flows); restricted/unavailable never eager (POL-4 bytes never fetched). Relies on per-item pins incl ArchiveMode coverage, respecting engine backfill pacing; AccountInfo untouched. FINDING: engine pin/unpin (cache/pin.rs) does not journal an item change or bump metadata_version, so pin changes propagate to the working set only on restart/re-enumeration, not live -> flagged for engine/coordinator, out of DEC-006 provider scope.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-12a20b, pid=17951, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260719-6d8466, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260719-6d8466)
REVIEW-01 verdict: ACCEPTED -> done. AC met for the SYNC-053 macOS-surface scope: durable pin projected via ReadTxn::pin into all four read paths (reopen test proves durability, unpin drops to None); contentPolicy maps pinned->downloadEagerlyAndKeepDownloaded (SYNC-051/POL-2), unpinned file->downloadLazily (SYNC-052), unpinned dir->inherited, restricted/unavailable never eager (POL-4); pure/total re-derivation. Architecture fit: read-only StateRole::Provider (no pin write leaked), additive contract 0.4.0->0.5.0 with uniffi default. Gates independently re-run: cargo test -p gramdrive-ffi 29/29, swift test 252/252, make check 8/8. Cross-boundary finding CONFIRMED by reading cache/pin.rs: engine pin/unpin do not bump metadata_version or journal a change, so pin changes propagate live only on restart/re-enumeration; this is TASK-11abx8 core scope (read-only provider cannot bump it), correctly flagged not hacked -> recommend coordinator open engine follow-up for live propagation. Minor doc nit: DEC-006 cited for read-only is actually the no-TDLib-in-iOS decision; read-only traces to StateRole::Provider/NFR-014/DOM-008. Non-blocking. Full evidence: TASK-260715-3s461k_review-r1.md.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-6d8466, pid=36640, exit=0)

## Precondition Resources
(none)

## Outcome Resources
- [TASK-260715-3s461k_spawn-log_-implementer--developer--claude-.log](file://TASK-260715-3s461k/TASK-260715-3s461k_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-3s461k_results.md](file://TASK-260715-3s461k/TASK-260715-3s461k_results.md) — Implementation notes: FFI pin projection + contentPolicy mapping, AC coverage, tests, cross-boundary finding
- [TASK-260715-3s461k_make-check.log](file://TASK-260715-3s461k/TASK-260715-3s461k_make-check.log) — make check --suite all: 8/8 gates green
- [TASK-260715-3s461k_swift-test.log](file://TASK-260715-3s461k/TASK-260715-3s461k_swift-test.log) — swift test apple/GramDriveSupport: 252/252 across 47 suites
- [TASK-260715-3s461k_spawn-log_-reviewer--reviewer--claude-.log](file://TASK-260715-3s461k/TASK-260715-3s461k_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-3s461k_review-r1.md](file://TASK-260715-3s461k/TASK-260715-3s461k_review-r1.md) — REVIEW-01 verdict: ACCEPTED. AC/architecture verified, gates re-run green (ffi 29/29, swift 252/252, make check 8/8), cross-boundary metadata_version finding confirmed + routed to engine owner.
