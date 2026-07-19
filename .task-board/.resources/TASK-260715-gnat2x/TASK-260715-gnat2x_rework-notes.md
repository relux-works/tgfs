# TASK-260715-gnat2x — Rework: launch must not auto-run destructive repair

**Rework of:** review verdict `TASK-260715-gnat2x_review-verdict.md` (CHANGES
REQUESTED). Primitives were accepted; only the launch integration was wrong.
**Remediation chosen:** **Option A** (matches spec + retained doc). The
remover/registrar split and the removal/repair primitives are unchanged.

## What the review flagged

1. `CompanionMain.init()` had been switched from add-only
   `DomainStartupReconcile.run()` to full `DomainRepair.run()`, so **stray
   removal ran automatically on every launch** — reversing the code's own
   documented safety invariant (`DomainRepair.swift` still asserted "repair is
   user-triggered / auto-teardown-at-launch is the failure mode").
2. The "only genuine orphans / fails closed" claim was **not** fail-closed for a
   *spurious-empty* canonical read: an empty account list is a normal,
   non-throwing answer (`shared_state.rs: accounts()`), so an empty-but-present
   DB (App Group id change across upgrade; externally reset state dir) made
   **every** registered domain a "stray" and auto-repair would tear them all
   down. Empty does not throw, so fail-closed-on-throw did not cover it.
3. SYNC-070 (automatic startup recovery) and SYNC-071 (**user-triggered**
   repair) were collapsed into one automatic path; the destructive
   empty-desired / non-empty-registered branch had **zero** test coverage.
4. Secondary: `DomainStartupReconcile` became dead production code; README:88
   framing was stale.

## Changes

### Launch reverted to add-only (SYNC-070)
- `CompanionMain.init()` now runs `DomainStartupReconcile.run()` again
  (add/rename only — never removes). Comment rewritten to state the invariant:
  auto-teardown at launch is the failure mode the split prevents.

### Repair wired behind an explicit user action (SYNC-071)
- `CompanionMain` adds a `.commands { CommandGroup(after: .appInfo) { Button
  "Repair File Provider Domains…" } }` that calls a new
  `GramDriveCompanionApp.repairFileProviderDomains()` — runs `DomainRepair.run()`
  off the main thread. Domain management must run in the app that embeds the
  extension (PLAT-MAC-001), so this app-side repair lives in the shell process,
  distinct from the engine-side `RepairViewModel` (which repairs the *source*
  via the agent control channel). The action logs the withheld-teardown case.

### Total-teardown guard (the destructive branch)
- New `TotalTeardownPolicy { refuse, allow }`; `DomainRepair.repair(...)`,
  `repair(store:...)`, `run(dataRoot:...)`, `run()` all take
  `totalTeardown: .refuse` by default.
- Guard: when `desired.isEmpty && !plan.strays.isEmpty && policy == .refuse`,
  repair **withholds** every stray removal (removes nothing) and reports them in
  the new `DomainRepairOutcome.withheldStrays` / `withheldTotalTeardown`. The
  guard is narrow — a genuine orphan alongside a live account (`desired`
  non-empty) is still cleaned. The genuine last-account logout removes its
  domain through the targeted `DomainRemoval` flow, not by repair inferring
  teardown from emptiness.

### Docs reconciled
- `DomainRepair.swift` rationale now matches behavior (launch = add-only;
  repair = user-triggered; total-teardown guarded).
- README: `DomainStartupReconcile` row no longer claims the app runs the
  "superset" repair at launch; `DomainRepair` row + prose describe the
  user-triggered action and the total-teardown guard; verification bullet
  updated.

## Tests (new, in `DomainRemovalRepairTests.swift`)
- `refusesTotalTeardown` — empty accounts + 2 registered domains ⇒ nothing
  removed, both withheld, registry untouched.
- `allowedTotalTeardownRemovesAll` — same setup, `.allow` ⇒ both removed.
- `straysRemovedWhenAnAccountRemains` — proves the guard is narrow.
- `runRefusesTotalTeardown` — the branch through the `run(dataRoot:)` app entry
  over a real empty substitute container.
- `failureReportsFailed` updated to pass `.allow` so it still reaches the
  failing remover (proving remover failure surfaces as `.failed`).

## Verification
- `swift build` — clean; `GramDriveCompanionMain` compiles (the `.commands`
  wiring and the guard).
- `swift test` — **140/140 in 29 suites** (was 136; +4 total-teardown tests).
- `make check` — **8/8** (Rust workspace + repo gates; unchanged by this
  Swift-only rework, re-run green).
- No files committed (no-auto-commit policy).
