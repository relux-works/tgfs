# TASK-260715-gnat2x — Review verdict: CHANGES REQUESTED (→ to-dev)

**Reviewer:** reviewer (claude) · **Date:** 2026-07-19
**Scope reviewed:** domain removal & repair layer in `GramDriveFileProvider`,
plus the `CompanionMain` launch wiring, README, and LOGBOOK.

## Gates (independently re-run)

- `swift test` (apple/GramDriveSupport): **136/136 in 29 suites — PASS**
  (17 new tests across 3 suites; build clean).
- `make check` (Rust core): **8/8** — but note this exercises the Rust
  workspace only; the entire changeset is Swift, so `swift test` is the real
  gate for this task. Confirmed green.

## What is solid (accepted in substance)

- **Seam design.** `DomainRemover`/`SystemDomainRemover` is deliberately
  separate from the remove-free `DomainRegistrar`, so "registration never
  destroys Finder state" is structural, not disciplined. Correct call.
- **`DomainDataDisposition`.** Correct mapping (`deleteLocalData`→`removeAll`,
  `preserveDownloads`→`preserveDownloadedUserData`); omitting
  `preserveDirtyUserData` for read-only V1 is well-reasoned and tested.
- **`DomainRemoval`.** Idempotent per-account unregister; already-gone is a
  no-op success; delete preserves nothing; preserve surfaces the kept-files
  URL. Remover failure surfaces (no false-success). All covered.
- **`DomainRepair` core.** Reconcile + stray resolution; adds/renames run
  before stray removal so an interrupted pass converges from either side;
  fail-closed when the store read *throws*. Interruption-at-each-step and
  re-registration-after-crash are genuinely tested (mid-add, mid-stray).
- **Ownership boundary — no forced fit.** Engine-side SEC-004 halves (server
  logout, on-disk wipe) correctly left to the agent behind the `notWired`
  control channel; the read-only Swift store does not delete account rows.
  No stub/flag was added to fake the engine side. This is the right restraint.

## Blocking finding — launch wiring reverses a documented safety invariant

`CompanionMain.init()` was changed from add-only `DomainStartupReconcile.run()`
to full `DomainRepair.run()` (CompanionMain.swift:42), so **stray removal now
runs automatically on every launch.** Three problems:

### 1. The code contradicts its own retained safety rationale
`DomainRepair.swift:64-68` still asserts the opposite invariant, verbatim:

> "This is why repair is user-triggered and the startup reconcile is add-only:
> automatically destroying Finder state on every launch is the failure mode the
> split guards against, but an explicit repair the user asked for is allowed to
> clean orphans."

The same changeset that says "repair is user-triggered / auto-teardown-at-launch
is the failure mode to avoid" now runs repair automatically at launch. This is a
self-contradiction: either the wiring is wrong, or the rationale is now false and
misleads the next reader.

### 2. The "only genuine orphans" safety claim is not actually fail-closed
Both the launch comment and `results.md` claim repair "fails closed" and "only
ever tears down genuine orphans." That holds only when the canonical account
read is complete and truthful. The Rust contract
(`shared_state.rs`: `accounts()`) states an **empty account list is "a normal
answer, not an error."** So a *spurious-empty* read — genuinely-empty-but-present
DB while domains are still registered — makes **every** registered domain a
"stray," and auto-repair tears them ALL down on launch. Empty does **not** throw,
so fail-closed does not cover it.

Corruption *does* fail closed (a corrupt DB throws on provider-role open →
`.failed` → nothing touched), so that vector is safe. The remaining
spurious-empty vectors are narrow but real and in-scope:
- **App Group ID change across an upgrade** → new empty container while the old
  build's domains stay registered under the same bundle. PLAT-004 puts upgrades
  in acceptance scope.
- External deletion/reset of the state dir → Rust `open` recreates a fresh empty
  DB → empty accounts → all domains torn down.

(The genuine "last account logged out → 0 accounts → remove its domain" case is
*correct* — the danger is only the spurious-empty read.)

### 3. Spec split collapsed; dangerous branch untested
- SYNC-070 (startup recovery, automatic) and SYNC-071 (**user-triggered**
  repair) are distinct in the spec. The wiring folds the destructive SYNC-071
  behavior into the automatic SYNC-070 path.
- No test covers "empty/partial desired set + non-empty registered set ⇒
  auto-teardown." `emptyRepairSettles` and `emptyContainer` both have **zero**
  registered domains, so the destructive branch is never exercised. The AC
  explicitly requires flows be "covered."

## Secondary findings

- **`DomainStartupReconcile` is now dead production code.** No production caller
  remains after the switch (only its own definition + tests). README:88 still
  calls it "the add-only building block," implying something builds on it —
  stale. Either re-wire it (option A below) or drop the framing.
- **Doc drift** (same as finding #1): `DomainRepair.swift:64-68` must be
  reconciled with actual behavior regardless of which remediation is chosen.

## Remediation (developer's choice — both autonomous, no human decision needed)

- **Option A (matches spec + retained doc):** revert the launch site to add-only
  `DomainStartupReconcile.run()` (SYNC-070 reconcile), and wire the destructive
  `DomainRepair` behind an explicit user-triggered action (SYNC-071) in the
  companion story. Removes the dead-code and doc-contradiction problems for free.
- **Option B (keep auto-repair at launch):** guard the "everything is a stray"
  case — skip stray removal when the desired set is empty but the registered set
  is non-empty (treat total-teardown as suspicious), add a test for that branch,
  AND rewrite `DomainRepair.swift:64-68` so the rationale matches the behavior.

Either way: fix the contradicting doc and add coverage for the empty-desired /
non-empty-registered path.

## Verdict

**Changes requested → `to-dev`.** The removal/repair *primitives* are
well-designed, correct, and thoroughly tested — the rework is narrow: the one
launch-integration decision introduces an unguarded, untested destructive path
that reverses the code's own documented safety invariant and conflicts with the
SYNC-070/071 split. Not `blocked` (no external blocker, no human-only decision).
Not `done` (self-contradicting safety doc + untested destructive branch).
