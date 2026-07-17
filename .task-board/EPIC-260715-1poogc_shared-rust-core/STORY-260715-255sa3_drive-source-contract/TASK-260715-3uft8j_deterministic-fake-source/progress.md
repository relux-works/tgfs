## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-15T00:52:44Z

## Last Update
2026-07-17T11:03:20Z

## Blocked By
- TASK-260715-1j4ij3

## Blocks
- TASK-260715-3e8q4m
- TASK-260715-26eoqx

## Checklist
- [x] Deterministic fake DriveSource: scripted snapshots, paged listings, change feeds, ranged reads, injectable delays/failures/version-races and cancellation points, reproducible by seed/script
- [x] Request/interaction recording so tests can assert exact calls, cancellation propagation and side effects
- [x] Lives in gramdrive-testkit, usable from any crate and future platform integration tests without duplication
- [x] All quality gates green (make check)
- [x] Code written per task description and AC
- [x] Relevant tests written for new or changed behavior and passing
- [x] Lint clean
- [x] Relevant build/validation commands run after changes and build not broken
- [x] New outcome artifact attached on the board with a task-scoped name when the work produces notes, logs, screenshots, or other deliverables
- [x] Important findings, decisions, anomalies, or regressions recorded in logbook when relevant
- [x] Implementation matches AC
- [x] Solution fits project architecture
- [x] Tests green
- [ ] If review does not accept the work — verdict evidence added and status routed by the explicit verdict branches

