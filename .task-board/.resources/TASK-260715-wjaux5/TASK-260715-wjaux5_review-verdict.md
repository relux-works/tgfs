# TASK-260715-wjaux5 — Reviewer verdict: ACCEPTED

Reviewer: reviewer (claude). Read-only review of the account-removal workflow
(SEC-004) in `gramdrive-source-tdjson` (src/removal.rs, src/removal/journal.rs,
tests/account_removal.rs, lib.rs re-exports, README).

## AC compliance
- **Every stage idempotent** — owned executors treat NotFound as success
  (wipe_storage, wipe_exports, revoke_keychain via InMemorySecrets.delete_account);
  `complete()` de-dupes; caller directives (SignalQuiesce/PurgeState) idempotent
  by construction. Proven by `owned_stages_are_idempotent_under_repeat`.
- **Partial failure resumes** — durable journal under `root/.gramdrive-removal/`,
  deliberately outside the wiped subtree, written atomically (temp+fsync+rename,
  best-effort dir fsync). Effect-before-record invariant. `removal_resumes_from_a_crash_at_every_stage`
  crashes at all 7 stage boundaries and converges identically.
- **Logout vs local-only distinguished** — RemovalMode::RevokeSession→`logOut`
  (server session revoked), LocalOnly→`close` (server session kept). `begin`
  adopts an in-progress journal and refuses to downgrade the mode.

## DoD / gates
- `make check` 8/8 green (toolchain, format, lint -D warnings, workspace test
  13.7s, architecture, cargo-deny supply-chain, traceability, scripts).
  Provenance: .temp/acceptance/local-all. Re-verified in review.
- `cargo test -p gramdrive-source-tdjson`: all pass incl. account_removal 7/7.
- clippy -D warnings on crate: exit 0. `cargo fmt --check`: exit 0.
- No-trace fixture scan (`full_removal_leaves_no_trace_of_the_account_on_disk`,
  siblings untouched) and concurrent-access-fails-safe (200 guard_open samples
  all InProgress during the destructive window) both present and green.

## Architecture fit
- Layering verified against crates/README.md + Cargo.toml: crate is layer 1,
  depends only on gramdrive-model. SignalQuiesce (cancel transfers / unregister,
  engine=layer 2) and PurgeState (state rows, gramdrive-state composed at 2/3)
  are typed directives the composing caller executes — the workflow still
  sequences+checkpoints them durably. This is the correct seam, enforced by the
  automated `architecture` gate, not a forced fit.
- Follows crate idioms: sans-IO driver, hand-built JSON (matches base64 helper),
  process-id+counter temp/fixture pattern, StorageLayout::wipe_account reuse.

## Non-blocking notes (for follow-up TASK-260715-kxzfy7, blocked-by this)
1. TerminateSession's real submit+await-`authorizationStateClosed`, plus the
   SignalQuiesce/PurgeState effects, are the composing caller's job (engine/FFI/
   DriveSource, not built yet). `resolve` today is called only in tests, so no
   live account-open path bypasses `guard_open` — the guard/pending() wiring is
   a tracked seam, not a current hole.
2. journal::list() fails closed on a single malformed journal, so a corrupt
   record for one account would abort `pending()` recovery for all others.
   Deliberate fail-closed choice for a security cleanup; acceptable, noted.
3. `.gramdrive-removal` creation isn't fsynced before first write (root dir
   entry not synced); documented best-effort, fine on APFS.

Verdict: implementation matches AC, fits the architecture, tests green → done.
