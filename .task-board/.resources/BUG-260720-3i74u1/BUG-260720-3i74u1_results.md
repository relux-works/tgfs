# BUG-260720-3i74u1 — companion↔agent control channel: results

Run 2 (resumed after RUN-260719-f02962 died mid-verification). The control
channel is implemented, tested, and proven live against Telegram's real test
DC from the signed packaged bundle — up to the one hop Telegram has retired
for everyone (details in "External blocker").

## What ships

**Rust (prior session, verified green this run)**
- FFI contract 0.6.0: `AuthSession` (tdjson `AuthMachine` + `TdRuntime`, real
  under `cfg(real_tdjson)`, deterministic mock otherwise), durable account row
  persisted on Ready, `removeAccount` + `probeAuthorization` exports,
  `purge_account` in gramdrive-state. 43 ffi tests green.

**Swift agent (prior session + fixes this run)**
- `ControlContract` — the third narrow IPC channel beside health/hydration:
  NDJSON over `<root>/agent/control.sock`, versioned, size-capped; one-shot
  commands (status / reloadSettings / repair / removeAccount) and the auth
  session upgrade (states out, sequence-correlated input frames in).
- `ControlServer` wired into `AgentLifecycle` (always on), `ControlClient` +
  `ControlAuthChannel` for the companion side.
- `CoreControlBackends`: `KeychainSecretVault` (SEC-003), FFI-backed
  `CoreAuthorizer`/`CoreAccountRemover`/`CoreRepairRunner`.
- `AgentMain`: seams + `--telegram-test-dc` (smoke only).

**Swift companion (prior session, verified)**
- `LiveAuthorizationSession` mapping wire states/results onto the shell
  vocabulary; `AgentEnsurer` + `BundledAgentStarter` (SMAppService when
  launch-at-login is preferred, direct spawn otherwise — the preference is
  never silently upgraded); `LiveCompanionBackend` rewired: every command
  ensures the agent runs first; `notWired` is unreachable from the live
  backend (only test harnesses/previews can produce it).

## Fixed this run

1. **rustfmt drift** in `crates/gramdrive-ffi/src/auth.rs` — was failing the
   `format` gate.
2. **Agent died of SIGPIPE (exit -13)** during the first live E2E run: the
   agent's own sockets set `SO_NOSIGPIPE`, but TDLib's network sockets inside
   libtdjson carry the process default; a test-DC peer reset mid-write killed
   the process. Fix: `signal(SIGPIPE, SIG_IGN)` at agent start
   (`AgentMain.runAgent`). Regression probe added to the lifecycle smoke
   (step 2b: `kill -PIPE` → agent alive, health answers) — green.
3. **Keychain consent hang**: `SecItemCopyMatching` on the
   `gramdrive-telegram` items blocked forever in securityd (proven by
   `sample`: `ClientSession::decrypt` mach wait). File-keychain items created
   by the `security` CLI are partition-locked to `apple-tool:`; a Developer ID
   binary always prompts, trusted-app ACL or not. Fix: new
   `.scripts/keychain/provision_telegram_credentials.py` + Developer
   ID-signed Swift tool — recreates the items from a team-signed binary
   (partition `teamid:262RZ595FP`) with an ACL naming the packaged
   agent/companion. After provisioning, `authStart` streams
   `starting → configuring → wait-phone-number` unattended in seconds.
4. **Smoke hardening**: code candidates derive from the server-reported
   `codeLength` with 5/6 fallbacks (tdlib/td#1524); `--phone` operator mode
   added (below).

## Verification

| Gate | Result |
|---|---|
| `swift test` (apple/GramDriveSupport) | 275 tests / 49 suites green, incl. new `ControlChannelTests` (14) + `LiveControlTests` (10) + rewritten `LiveBackendTests` |
| `make check` | 8/8 (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts) |
| `make package-app` | APP PACKAGING PASSED — signed (Developer ID, hardened runtime), dmg built; spctl "Unnotarized Developer ID" is the expected pre-notarize verdict |
| `make smoke-agent-lifecycle` (with new SIGPIPE probe) | PASSED |
| `smoke-control-auth` (unattended pattern mode) | Blocked by Telegram (below); every layer up to code acceptance proven live |

Live E2E evidence from the shipped bundle against the real test DC
(149.154.167.40 = test DC2, confirmed via `lsof` at `wait-code`): agent
auto-start; silent keychain reads; `authStart` → typed states; phone accepted
by the real server; DC migration; `wait-code` with real `codeInfo`
(`authenticationCodeTypeSms`, length 5) over the wire; sequence-correlated
submits; the server's real `PHONE_CODE_INVALID` classified `invalid-code`
through FFI → agent → wire → client; sign-in slot reused cleanly across
sequential sessions; agent stable throughout (no SIGPIPE).

## External blocker (human action required for the last AC hop)

Telegram retired the shared-test-number auto-code for third-party api ids:
tdlib/td#3361 (maintainer, 2025-06): "The test phone numbers don't work
anymore for regular users" — the account must be created via an official app
first and codes are delivered for real. Proven independently of our stack by
a raw ctypes probe over the staged `libtdjson.dylib`: same
`PHONE_CODE_INVALID` for X*5 and X*6 on DC1/DC2, random and popular suffixes.
TASK-260716-1iypv4's "test accounts provisioned programmatically, no
human-owned account required" no longer holds server-side.

**Path forward (implemented, one-time human step):**
1. Human creates a test-DC account once with a real phone number via an
   official Telegram app in test mode.
2. `python3 .scripts/smoke/run_control_auth_smoke.py --phone +<number>` —
   one interactive code entry (code arrives in the official app session);
   `--keep` is implied, the authorized session persists in the container.
3. The restart/repair legs of that run — and any future re-runs over the
   kept container — are unattended. Session-persistence AC is proven by the
   same run (agent restart → status shows the account → repair completes).

Alternative for review: accept the layered live evidence above (everything
except Telegram accepting a code) plus the green suites as sufficient for
this bug, and track the dedicated-test-account provisioning as the follow-up
human task it now is (successor to TASK-260716-1iypv4's assumption).

## Product gap noted (out of scope, for the board)

A clean end-user machine has no `gramdrive-telegram` keychain items at all —
sign-in fails typed (`authRequired`: credentials not provisioned). How
api_id/api_hash reach end users (embedded at build vs provisioned) is an open
product decision; keychain provisioning is the dev/CI convention. Recorded in
LOGBOOK (0330 entry).

## Artifacts

- LOGBOOK 2026-07-20 entries 0320 (SIGPIPE), 0330 (keychain partition), 0400
  (Telegram test-number retirement) — root causes with evidence.
- `.temp/control-auth-smoke/` — smoke logs; `.temp/BUG-260720-3i74u1/diag/` —
  diagnostic probes (sampled stacks, raw tdjson probe, agent logs).
- README: tool rows for the control-auth smoke and keychain provisioning;
  Makefile `smoke-control-auth` comment updated.