## Notes
spawn queued: [implementer] developer (claude) (run=RUN-260717-b41382, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260717-b41382)
Deterministic fake DriveSource implemented in gramdrive-testkit. 144 tests (72 unit + 65 integration + 7 doc), make check 8/8, zero new dependencies.
Key design decision for review: a scripted delay is a count of yields, not a Duration. A fake that slept on a real clock would be non-reproducible by construction and would force an async runtime dependency on every consumer; nothing the delay exists to test is about elapsed time. Wall-clock time reaches the contract only where the contract names it (the Duration inside RateLimited, which is data the caller reads). Every yield wakes itself so the fake is drivable by both the bundled executor and the engine tokio -- tested with a waker-respecting executor, and verified by mutation that removing the wake fails only that test.
Also deliberate: page tokens name the revision they were minted at, so advancing mid-enumeration rejects the continuation. Stricter than SYNC-003 requires, because the alternative is a fake that splices two states into one listing -- the exact failure the conformance suite exists to catch.
Two lint exemptions, both argued from measurement rather than assumed (result_large_err: Result<SourceScript, ScriptError> is 288 bytes = exactly size_of::<SourceScript>(), so the Err rides free). See TASK-260715-3uft8j_results.md and LOGBOOK 2026-07-17 for full rationale.
Found and fixed during development: Tree::upsert silently created a second root when an upsert set parent to None.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-b41382, pid=66096, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-0a0e52, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-0a0e52)
REVIEW VERDICT: changes requested -> to-dev. Evidence: TASK-260715-3uft8j_review.md + TASK-260715-3uft8j_repro-clear-interactions.rs.
Verified independently, all CONFIRMED: make check 8/8 re-run clean; 144 tests exact (72+65+7); result_large_err sizes measured 168/288/288 exactly as argued; mutation check reproduced (removing wake_by_ref fails ONLY every_yield_wakes_itself..., other 64+72 pass); SplitMix64/FNV-1a constants canonical; architecture boundary holds, zero new deps. Implementation notes were honest on every measurable claim. Design decisions all endorsed (delay-as-yields, revision-pinned page tokens, written-out PRNG, up-front build validation, both lint exemptions).
Rework needed:
1. [correctness] clear_interactions() misattributes outcomes. Recorder::begin assigns seq=log.len(); settle writes log[seq]; clear() empties the log without invalidating live CallGuard seqs, so indices are reused. Reproduced via public API: a SUCCESSFUL root() is recorded Ok, then flips to Cancelled{delivered:0} when an unrelated in-flight fetch is dropped. Second silent mode: stale seq past the end makes settle a no-op, so a cancellation record vanishes. Untested because interactions_can_be_cleared_between_phases only clears when everything has settled. Fix: generation/epoch bumped by clear() and carried by the guard, or monotonic ids instead of positional indices + a test that clears with a call in flight.
2. [test-coverage] Call::Fetch arguments never asserted anywhere (grep -c = 0). AC is "assert requests"; fetch is the richest call (item+version+range). every_call_is_recorded_in_order_with_its_arguments covers the other five and omits it.
3. [docs] fault.rs:34-36 states a second matching fault"s counter does not advance; the implementation advances every matching fault"s counter (fake.rs:225-238) and fake.rs:214-220 documents that. Verified empirically. Behavior is right, the module doc is wrong.
4. [docs] results.md overstates a_full_scripted_scenario_reaches_every_configured_event as asserting "the exact 9-call interaction log": it discards arguments, asserts len==9, operation for 2 of 9, outcome for 6 of 9 (indices 4/5/6 unasserted), no arguments compared. Its docstring also claims cancellation coverage the test does not have, and its delay(1) is unobservable under exec::drive.
5. [test-coverage] No fault ever played on Operation::Changes or Operation::Thumbnail.
6. [test-quality] Several is_err() assertions do not check the scripted error type; fixed_chunking_cuts_at_stated_boundaries take().all() is vacuous without a len assert; a_version_conflict_asks_for_a_refresh duplicates a gramdrive-source test and touches no fake code.
7. [design, non-blocking decision] Outcome::Failed carries no delivered count (only Cancelled does), so a version race/sink-stop side effect is unreadable from interactions(). Decide before the conformance suite builds on it.
Not blocked: ordinary rework, no external/human decision needed.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-0a0e52, pid=79977, exit=0)
spawn queued: [implementer] developer (claude) (run=RUN-260717-e717d5, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260717-e717d5)
Rework complete per TASK-260715-3uft8j_review.md — all 6 items addressed + decision on non-blocking finding 7. Evidence: TASK-260715-3uft8j_rework-results.md.
1) clear_interactions() misattribution FIXED: log carries an epoch that clear() bumps and each CallGuard remembers; a guard from a cleared epoch settles nothing. Chose epoch over monotonic ids so seq keeps its documented positional-from-zero meaning. Verified two ways: reviewer repro binary re-run UNCHANGED now prints Ok where it printed Cancelled{delivered:0}; and mutation (epoch check removed) fails exactly the two new tests written for it, one per layer, nothing else.
2) Call::Fetch args ASSERTED in every_call_is_recorded_in_order_with_its_arguments (item+version+range, non-trivial 4..12 range). grep Call::Fetch: 0 -> 3.
3) fault.rs counting doc CORRECTED to the implemented every-matching-fault-advances rule, with why it is the rule that composes.
4) Full-scenario test now EARNS its docstring: added a delay(2) fault dropped mid-delay (real cancellation + the delay is now observable), asserts all 10 calls exactly with arguments and all 10 outcomes incl. the previously untouched 4/5/6.
5) Fault coverage ADDED for Operation::Changes (fails once, same cursor still serves the retry) and Operation::Thumbnail (delay + scripted failure stays distinguishable from a scripted absent thumbnail). Every Operation variant now has a fault played on it.
6) Assertions TIGHTENED: 3 is_err() -> scripted error types; vacuous take().all() -> exact assert_eq!(sizes, [10,10,10,10,10,6]); cancellation bound 0<d<56 -> exact d==20; duplicate a_version_conflict_asks_for_a_refresh DELETED with a comment saying why.
7) DECISION on finding 7: Outcome::Failed(SourceError) -> Failed{error, delivered}. Taken now, before the conformance suite builds on the type — the AC says assert side effects, and a race that moved 8 bytes was only visible via the sink. Deliberately NO delivered() accessor: it would return 0 for a successful full-range fetch, a lossy answer to the question it names.
Honest limit: the two clear-tests covering the silent-loss half pass under the mutation too (that half was already safe by accident via get_mut returning None) — kept as intent-documentation, not claimed as regression coverage.
make check 8/8. Testkit 151 tests (76 unit + 68 integration + 7 doc), was 144. Workspace 335. Original results.md corrected via CLI: its exact-9-call-log claim was the overstatement review caught.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-e717d5, pid=84361, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-fc4f9e, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-fc4f9e)
REWORK REVIEW: accepted -> done. All 6 requested items fixed and verified independently, not on trust. Finding 1 (recorder misattribution): epoch on Log, bumped by clear(), carried by CallGuard, checked in settle(); a stale guard settles nothing. Verified two ways by the reviewer: (a) rebuilt the original repro unchanged against the fixed crate via public API - now prints Ok where it printed Cancelled{delivered:0}; (b) own mutation - stripping the epoch guard fails exactly the 2 new tests (1 unit + 1 integration) and all other 142 pass. Findings 2-6 confirmed: Fetch args now asserted with a non-trivial range (grep 5, was 0); fault.rs counting doc now matches gate() impl and explains why the rule composes; full-scenario test earns its docstring (real cancellation via a polled-to-Pending delay, all 10 calls with arguments, all 10 outcomes incl. previously untouched 4/5/6); faults now played on every Operation variant; all 7 assertions tightened (vacuous take().all() and loose bounds gone, duplicate deleted with a why-comment). Finding 7 decided well and at the right time: Failed{error,delivered} taken before TASK-260715-3e8q4m depends on the type; no delivered() accessor and no count on Ok both correctly refused. make check 8/8 re-run by reviewer; 151 tests (76+68+7) counted exactly. Blast radius confirmed testkit-internal: no Cargo.toml references the testkit. Implementation notes were honest on every measurable claim, including volunteering that the two silent-loss tests pass under mutation and are intent-documentation rather than regression coverage. Outcome shape is now the stable surface for the conformance suite.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-fc4f9e, pid=90525, exit=0)

