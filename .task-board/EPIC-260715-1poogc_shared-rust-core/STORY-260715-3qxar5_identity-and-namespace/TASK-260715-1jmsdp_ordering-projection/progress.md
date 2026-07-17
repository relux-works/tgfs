## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-15T00:52:44Z

## Last Update
2026-07-17T09:35:09Z

## Blocked By
- TASK-260715-2cl112
- TASK-260715-3tjduq

## Blocks
- (none)

## Checklist
- [x] POL-1 implementation: stable folder names, order.json per list root (Main/Archive/folder) regenerated on reorder events; numeric-prefix mode explicitly absent per DEC-013 (no dead mode code)
- [x] Chat position snapshots from server order (position.order, chat.id descending); reorder fixtures produce expected order.json changes with stable IDs and cached content untouched
- [x] order.json schema documented and deterministic (stable serialization); rename-only-on-chat-rename proven by fixture
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
spawn queued: [implementer] developer (claude) (run=RUN-260717-3bae54, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260717-3bae54)
Ready for review. POL-1/DEC-013 ordering projection in gramdrive-model: new `ordering` module (position snapshots -> deterministic order.json), plus three additive extensions it required — `OrderDocKey` + canonical tag 0x0b (GeneratedDocKey is chat-scoped and cannot name a list-scoped doc), `SiblingName::fixed` in the naming policy (a chat titled order.json must not shadow the metadata), and order.json as a node at each list root in the tree. Numeric-prefix mode is absent, not disabled (DEC-013, no dead mode code). make check 8/8; 118 tests in gramdrive-model.
Self-review found and fixed a real bug: duplicate-chat detection compared sorted neighbours, but the sort key starts with `order`, not chat_id, so an interleaved duplicate escaped and would have produced two identically-named folders. Now a set-based scan; both regression tests confirmed to fail against the reverted fix.
Also folded in the deferred comment-only fix from the 0821 review of TASK-260715-1ffbkg (naming.rs fold_key docs: last -> first), as the next task touching naming.rs.
FLAGGED FOR REVIEW, not changed: .spec/sync-and-filesystem-semantics.md SYNC-011 still reads "Numeric order prefixes are an optional presentation mode", which predates DEC-013 (no numeric prefixes in v1). Code follows DEC-013/POL-1 and the traceability matrix already reads SYNC-011 that way; left the stale sentence for the owner rather than rewriting a spec unilaterally.
Artifact: TASK-260715-1jmsdp_results.md. Logbook: 4 entries at 0843. Nothing staged or committed.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-3bae54, pid=10750, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-a0aee3, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-a0aee3)
agent completed: [reviewer] reviewer (claude) (exit=1)
spawn run completed: claude (run=RUN-260717-a0aee3, pid=24104, exit=1)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-b76621, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-b76621)
agent completed: [reviewer] reviewer (claude) (exit=1)
spawn run completed: claude (run=RUN-260717-b76621, pid=24835, exit=1)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-708d45, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-708d45)
REVIEW: ACCEPTED -> done. Re-ran make check (8/8 green) and cargo test -p gramdrive-model (118 passed, ordering_fixture 13/13 + ordering_properties 8/8). AC met and pinned by tests: reorder_changes_metadata_only asserts order.json bytes change while (id,name) set + doc_id + doc_key are byte-identical; rename_changes_the_name_and_nothing_else; positions_never_reach_identity_or_names (property). DEC-013 handled correctly: numeric-prefix mode is ABSENT not disabled, no dead mode code. Three additive extensions all justified: OrderDocKey+tag 0x0b (GeneratedDocKey is chat-scoped, cannot name a list-scoped doc; goldens pin additivity), SiblingName::fixed (list root is the only dir mixing a GramDrive constant with user titles; asymmetry is reasoned; loop termination survives), order.json tree node. Verified the self-reported duplicate-detection fix independently: old windows(2) check assumed sorted=>adjacent but sort is (order,chat_id) desc so duplicates at [(c5,20),(c9,15),(c5,10)] are non-adjacent; set-based scan is correct; both regression tests are genuine (2-record fixture had agreed with the bug); proptest seed checked in, not gitignored. order-as-JSON-string is correct (int64 rounds through IEEE-754 double). Nothing staged/committed. TWO NON-BLOCKING follow-ups, do not gate: (1) [owner] .spec/sync-and-filesystem-semantics.md:32 SYNC-011 still says numeric prefixes are an optional mode, predates DEC-013 - one-sentence owner edit, implementer correctly flagged rather than unilaterally rewriting owner spec; (2) [hardening] no cross-module test pins ordering<->tree name/id consistency, holds by construction today, add when provider layer lands. Full evidence: TASK-260715-1jmsdp_review.md.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-708d45, pid=42836, exit=0)

## Precondition Resources
(none)

## Outcome Resources
- [TASK-260715-1jmsdp_spawn-log_-implementer--developer--claude-.log](file://TASK-260715-1jmsdp/TASK-260715-1jmsdp_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-1jmsdp_results.md](file://TASK-260715-1jmsdp/TASK-260715-1jmsdp_results.md) — Implementation notes: ordering projection (POL-1/DEC-013), design decisions, duplicate-detection bug found in self-review, test coverage, make check 8/8
- [TASK-260715-1jmsdp_spawn-log_-reviewer--reviewer--claude-.log](file://TASK-260715-1jmsdp/TASK-260715-1jmsdp_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-1jmsdp_review.md](file://TASK-260715-1jmsdp/TASK-260715-1jmsdp_review.md) — Review verdict: ACCEPTED. make check 8/8 + 118 tests re-run independently; AC verified against fixtures; duplicate-detection fix verified; 2 non-blocking follow-ups (stale SYNC-011 spec sentence, ordering-tree consistency test)
