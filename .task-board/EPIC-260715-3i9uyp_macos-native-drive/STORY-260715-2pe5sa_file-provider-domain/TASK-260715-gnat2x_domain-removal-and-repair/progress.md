## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-15T00:52:47Z

## Last Update
2026-07-19T12:44:23Z

## Blocked By
- (none)

## Blocks
- (none)

## Checklist
- [x] Domain removal and repair flows: unregister domain, reconcile system registration vs app state, preserve-or-delete local data per explicit user choice; idempotent and interruption-safe
- [x] Repair rebuilds provider state from canonical store without data loss; tests cover interruption at each step and re-registration after crash
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
spawn queued: [implementer] developer (claude) (run=RUN-260719-f46042, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260719-f46042)
Plan: domain removal+repair lives in GramDriveFileProvider (parallel to DomainReconciler). Deliverables: (1) DomainRemover seam + SystemDomainRemover over NSFileProviderManager.remove(_:mode:), DomainDataDisposition (deleteLocalData->removeAll, preserveDownloads->preserveDownloadedUserData, returns preserved URL). (2) DomainRemoval: targeted idempotent per-account domain unregister (SEC-004 provider-registration cleanup step). (3) DomainRepair: reconcile registered vs canonical account store — adds/renames via registrar (rebuild w/o data loss via stable id) + stray removal via remover (SYNC-070/071), idempotent + interruption-safe, plus never-throwing run(dataRoot:)/run() app entries. (4) Tests: idempotency, interruption at each step, re-registration after crash, disposition mapping. (5) README + uninstall guidance (PLAT-004/SEC-004). Ownership boundary: domain/provider-registration side only; engine-side account-row wipe stays in agent (control channel notWired).
READY FOR REVIEW. Delivered domain removal/repair in GramDriveFileProvider: DomainRemover/SystemDomainRemover (removal seam, separate from remove-free DomainRegistrar), DomainDataDisposition (deleteLocalData->removeAll, preserveDownloads->preserveDownloadedUserData), DomainRemoval (idempotent per-account unregister), DomainRepair (reconcile + stray resolution, never-throwing run() entries). Interruption-safe by ordering (row-first removal; adds-before-stray-removal repair; fail-closed on unreadable store). Wiring: CompanionMain launch pass upgraded add-only reconcile -> full DomainRepair.run() (SYNC-070 self-heal, no data loss, orphans-only). Docs: README removal/repair section + uninstall guidance (PLAT-004/SEC-004). Ownership: engine-side SEC-004 halves (logout/on-disk wipe) stay in agent behind notWired control channel (separate story); Swift store is read-only, so no account-row deletion here. Verification: make check 8/8 (.temp/TASK-260715-gnat2x/make-check-01.log); swift build clean; swift test 136/136 in 29 suites (17 new across 3 suites incl. interruption-at-each-step + re-registration-after-crash). Nothing committed.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-f46042, pid=10462, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260719-da3987, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260719-da3987)
REVIEW: CHANGES REQUESTED -> to-dev. Gates re-run green (swift test 136/136 in 29 suites; make check 8/8 Rust core — but the whole changeset is Swift, so swift test is the real gate). Removal/repair PRIMITIVES accepted: clean remover-vs-registrar seam split, correct disposition->mode mapping, idempotent per-account removal, repair adds-before-stray-removal, interruption-at-each-step + re-registration-after-crash tested, ownership boundary respected (engine-side SEC-004 halves left to agent behind notWired channel — no forced fit). BLOCKING: CompanionMain launch pass switched add-only DomainStartupReconcile.run() -> full DomainRepair.run() (CompanionMain.swift:42), so stray removal now runs automatically every launch. This contradicts the retained rationale in DomainRepair.swift:64-68 (repair is user-triggered; auto-teardown-at-launch is the failure mode the split guards against). The fails-closed / only-orphans claim is NOT fail-closed: Rust accounts() treats an empty list as a normal answer, not an error, so a spurious-empty read (App Group ID change on upgrade [PLAT-004], or a state-dir reset) makes EVERY registered domain a stray and auto-repair tears them all down. Spec SYNC-070 (auto reconcile) vs SYNC-071 (user-triggered repair) collapsed into one auto path. The destructive branch (empty desired + non-empty registered) is untested — emptyRepairSettles/emptyContainer both have zero registered domains. Secondary: DomainStartupReconcile is now dead production code (README:88 still frames it as a building block). Fix Option A: revert launch to add-only reconcile + wire repair behind explicit user action (matches spec+doc). Option B: guard the everything-is-a-stray case + add a test + fix the contradicting doc. Both autonomous. Full evidence: outcome resource TASK-260715-gnat2x_review-verdict.md
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-da3987, pid=19230, exit=0)
spawn queued: [implementer] developer (claude) (run=RUN-260719-ad15b9, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260719-ad15b9)
Rework (Option A) complete — ready for review. Launch reverted to add-only DomainStartupReconcile.run() (SYNC-070); DomainRepair.run() wired behind explicit CompanionMain command "Repair File Provider Domains…" (SYNC-071). Total-teardown guard: TotalTeardownPolicy{refuse(default),allow}; empty desired + non-empty registered withholds all stray removals (DomainRepairOutcome.withheldStrays / withheldTotalTeardown) unless explicitly confirmed. Guard is narrow (orphan alongside live account still cleaned). Docs reconciled (DomainRepair rationale + README). Tests: swift test 140/140 (+4 destructive-branch tests: refusesTotalTeardown, allowedTotalTeardownRemovesAll, straysRemovedWhenAnAccountRemains, runRefusesTotalTeardown; failureReportsFailed uses .allow). make check 8/8. See TASK-260715-gnat2x_rework-notes.md.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-ad15b9, pid=22461, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260719-a1b8d3, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260719-a1b8d3)
ACCEPTED (rework, Option A). All 4 blocking points from the first review resolved: (1) launch reverted to add-only DomainStartupReconcile.run(); (2) DomainRepair.run() wired behind explicit menu action Repair File Provider Domains (SYNC-071), no launch-time caller; (3) total-teardown guarded (TotalTeardownPolicy.refuse default withholds every stray when desired.isEmpty && registered non-empty) + 4 new tests exercising the destructive branch; (4) docs reconciled, DomainStartupReconcile is a live caller again (no dead code, no self-contradicting safety doc). Gates independently re-run by reviewer: swift test 140/140 in 29 suites, make check 8/8. Verdict evidence: TASK-260715-gnat2x_review-verdict-accepted.md. Routing to done.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-a1b8d3, pid=27746, exit=0)

