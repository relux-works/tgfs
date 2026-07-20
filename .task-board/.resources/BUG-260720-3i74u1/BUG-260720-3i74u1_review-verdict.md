# BUG-260720-3i74u1 — Review verdict: CHANGES REQUESTED (→ to-dev)

Reviewer run RUN-260719-5b6015, 2026-07-20. Read-only review; no code modified.

## Verdict

The control channel implementation is high quality and architecturally clean, and
every implementer verification claim I re-checked independently held up. Not
accepted yet: three confirmed majors (two Rust lifecycle/concurrency, one
security in dev tooling) need a rework pass. Status routed `to-dev` — the rework
is ordinary and autonomous, so `blocked` would be wrong; the separately-verified
external Telegram blocker (below) stays on record for the next cycle.

## Independently verified this review (all green)

- `swift test` (apple/GramDriveSupport): 275 tests / 49 suites passed — re-run, not
  taken on faith. Includes ControlChannelTests (real server+client over real
  sockets), LiveControlTests (ensurer contract, removal both halves), rewritten
  LiveBackendTests.
- `make check`: 8/8 (toolchain, format, lint, test, architecture, supply-chain,
  traceability, scripts) — re-run.
- Packaging: `.temp/app-packaging/GramDrive.app` signed Developer ID
  (262RZ595FP), hardened runtime; `libtdjson.dylib` staged in
  Contents/Frameworks; shipped agent + app reference `@rpath/libtdjson.dylib`
  with `@executable_path/../Frameworks` rpath (verified `otool -L`/`-l`);
  GramDrive-0.1.0.dmg built 03:00.
- `notWired` unreachable from the live backend: grep confirms it survives only in
  the enum definition and preview/test support (`CompanionPreviewSupport.swift`).
- Agent auto-start honors the login-item preference (SMAppService vs direct
  spawn, never silently upgraded) — code + tests confirm.
- External blocker verified at the primary source via GitHub API
  (tdlib/td#3361): levlam 2025-06-14 "The test phone numbers don't work anymore
  for regular users"; 2025-08-25 "You need to create the account using an
  official mobile app first." Implementer's quotes are verbatim.
- Live E2E evidence is real: `.temp/BUG-260720-3i74u1/diag/agent8.log` has 36×
  `PHONE_CODE_INVALID` and connections to Telegram test-DC IPs (149.154.x);
  smoke logs show the full typed flow to `wait-code`.
- Rust architecture gate passes; `gramdrive-ffi → gramdrive-source-tdjson` is an
  allowed edge; exported FFI vocabulary stays provider-neutral (DEC-003).
  `purge_account` cascade coverage verified against v1/v2 schemas with
  `foreign_keys` on. `cfg(real_tdjson)` gating sound (no mock reachable from
  exports, hermetic path fails truthfully).

## Changes requested (fix before next review)

1. **major — crates/gramdrive-ffi/src/auth.rs (~609)**: `AuthSession` has no
   `Drop` impl (confirmed: only `ScopeGuard` and a test helper implement Drop).
   A host dropping the object without `close()` leaks the pump thread spinning
   on `recv_timeout` and permanently holds the sign-in slot `ScopeGuard`,
   blocking every future sign-in in the process. Swift call sites do close
   today; add `Drop → close()` as defense in depth.
2. **major — crates/gramdrive-ffi/src/auth.rs:919-951 (`persist_account`)**:
   finalization stores the real account's vault key, wipes and renames its
   storage dir while holding only the SIGN_IN_SLOT scope guard. A concurrent
   `probe_authorization` / `remove_account` of the same account id (each under
   its own scope) races the wipe/rename — hole in the documented one-op-per-
   account-scope invariant (reachable: Repair clicked while sign-in finalizes).
   Acquire the real account's scope during finalization.
3. **major (security) — .scripts/keychain/provision-telegram-credentials.swift
   (`--exec` mode, ~58-74)**: promptless secret-exfiltration primitive. The
   compiled tool is team-signed, names itself in the items' ACL, sits in
   `.temp/keychain-provision/`, and has zero callers in the repo — any local
   process can run `--exec sh -c 'echo $GRAMDRIVE_API_HASH'`-style invocations
   and defeat both keychain gates the 0330 fix exists to respect. Remove the
   mode (at minimum drop the tool's self-trust from the ACL).
4. **should — .scripts/apple-app/build_app_bundle.py (`embed_runtime_libraries`,
   ~834-857)**: the `-change <staged path>` fixup silently no-ops if the core
   artifact was relocated after staging (LC_LOAD_DYLIB no longer matches), and
   the result still runs on the build machine while failing everywhere else.
   This build is correct (verified), but nothing gates it: add a post-embed
   `otool -L` assertion that shipped Mach-Os contain no absolute
   staged/Homebrew references.

### Minors (same pass or trail — implementer's judgment)

- auth.rs:536-547: `OnceLock` caches a transient `TdRuntime::start` failure
  forever → all sign-ins fail `SourceUnavailable` until process restart.
- auth.rs:217-240: `AuthCommand` derives `Debug` with plaintext code/password —
  one `debug!("{command:?}")` from leaking a 2FA password; manual-redact like
  `VaultApiCredentials`.
- auth.rs:759: `Starting` phase emitted synchronously on the constructor
  caller's thread, contradicting the listener contract's "never a platform main
  thread".
- auth.rs:571: scope key is the raw uncanonicalized `data_dir`; slot key
  namespace is global while the guard is per-data-dir.
- removal.rs:143-183: `terminate_session` inner wait can ~double the stated
  deadline.
- run_control_auth_smoke.py:257-264: a `failed` event during submit-wait is
  ignored → raw socket.timeout traceback instead of a clean skip; operator
  `--phone` number and account row logged unmasked to stdout.
- provision_telegram_credentials.py:117-121: old keychain items deleted before
  the tool proves runnable — a tool failure destroys the only local copies.
- No unit coverage for `core_tdjson_linked` / `embed_runtime_libraries` /
  Frameworks signing loop (core-artifacts side did get tests).
- Untested FFI paths: `Failed` finalization codes, `Cancel` in `Unsupported`.

## External blocker (stands, human-only, NOT this verdict's status)

Telegram retired shared-test-number auto-codes for third-party api ids
(tdlib/td#3361 — verified verbatim). The last AC hop ("Start Sign In drives the
real TDLib auth flow end-to-end with session persisting across restart") cannot
be proven by any agent autonomously. Every other hop is proven live from the
shipped bundle. Path forward already implemented by the dev
(`run_control_auth_smoke.py --phone`):

1. A human creates a test-DC account once with a real number via an official
   Telegram app in test mode.
2. `python3 .scripts/smoke/run_control_auth_smoke.py --phone +<number>` — one
   interactive code entry; kept container makes restart/repair legs and future
   re-runs unattended.

Decision the orchestrator/human owes the next review cycle: run that one-time
bootstrap, OR amend the AC to accept the layered live evidence and spawn a
successor provisioning task (TASK-260716-1iypv4's assumption is dead
server-side). Until one of these happens, checklist item 3 stays unchecked and
the next reviewer will face the same hop.

## Checklist disposition set by this review

- Item 12 "Solution fits project architecture" — checked (verified).
- Item 13 "Tests green" — checked (verified by re-run).
- Item 11 "Implementation matches AC" — left unchecked (E2E hop unproven).
- Item 14 verdict-evidence — checked (this artifact + notes + LOGBOOK 0338).
