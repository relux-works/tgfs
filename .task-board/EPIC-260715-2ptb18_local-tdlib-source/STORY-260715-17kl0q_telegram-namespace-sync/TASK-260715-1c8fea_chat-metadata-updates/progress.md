## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-15T00:52:46Z

## Last Update
2026-07-17T23:36:06Z

## Blocked By
- TASK-260715-30amrq

## Blocks
- TASK-260715-10p5zp
- TASK-260715-rhcnhc

## Checklist
- [x] Chat metadata updates (title, photo, order/pin changes, membership) applied incrementally from TDLib updates into state; ordering metadata stays consistent with POL-1 projection
- [x] Out-of-order and duplicate update handling proven by scripted tests; rename triggers folder rename event, reorder triggers order.json regen only
- [x] All quality gates green (make check)
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
spawn queued: [implementer] developer (claude) (run=RUN-260717-1a5ae1, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260717-1a5ae1)
Design locked (.temp/TASK-260715-1c8fea/design.md): sans-IO UpdateMachine in gramdrive-source-tdjson mapping live TDLib chat/list updates to a provider-neutral normalized change stream + POL-1 invalidations (FolderName vs ListOrdering vs ChatMetadata). Reuses upsert_chat + two new incremental chat_list_entry repo methods. Implementing.
Implemented UpdateMachine (sans-IO live chat-metadata/list mapper) in gramdrive-source-tdjson/src/updates.rs: TDLib push updates -> provider-neutral normalized change stream + POL-1 invalidation split (FolderName=rename, ListOrdering=reorder->order.json only, Metadata=first-sight/photo/protection). Idempotent under duplicate/out-of-order/restart; unknown chats -> UpdateBatch.unresolved (getChat resolution, SYNC-023). Shared wire parsers extracted to src/wire.rs. Added incremental upsert_chat_list_entry/remove_chat_list_entry to gramdrive-state (whole-list replace would wipe under a partial model). 14 unit + 5 integration (tests/chat_updates.rs) + 1 state-repo test. make check 8/8 green. Artifacts: TASK-260715-1c8fea_results.md, _design.md. AC met: replay converges, reorder keeps canonical id (written==0), gap/restart pass.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-1a5ae1, pid=86834, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-2a5420, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-2a5420)
REVIEW VERDICT: ACCEPTED -> done. Read-only review, no code modified. AC all met and verified against code+tests: (1) replay converges (last-write-wins + no-op coalescing + idempotent upserts; duplicate/out-of-order/independent-order suites, replay is a fixed point); (2) reorder does not change canonical ID (position change never dirties metadata -> no upsert_chat -> written==0, after==before); (3) gap/restart pass (unknown chat -> unresolved, no forged row, FK-held membership, getChat feedback resolves; restart re-push churns nothing). POL-1 split correct: rename->FolderName, reorder->ListOrdering only, first-sight/photo/protection->Metadata. Architecture fits SnapshotMachine precedent: sans-IO reducer, product depends only on gramdrive-model, state stays dev-dep; new upsert/remove_chat_list_entry ON CONFLICT matches chat_list_entries PK, FK-enforced (is_err on unknown chat), idempotent; shared wire.rs kills int64-order drift risk. make check RE-RUN INDEPENDENTLY: 8/8 green (toolchain/format/lint -D warnings --all-features/test/architecture/supply-chain cargo-deny/traceability/scripts). Non-blocking info notes for next producer in TASK-260715-1c8fea_review.md (combined rename+photo emits FolderName only by design; photo_token to_string fallback; lingering empty positions maps). No rework needed.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-2a5420, pid=99549, exit=0)

## Precondition Resources
(none)

## Outcome Resources
- [TASK-260715-1c8fea_spawn-log_-implementer--developer--claude-.log](file://TASK-260715-1c8fea/TASK-260715-1c8fea_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-1c8fea_results.md](file://TASK-260715-1c8fea/TASK-260715-1c8fea_results.md) — Implementation results: AC/DoD evidence, design decisions, gate status
- [TASK-260715-1c8fea_design.md](file://TASK-260715-1c8fea/TASK-260715-1c8fea_design.md) — Design/research notes: mapper altitude, invalidation split, gap/restart model
- [TASK-260715-1c8fea_spawn-log_-reviewer--reviewer--claude-.log](file://TASK-260715-1c8fea/TASK-260715-1c8fea_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-1c8fea_review.md](file://TASK-260715-1c8fea/TASK-260715-1c8fea_review.md) — Reviewer verdict: ACCEPTED — AC/DoD/architecture evidence, independent make check 8/8, informational notes
