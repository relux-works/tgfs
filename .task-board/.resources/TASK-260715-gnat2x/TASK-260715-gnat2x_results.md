# TASK-260715-gnat2x — Implement domain removal and repair

**Status:** ready for review (`to-review`)
**Scope:** File Provider domain removal & repair for the macOS native drive
(story STORY-260715-2pe5sa, *File Provider domain lifecycle*).

> Reworked per review `TASK-260715-gnat2x_review-verdict.md` (Option A). The
> removal/repair primitives were accepted unchanged; the launch integration was
> corrected. See `TASK-260715-gnat2x_rework-notes.md` for the rework detail.

## What was built

The domain-*lifecycle* half of the story: registration (TASK-260715-3s44pc)
only ever adds; this task adds the capability to take a domain **away** and to
**repair** the registered set against the canonical account store. All new code
lives in the `GramDriveFileProvider` target of `apple/GramDriveSupport`,
parallel to the reconciler, and is tested against fakes exactly as the
reconciler is (the live `NSFileProviderManager` path is proven by the
signing/packaging task, not unit tests — PLAT-MAC-001).

### New source files

- `Sources/GramDriveFileProvider/DomainRemover.swift`
  - `DomainDataDisposition` — the preserve-or-delete-per-user-choice decision
    (PLAT-MAC-004; SEC-004): `deleteLocalData` → platform `removeAll` (trace-free
    wipe), `preserveDownloads` → `preserveDownloadedUserData` (system moves the
    user's files aside and returns where). The third platform mode
    (`preserveDirtyUserData`) is deliberately omitted — read-only V1 never has
    dirty user data.
  - `DomainRemovalOutcome` — identifier, `wasRegistered` (the idempotent no-op
    signal), disposition, and the preserved-data location.
  - `DomainRemover` protocol + `SystemDomainRemover` — the **removal seam**,
    deliberately separate from `DomainRegistrar` (which has no remove at all),
    so "registration never destroys Finder state" stays structural. Live impl
    wraps `NSFileProviderManager.remove(_:mode:)`.
- `Sources/GramDriveFileProvider/DomainRemoval.swift`
  - `DomainRemoval.removeAccountDomain(accountId:disposition:registrar:remover:)`
    and `remove(identifier:...)` — targeted, idempotent per-account removal (the
    provider-registration step of the SEC-004 sequence). Reads the registered
    set, removes the one matching the account's stable identifier if present,
    reports a no-op when already gone. This is the flow the genuine last-account
    logout uses to remove its domain.
- `Sources/GramDriveFileProvider/DomainRepair.swift`
  - `DomainRepair.repair(accounts:/store:registrar:remover:strayDisposition:totalTeardown:)` —
    `DomainReconciler` plus stray resolution: re-register lost domains (recover
    Finder state under the stable identifier — rebuild without data loss,
    SYNC-071) and remove strays no account row explains (default
    `preserveDownloads` — orphan cleanup still keeps files).
  - `TotalTeardownPolicy { refuse, allow }` + `DomainRepairOutcome.withheldStrays`
    / `withheldTotalTeardown` — the guard for the everything-is-a-stray case.
  - `DomainRepair.Outcome` + `run(dataRoot:)` / `run()` — never-throwing app
    entry points mirroring `DomainStartupReconcile`.

### Wiring (Option A: launch add-only, repair user-triggered)

- `Sources/GramDriveCompanionMain/CompanionMain.swift`
  - **Launch (SYNC-070):** `init()` runs the add-only
    `DomainStartupReconcile.run()` off the main thread — it re-registers every
    account's domain but **never removes**. Auto-teardown at launch is precisely
    the failure mode the reconcile/repair split prevents (an empty or partial
    canonical read at startup would otherwise make every domain look like a
    stray and wipe it).
  - **Repair (SYNC-071):** `DomainRepair.run()` is wired behind an explicit
    `.commands` menu action — **"Repair File Provider Domains…"** → the static
    `repairFileProviderDomains()`. Because repair can *remove* domains it never
    runs automatically. Domain management must run in the app that embeds the
    extension (PLAT-MAC-001), so this app-side repair lives in the shell
    process, distinct from the engine-side `RepairViewModel` (which repairs the
    *source* through the agent control channel).

### Docs

- `apple/GramDriveSupport/README.md` — the "Domain removal, repair, and cleanup"
  section (types table + the launch=add-only / repair=user-triggered behavior
  and the total-teardown guard) and an **Uninstall cleanup** subsection
  (PLAT-004 / SEC-004): macOS does not unregister a provider's domains when the
  app is deleted, so clean uninstall is *remove accounts first, then delete the
  app*; leftover registrations are reconciled by reinstall + user-triggered
  repair. Verification bullet updated with the new suites.

## Interruption safety, idempotency & the total-teardown guard (the core AC)

- **Removal ordering:** the engine drops the canonical account row first, then
  the app removes the domain. A crash between the two leaves a *stray*, which
  repair cleans up — never a domain re-registered for an account that is gone.
- **Repair ordering:** adds/renames run before stray removal, so an interrupted
  pass re-runs into a completed one from either side; a completed pass re-runs
  into a settled no-op.
- **Fail-closed:** repair removes strays only after successfully reading the
  canonical rows; an unreadable store yields `.failed` and touches nothing.
- **Total-teardown guard:** an *empty* account list is a normal, non-throwing
  answer from the canonical store, indistinguishable from a spurious-empty read
  (App Group id change across an upgrade, an externally reset state dir). So
  when the desired set is empty while domains are still registered, repair
  **withholds** every stray removal by default (`TotalTeardownPolicy.refuse`) —
  it does not trust a possibly-spurious empty read to authorize wiping every
  Finder root. Only an explicitly-confirmed teardown (`.allow`) proceeds. The
  guard is narrow: a genuine orphan alongside a live account is still cleaned.

## Ownership boundary (no forced fit)

Domain/provider-registration management is app-side and runs where the extension
is embedded (PLAT-MAC-001). The **engine-side** halves of the SEC-004 sequence —
the server logout and the trace-free on-disk account wipe — belong to the agent
(coordinator role) and are reached through the agent control channel, which is
`notWired` in this build (a separate story). The Swift-side `SharedStateStore` is
read-only by design, so this task does not (and must not) delete account rows.
No stub or flag was added to fake the engine side; the domain layer is complete
and correct on its own, and the removal/repair UI trigger + preserve/delete
toggle belong to the companion-app story's view models (which also need that
control channel for the engine halves to be end-to-end).

## Verification

- `swift build` (all targets) — clean.
- `swift test` — **140/140** in 29 suites (21 tests across the removal/repair
  suites, incl. 4 total-teardown branch tests: `refusesTotalTeardown`,
  `allowedTotalTeardownRemovesAll`, `straysRemovedWhenAnAccountRemains`,
  `runRefusesTotalTeardown`).
- `make check` — 8/8 (Rust core + repo gates; the Swift package is proven by
  `swift test`). Logs: `TASK-260715-gnat2x_swift-test-rework.log`,
  `TASK-260715-gnat2x_make-check-rework.log`.
- No files committed (no-auto-commit policy).

## Spec traceability

PLAT-MAC-004 (domain removal), SEC-004 (provider-registration cleanup step +
uninstall guidance), SYNC-070 (startup recovery = the add-only launch reconcile,
adds/renames only), SYNC-071 (user-triggered repair rebuilds provider state
without data loss and cleans strays, with the total-teardown guard), PLAT-004
(uninstallation cleanup). Story AC: registration/removal idempotent, stale
domains repair, logout/uninstall leaves no broken Finder root.