## Precondition Resources
- [TASK-260715-gnat2x_rework-scope.md](file://TASK-260715-gnat2x/TASK-260715-gnat2x_rework-scope.md) — Rework: launch must not auto-run destructive repair

## Outcome Resources
- [TASK-260715-gnat2x_spawn-log_-implementer--developer--claude-.log](file://TASK-260715-gnat2x/TASK-260715-gnat2x_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-gnat2x_results.md](file://TASK-260715-gnat2x/TASK-260715-gnat2x_results.md) — Implementation results (reworked, Option A): launch add-only reconcile, user-triggered repair, total-teardown guard
- [TASK-260715-gnat2x_make-check.log](file://TASK-260715-gnat2x/TASK-260715-gnat2x_make-check.log) — make check suite 'all' 8/8 passed
- [TASK-260715-gnat2x_spawn-log_-reviewer--reviewer--claude-.log](file://TASK-260715-gnat2x/TASK-260715-gnat2x_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-gnat2x_review-verdict.md](file://TASK-260715-gnat2x/TASK-260715-gnat2x_review-verdict.md) — Reviewer verdict: changes requested — launch wiring runs full DomainRepair automatically, reversing the documented user-triggered/add-only safety invariant; unguarded empty-accounts teardown; untested destructive branch. Primitives accepted.
- [TASK-260715-gnat2x_rework-notes.md](file://TASK-260715-gnat2x/TASK-260715-gnat2x_rework-notes.md) — Rework notes: Option A — launch add-only reconcile, user-triggered repair, total-teardown guard
- [TASK-260715-gnat2x_swift-test-rework.log](file://TASK-260715-gnat2x/TASK-260715-gnat2x_swift-test-rework.log) — swift test after rework: 140/140 in 29 suites
- [TASK-260715-gnat2x_make-check-rework.log](file://TASK-260715-gnat2x/TASK-260715-gnat2x_make-check-rework.log) — make check after rework: 8/8
- [TASK-260715-gnat2x_review-verdict-accepted.md](file://TASK-260715-gnat2x/TASK-260715-gnat2x_review-verdict-accepted.md) — Rework review verdict: ACCEPTED (Option A) — launch add-only, repair user-triggered + total-teardown guard; gates independently green (swift test 140/140, make check 8/8)
