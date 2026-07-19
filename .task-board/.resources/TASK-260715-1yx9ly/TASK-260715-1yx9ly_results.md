# TASK-260715-1yx9ly — companion agent lifecycle: implementation notes

Date: 2026-07-18. Role: developer. Status at handoff: to-review.

## What landed

The macOS companion background agent, as two new products of the existing
`apple/GramDriveSupport` package (the package every GramDrive process
links; agent placement follows the story that owns both):

- **`GramDriveAgentCore`** (library) — the lifecycle the agent binary and
  the app shell both link:
  - `AgentLifecycle` — `launching → recovering → running → draining →
    stopped`; startup order: single-instance lock → shared state as
    `.coordinator` with corruption recovery (quarantine + one retry) →
    `DriveCore` → health endpoint → power observation.
  - `SingleInstanceLock` — `flock(LOCK_EX|LOCK_NB)` on
    `<data_root>/agent/agent.lock`. Kernel releases a dead holder's lock:
    crash recovery needs no stale-lock cleanup and can never be locked out
    by a dead agent.
  - `TransferRegistry` — in-flight ledger + drain (admission refusal,
    grace period, cancellation through each operation's FFI
    `CancellationToken`, bounded wait, `abandoned` reported rather than
    hidden). Process-local by design; durable transfer state is the
    engine's — that is why crash recovery cannot duplicate work.
  - `AgentHealthServer`/`AgentHealthClient` — bounded local IPC: UNIX
    socket at `<data_root>/agent/health.sock`; connect → one JSON
    `AgentHealthSnapshot` → EOF. No request vocabulary at all, 1 MiB cap,
    send/receive timeouts. `UnixSocketAddress` handles paths beyond
    `sockaddr_un`'s 103-byte budget (group-container paths routinely
    exceed it) via serialized `chdir`-relative bind/connect.
  - `AgentHealthSnapshot` — NFR-032 shape: pid, state, versions (agent +
    FFI contract), launch preference, state schema/data version, pending
    transfer count, sleep/wake stamps, redacted fixed-vocabulary recent
    events. Unwired fields (source update, cursor, cache pressure,
    provider registration) are honest `nil`s until their owning stories
    land.
  - `AgentSettings`/`AgentSettingsStore` — durable host preferences
    (launch-at-login, default off), atomic JSON under `agent/`; never in
    the engine database (DEC-006). Unreadable settings are reported in
    health, not fatal.
  - `LaunchAtLoginPolicy` + `SMAppServiceAgentLoginItem` — idempotent
    reconcile of preference ↔ registration (`registered` / `unregistered`
    / `awaitingApproval` / `noChange`; approval is never retried in a
    loop). Platform constraint honored: the launchd plist lives in the
    *app* bundle, so registration is driven by the app shell
    (TASK-260715-13pxnu wires the UI); the agent honors the preference by
    reporting it and never self-registering.
  - `PowerEventSource`/`WorkspacePowerEventSource` — sleep/wake; wake
    re-probes `dataVersion` because Darwin doorbells rung during sleep are
    lost.
- **`gramdrive-agent`** (executable) — `run` (default) and `health`
  subcommands; `--container`/`--data-root` substitute roots for tests and
  smoke; SIGTERM/SIGINT → drain → exit 0 (exactly what launchd delivers on
  unload/logout/update); exit codes 0/2 (already running)/3 (startup)/4
  (health unavailable)/64 (usage). `--probe-transfer-ms` hosts one
  synthetic in-flight transfer through the real contract probe so drain is
  observable end to end.

Verification infrastructure:

- `make smoke-agent-lifecycle` → `.scripts/smoke/run_agent_lifecycle_smoke.py`
  — real processes over a substitute container: health over the socket
  (right pid, pending=1, schema healthy), second agent refused with exit 2
  while the first keeps serving, SIGTERM drain (`cancelled=1`, exit 0,
  socket removed, health unavailable), SIGKILL → successor starts
  immediately with healthy durable state.
- 39 new Swift Testing tests (package total 50): lock contention/release/
  diagnostics, settings store, launch-policy matrix (8 cases incl. error
  propagation and approval-pending), health channel (round-trip,
  beyond-`sun_path` path, stale socket reclaim, sequential/concurrent
  fetches, unavailable-after-stop), registry drain semantics (completed/
  cancelled/abandoned/refused), and the full lifecycle against real FFI
  shared state — including corrupt-DB quarantine recovery and draining a
  real hosted `probeTransfer` through its token.

