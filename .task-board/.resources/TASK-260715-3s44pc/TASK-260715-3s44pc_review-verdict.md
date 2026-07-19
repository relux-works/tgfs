# TASK-260715-3s44pc — Review verdict: ACCEPTED → done

Date: 2026-07-19. Role: reviewer (read-only; no code modified).

## Verdict

**Accepted.** Implementation matches the AC, the solution fits the project
architecture, and every quality gate was independently re-run by the
reviewer and came back green.

## What the reviewer independently re-ran

| Gate | Result |
|---|---|
| `make check` (suite all, 8 steps) | **8/8 passed** — provenance `.temp/acceptance/local-all` (second run; see flake note below) |
| `swift test` (apple/GramDriveSupport) | **118/118 in 25 suites** (28 new across 5 domain suites) |
| `make smoke-shared-state` | **PASSED** incl. the new `domains` step |

Cross-process AC proof observed directly in `.temp/shared-state-smoke/domains.log`:
Rust-seeded account 7 → `domain_id=account-7`, `domain_name=GramDrive` →
the real `GramDriveFileProviderExtension` resolves
`context_root=gdaeaqaaaaaaaaaaah` — byte-identical to the seeder's
`account_root`.

Developer-provided logs verified for the remaining gates (recent, same
commit `0d9878f-dirty`): `make smoke-bindings` PASSED (contract 0.3.0
asserted in both Swift and Kotlin consumers), `make smoke-agent-lifecycle`
PASSED (regression), `make package` PACKAGING PASSED (artifact carries
0.3.0).

## AC verification

- **Domain appears once with correct identity/name.** `DomainIdentity`
  makes the identifier `account-<id>` a pure function of the stable
  account id — display name, auth state, and namespace epoch provably play
  no part (dedicated tests). Parse-back is strict by round-trip
  (`account-007`, `account-7x`, overflow, foreign prefixes all refused),
  so a foreign domain can never alias an account. Naming follows
  POL-7/DEC-019 (verified against `.spec/policies.md` § POL-7): single
  account → exactly "GramDrive"; multiple → "GramDrive — <name>" in
  identity order, collisions append the id, blank names fall back to
  "Account <id>".
- **Recovers after app/provider restart.** Registration is durable in the
  system; the companion shell re-runs `DomainStartupReconcile` at every
  launch (off-main, never blocking/failing startup), and the
  repeat-pass test proves a healthy install converges with **zero**
  registrar calls.
- **Scope paths.** First run (registers exactly once), reauthorization
  (identical desired set, zero calls across auth-state changes),
  duplicate install (stable identifier + upserting `add` + idempotent
  pass; foreign registrations survive untouched), multiple accounts
  (identity ordering, second-account arrival renames the first domain,
  deterministic collision disambiguation).

## Architecture fit

- **Registrar seam has no `remove` operation** — "registration never
  destroys Finder state" is structural, not disciplined. Strays are
  reported, never touched; removal/repair stays with TASK-260715-gnat2x.
- **FFI 0.2.0 → 0.3.0 is genuinely additive**: `accounts()`/`account()`
  snapshot reads only, no writes (DEC-006 intact), `secret_ref` never
  crosses the boundary, and the root item id is derived in Rust
  (`ItemKey::Canonical(CanonicalKey::Account(..))`) so hosts never learn
  the identifier scheme. The state store orders accounts by identity in
  SQL (`ORDER BY account_id`, `crates/gramdrive-state/src/repo/accounts.rs`),
  and `desiredDomains` defensively re-sorts.
- **No TDLib in the extension by construction**: `GramDriveFileProvider`
  links only GramDriveSupport + GramDriveCore (`Package.swift`); the
  extension opens shared state read-only as `StateRole.provider`.
  Callbacks refuse honestly: unresolvable domain → `noSuchItem`,
  resolvable-but-unimplemented → `featureUnsupported`, storage failures
  pass through untranslated.
- Layering/architecture gate (`check_crate_architecture.py`) green.

## Findings

1. **Flaky test, unrelated to this task** (worth a deflake follow-up if
   it recurs): `repeated_create_close_cycles_stay_clean`
   (`crates/gramdrive-source-tdjson/tests/runtime_lifecycle.rs:182`)
   failed once during the reviewer's first full `make check` under load
   (`Err(Timeout)` where `Err(Closed)` was expected), then passed 5/5 in
   isolation and green on the immediate full-suite re-run. The crate is
   untouched by this task's diff. Recorded in LOGBOOK 2026-07-19 1605.

## Non-blocking notes

1. Live `SystemDomainRegistrar` registration (signed app + embedded
   .appex) is honestly deferred to packaging TASK-260715-1dk9ik — a real
   platform constraint (domain management resolves the extension from the
   calling app's bundle), documented, not forced. Everything above the
   seam is tested against fakes.
2. Contrived residual display-name collision: an account whose literal
   name ends in " (<other id>)" can collide with another account's
   disambiguated name. Cosmetic only — identifiers stay unique, Finder
   state is unaffected. Not worth rework.

## DoD checklist

Every developer DoD item verified; the three reviewer items
(implementation matches AC, fits architecture, tests green) confirmed by
the evidence above and checked on the board.