## Precondition Resources
- [TASK-260715-3uft8j_rework-scope.md](file://TASK-260715-3uft8j/TASK-260715-3uft8j_rework-scope.md) — Rework: recorder epoch bug + coverage and doc fixes

## Outcome Resources
- [TASK-260715-3uft8j_spawn-log_-implementer--developer--claude-.log](file://TASK-260715-3uft8j/TASK-260715-3uft8j_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-3uft8j_results.md](file://TASK-260715-3uft8j/TASK-260715-3uft8j_results.md) — Implementation notes (rework-corrected: 9-call log claim was an overstatement caught by review; test now asserts the full 10-call log)
- [TASK-260715-3uft8j_spawn-log_-reviewer--reviewer--claude-.log](file://TASK-260715-3uft8j/TASK-260715-3uft8j_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-3uft8j_review.md](file://TASK-260715-3uft8j/TASK-260715-3uft8j_review.md) — Review verdict: changes requested. Confirmed recorder defect (repro), AC coverage gap on Call::Fetch, doc contradictions; claim verification table (gates/sizes/mutation all confirmed)
- [TASK-260715-3uft8j_repro-clear-interactions.rs](file://TASK-260715-3uft8j/TASK-260715-3uft8j_repro-clear-interactions.rs) — Minimal public-API reproduction: clear_interactions() misattributes a successful root() outcome as Cancelled when a fetch is in flight across the clear
- [TASK-260715-3uft8j_rework-results.md](file://TASK-260715-3uft8j/TASK-260715-3uft8j_rework-results.md) — Rework results: recorder epoch fix (mutation-verified + reviewer repro re-run), Fetch arg assertions, doc corrections, Changes/Thumbnail fault coverage, assertion tightening, Outcome::Failed delivered decision
- [TASK-260715-3uft8j_rework-review.md](file://TASK-260715-3uft8j/TASK-260715-3uft8j_rework-review.md) — Rework review verdict: accepted. All 6 items verified independently (own repro + own mutation test); make check 8/8; 151 tests
- [TASK-260715-3uft8j_rework-review-make-check.log](file://TASK-260715-3uft8j/TASK-260715-3uft8j_rework-review-make-check.log) — Reviewer's independent make check re-run: 8/8 green, exit 0