Docs: `apple/GramDriveSupport/README.md` (§ The companion agent,
§ Verification), root `README.md` (smoke prerequisites + Tools row),
`Makefile` (`smoke-agent-lifecycle`).

## Acceptance criteria → evidence

| AC | Evidence |
|---|---|
| Recovers without duplicate work | Single coordinator enforced by flock (smoke step 2: second agent exit 2); crash: SIGKILL → successor immediately healthy (smoke step 4); recovery = durable state + coordinator-only quarantine (lifecycle test `startupQuarantinesACorruptDatabaseAndRecovers`); in-flight ledger is process-local so nothing is replayed from the agent side |
| Exposes health | `agent-lifecycle` smoke health checks over the socket; `AgentHealthClient` unit + lifecycle tests; NFR-032 field set with honest nils |
| Shuts down cleanly | SIGTERM → drain (grace → token cancel → bounded wait) → teardown in reverse start order → exit 0; smoke asserts `drained completed=0 cancelled=1 abandoned=0`, socket removal, health unavailable |
| Respects user launch preference | `AgentSettings.launchAtLogin` (default off, user opt-in), `LaunchAtLoginPolicy.reconcile` matrix-tested; preference surfaced in health; agent never self-registers (platform constraint documented) |

Scope items: **sleep/wake** — power observation + wake re-probe (tested
with a fake source; product source is `NSWorkspace`); **crash** — smoke
step 4 + quarantine test; **update** — clean SIGTERM path + agent/contract
versions in health for staleness detection (packaging story owns bundle
replacement); **logout** — `ShutdownReason.logout` drain path (tested);
the SEC-004 secure wipe is owned by the auth/logout work, not the
lifecycle; **multiple accounts** — lifecycle is account-agnostic: accounts
are rows in the shared database (`AccountScope`), one agent hosts all
accounts of the container (PRD-001 design path), never N agents.

## Decisions and notes for review

1. **UNIX socket, not XPC mach service, for health IPC.** Mach service
   registration needs the signed bundled launchd plist — unprovable in
   unit tests and the smoke. The socket lives in the App Group container
   (the exact surface the platform grants), the protocol has no request
   vocabulary, and the transport is one type per side if a later decision
   moves to XPC. Documented in `HealthChannel.swift` and the package
   README.
2. **`sun_path` handling.** Group-container socket paths exceed 103
   bytes; `UnixSocketAddress` falls back to serialized `chdir`-relative
   bind/connect (global-lock-guarded, cwd restored via held descriptor).
   Tested with a beyond-limit path.
3. **Registration is app-side by platform constraint.** `SMAppService
   .agent(plistName:)` resolves against the caller's bundle; the plist
   ships in the app. The policy module + adapter live here; the shell task
   calls them. The agent honors the preference by reporting it.
4. **Derived identifiers.** Launchd plist name
   `com.reluxworks.gramdrive.agent.plist` follows the DEC-019 derivation
   rule; the identifier table in `.spec/platform-requirements.md` lists
   only app + fileprovider explicitly — packaging (STORY-260715-2ca0k9)
   should add the agent's row when it lands the bundle.
5. **No new FFI surface.** The agent hosts what the contract exposes today
   (DriveCore, shared state coordinator, quarantine, contract version).
   TDLib itself enters this process via the engine when the composition
   stories land; the lifecycle is deliberately independent of that.
6. **`launching`/`recovering` states are near-instant today** (no
   long-running reconciliation yet); they exist so health can honestly
   report a slow recovery once the engine's startup reconciliation grows.

## Verification matrix

| Check | Result |
|---|---|
| `make check` (all 8 gates, CI-identical) | 8/8 passed, provenance `.temp/acceptance/local-all` |
| `swift test` (apple/GramDriveSupport, macOS 14+ arm64) | 50/50 passed (11 pre-existing + 39 new) |
| `make smoke-agent-lifecycle` | PASSED (all 4 phases; logs `.temp/agent-lifecycle-smoke/`) |
| `make smoke-shared-state` (regression after Package.swift change) | PASSED |

Nothing committed (repo rule: commits only after human review).
