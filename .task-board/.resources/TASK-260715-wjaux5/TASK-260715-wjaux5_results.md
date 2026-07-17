# TASK-260715-wjaux5 — Account logout and local removal workflow

**Status:** ready for review
**Requirement:** SEC-004 (documented secure cleanup sequence for credentials, session/database files, provider registrations, partial transfers, cached content).

## What was built

A crash-resumable, idempotent account-removal workflow in the local TDLib
source crate: `crates/gramdrive-source-tdjson/src/removal.rs` +
`src/removal/journal.rs`, re-exported from the crate root and documented in
the crate README.

`AccountRemoval` sequences the SEC-004 cleanup as a driver loop —
`next_pending` → perform the stage's effect → durably `complete` it — behind a
durable journal:

1. `SignalQuiesce` — cancel in-flight transfers / unregister provider state
2. `TerminateSession` — submit `logOut` (RevokeSession) or `close` (LocalOnly), await `authorizationStateClosed`
3. `WipeDatabase` — remove the TDLib database + files subtree
4. `WipeExports` — remove cached export directories (omitted under `ExportPolicy::Retain`)
5. `RevokeKeychain` — drop the account's database key from secure storage
6. `PurgeState` — delete the account's state rows

## Key design decisions

- **Telegram logout vs local-only removal is explicit** (`RemovalMode`).
  `RevokeSession` → `logOut` (server-side authorization terminated; TDLib also
  deletes its own DB before closing). `LocalOnly` → `close` (server session
  left intact; only local state torn down — the offline / "forget the account"
  path). Every stage after the session step is identical between the two.

- **Crash-resume via a durable journal** under `root/.gramdrive-removal/account-<id>.json`,
  deliberately *outside* the per-account subtree so `WipeDatabase` cannot
  delete the record of its own progress. Written atomically (temp + fsync +
  rename, best-effort dir fsync). `finalize` removes it last → no trace.

- **Effect-before-record invariant + idempotent stages** make the driver
  crash-safe by construction: a crash after an effect but before its record
  re-runs the idempotent effect on resume; a crash after the record skips it.
  There is no window where a stage is neither redone nor skipped.

- **Layering honesty (no forced fit).** `gramdrive-source-tdjson` (layer 1) may
  depend only on `gramdrive-model`/`gramdrive-source`. Two stages act on crates
  above it — cancelling transfers/unregistering provider state
  (`gramdrive-engine`, layer 2) and purging state rows (`gramdrive-state`,
  composed at layer 2/3). Rather than invert the dependency direction or fake
  those crates, `SignalQuiesce` and `PurgeState` are **typed directives** the
  composing caller (engine/FFI) executes; the workflow sequences and
  checkpoints them. The stages this crate owns (session request, on-disk wipe,
  keychain revocation, journal) it runs directly.

- **Concurrency fails safe.** `AccountRemoval::guard_open` refuses
  (`RemovalError::InProgress`) while a removal is in flight, so a concurrent
  open never observes a half-wiped account. `begin` adopts an in-progress
  removal instead of starting a second one.

- **Cached exports retained or discarded by explicit user choice**
  (`ExportPolicy`): `Retain` omits the export-wipe stage; `Discard` removes the
  host-registered export directories with everything else.

## Tests

- Unit (`src/removal.rs`, `src/removal/journal.rs`): plan ordering + exports
  omission, mode→request mapping, next-pending walk, token round-trips,
  journal write/read/remove/list round-trips, atomic write leaves no temp,
  malformed journals fail closed.
- Integration (`tests/account_removal.rs`):
  - `full_removal_leaves_no_trace_of_the_account_on_disk` — fixture scan finds
    no account trace anywhere under the root; siblings untouched.
  - `removal_resumes_from_a_crash_at_every_stage` — crash at each of the 7
    stage boundaries, recover via `pending`, converge identically.
  - `owned_stages_are_idempotent_under_repeat` — every owned executor run twice.
  - `concurrent_access_during_removal_fails_safe` — a reader thread sampling
    `guard_open` 200× during the destructive window is always refused;
    `begin_adopts_an_in_progress_removal_instead_of_starting_a_second`.
  - `retain_keeps_the_exports_but_removes_everything_else`.
  - `each_mode_builds_the_session_request_the_runtime_accepts` — `logOut`/`close`
    round-tripped through the real runtime over the deterministic mock.

## Verification

- `make check` — 8/8 gates green (toolchain, format, lint, test, architecture,
  supply-chain, traceability, scripts). Provenance: `.temp/acceptance/local-all`.
- `cargo test -p gramdrive-source-tdjson` — all pass (56 lib unit tests + the
  new `account_removal` integration binary).

## Follow-up / integration contract (not this task)

- The account-open path (native adapter / `DriveSource` wiring, not built yet)
  must call `AccountRemoval::guard_open` before `AccountConfig::resolve`, and
  run `AccountRemoval::pending` on startup to finish interrupted removals.
- The engine/FFI composition supplies the `SignalQuiesce` and `PurgeState`
  effects when it wires `gramdrive-engine`/`gramdrive-state` to this source.
