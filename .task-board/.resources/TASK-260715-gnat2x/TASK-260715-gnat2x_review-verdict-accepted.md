# TASK-260715-gnat2x — Review verdict (rework): ACCEPTED (→ done)

**Reviewer:** reviewer (claude) · **Date:** 2026-07-19
**Reviews rework of:** `TASK-260715-gnat2x_review-verdict.md` (first pass: CHANGES
REQUESTED → to-dev). Remediation taken: **Option A**.
**Scope reviewed:** the `GramDriveFileProvider` removal/repair layer, the
`CompanionMain` launch + menu wiring, README, LOGBOOK, and the new tests.

## Gates (independently re-run by the reviewer, not trusted from notes)

- `swift test` (apple/GramDriveSupport): **140/140 in 29 suites — PASS**
  (was 136; +4 total-teardown branch tests). Build clean.
- `make check` (repo root): **8/8** — toolchain, format, lint, test (cargo),
  architecture, supply-chain, traceability, scripts all green. (Rust workspace;
  the changeset is Swift-only, so `swift test` is the real gate — also green.)

## Every blocking point from the first review is resolved

1. **Launch no longer auto-runs destructive repair.** `CompanionMain.init()`
   runs the add-only `DomainStartupReconcile.run()` again (CompanionMain.swift:43)
   — add/rename only, never removes. The self-contradiction with the retained
   `DomainRepair` rationale is gone.
2. **Repair is behind an explicit user action (SYNC-071).** A `.commands`
   `CommandGroup(after: .appInfo)` "Repair File Provider Domains…" button →
   `repairFileProviderDomains()` → `DomainRepair.run()` off the main thread
   (CompanionMain.swift:74-95). No launch-time caller of `DomainRepair` remains.
3. **The everything-is-a-stray branch is guarded and tested.** New
   `TotalTeardownPolicy{refuse(default),allow}`. When `desired.isEmpty &&
   !plan.strays.isEmpty && .refuse`, repair withholds every removal (removes
   nothing) and reports `withheldStrays`/`withheldTotalTeardown`
   (DomainRepair.swift:151-158). The guard runs after the adds/renames loop,
   which is a no-op when desired is empty, so there is no partial side effect.
   Tests: `refusesTotalTeardown`, `allowedTotalTeardownRemovesAll`,
   `straysRemovedWhenAnAccountRemains` (proves the guard is narrow),
   `runRefusesTotalTeardown` (through the real `run(dataRoot:)` entry).
   `failureReportsFailed` now passes `.allow` so it still reaches the failing
   remover — remover failure still surfaces as `.failed`.
4. **Docs reconciled; `DomainStartupReconcile` is a live caller again.**
   `DomainRepair.swift` rationale, README rows/prose, and the launch comment all
   match actual behavior (launch = add-only; repair = user-triggered + guarded).
   No dead production code, no self-contradicting safety doc.

## Verified against the design, not just the diff

- `desiredDomains(for: [])` returns `[]` (DomainIdentity.swift:75), so an empty
  account read makes every registered domain a stray with empty adds/renames —
  exactly the spurious-empty vector the guard now refuses. `wasSettled` is
  correctly `false` for a withheld teardown (`plan.isSettled` is true but
  `plan.strays` is non-empty).
- No destructive path is reachable from the UI: the menu action calls `run()`
  with the default `.refuse`; `.allow` exists only for a future
  explicitly-confirmed teardown and is never wired to a bare menu click.
- Corruption still fails closed (corrupt DB throws on provider open → `.failed`
  → nothing touched); the empty-but-present read is now covered by the guard.

## Scope / AC / DoD

- Scope — logout (targeted `DomainRemoval`), local-only removal
  (`DomainDataDisposition` preserve/delete), corrupt registration (repair strays
  / fail-closed), uninstall guidance (README **Uninstall cleanup**): all present.
- AC — idempotent + interruption-safe, covered: idempotence and
  interruption-at-each-step (mid-add, mid-stray-removal) and
  re-registration-after-crash are genuinely tested. The live
  `NSFileProviderManager` path is proven on device by the signing/packaging task
  (TASK-260715-1dk9ik), consistent with sibling task 3s44pc — a documented
  platform boundary (PLAT-MAC-001), not a gap.
- Ownership boundary held: the read-only Swift store does not delete account
  rows; the engine-side SEC-004 halves stay behind the `notWired` channel. No
  stub/flag was added to fake the engine — no forced fit.

## Minor, non-blocking (no rework required)

- The menu action only logs; it surfaces neither the withheld-teardown nor the
  preserved-downloads location to the user. That UI belongs to the companion-app
  story's view models (correctly deferred in results.md), and the review only
  asked for an explicit trigger, which the menu command satisfies.
- The guard is scoped to empty-desired only; a hypothetical *partial* spurious
  read (e.g. 1 of N accounts) would still clean the other N−1 as strays. This
  matches the review's exact ask, strays preserve downloads by default, and it
  can now only happen under an explicit user-triggered repair — not at launch.
  Acceptable; not in this task's scope.

## Verdict

**ACCEPTED → `done`.** The rework is precise: it fixes exactly the launch
integration the first review flagged, closes the spurious-empty vector with a
guarded + tested branch, reconciles the docs, and restores
`DomainStartupReconcile` to a live caller — without touching the already-accepted
removal/repair primitives. Both gates independently green. No new defects found.
