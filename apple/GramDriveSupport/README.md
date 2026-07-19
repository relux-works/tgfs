# GramDriveSupport

The Apple provider-support Swift package (`.spec/architecture.md`): the
app, the companion agent, and the File Provider extension all link this
package so every GramDrive process resolves the same shared container,
derives the same file layout, and follows the same multi-process rules.
Owned by TASK-260715-gnsa2s (STORY-260715-33oacu, macos-engine-host).

Policy lives in the Rust core (`crates/gramdrive-ffi/src/shared_state.rs`
— WAL-only open, role rights, snapshot reads, migration-on-open,
coordinator-only corruption recovery); this package binds it to the Apple
host: App Group resolution, `URL`-shaped entry points, and the Darwin
change doorbell.

## Surface

| Type | Owns |
|---|---|
| `AppGroup` | The container identity (`262RZ595FP.com.reluxworks.gramdrive`, the team-prefixed entitlement form v1 ships — DEC-019/POL-7) and the data-root rule: `Library/Application Support/GramDrive` inside the container. Everything below the data root comes from the core's `sharedStateLayout`, so Swift and Rust can never disagree about paths |
| `SharedState` | Role-based open: `openInAppGroupContainer(role:)` for product processes, `open(dataRoot:role:)` for tests and tools. The engine host opens as `.coordinator`, the File Provider extension and UI surfaces as `.provider` |
| `ChangeSignal` | The cross-process change doorbell: a payload-free Darwin notification (App-Group-prefixed name, which sandboxed processes may post and observe). Writers `post()` after commit; observers treat a ring as *check now* — compare `SharedStateStore.dataVersion()` and re-read only on change. Advisory, never authoritative: the database is the truth. Finder signaling (`signalEnumerator`) is a separate channel owned by the File Provider domain work |

The `gramdrive-shared-state-smoke` executable is the harness process for
`.scripts/smoke/run_shared_state_smoke.py` (reader / watcher / doorbell
modes); it is not a product target.

## The companion agent (TASK-260715-1yx9ly)

The package also ships the macOS background agent: `GramDriveAgentCore`
(the lifecycle library the agent binary and the app shell both link) and
`gramdrive-agent` (the launch-agent executable, PLAT-MAC-002/-005). The
lifecycle is `launching → recovering → running → draining → stopped`:

| Type | Owns |
|---|---|
| `AgentLifecycle` | The coordinator process's lifecycle: single-instance guard first, then shared state as `.coordinator` with corruption recovery (quarantine + one retry), the `DriveCore` handle, the health endpoint, power observation. `shutdown(reason:)` drains before tearing anything down |
| `AgentRuntimeLayout` | Host-owned runtime paths beside the core's layout: `agent/agent.lock`, `agent/health.sock`, `agent/settings.json` under the same data root |
| `SingleInstanceLock` | One coordinator per container, via `flock` — the kernel releases a crashed agent's lock, so recovery needs no stale-lock cleanup |
| `TransferRegistry` | The in-flight transfer ledger and the drain: admission refusal once draining, a grace period, then cancellation through each operation's FFI `CancellationToken`. Process-local by design; durable transfer state is the engine's, which is why a crash cannot duplicate work |
| `AgentHealthServer` / `AgentHealthClient` | The bounded local IPC: one endpoint, no request vocabulary — connect, receive one `AgentHealthSnapshot` (NFR-032 shape; unwired fields are honest `nil`s), EOF. A UNIX socket in the container rather than an XPC mach service so the channel stays provable in tests and the smoke; paths beyond `sun_path` are handled |
| `AgentSettings` / `AgentSettingsStore` | Durable host preferences — launch-at-login, managed-cache quota and global Archive Mode (POL-2) — as atomic JSON under `agent/`; never in the engine's database (DEC-006). The app writes it, the agent reads it; decoding tolerates a missing key as its default, so a shell or agent update never orphans the document |
| `LaunchAtLoginPolicy` / `SMAppServiceAgentLoginItem` | Idempotent reconciliation of the user's preference with `SMAppService` registration. Called by the *app* (the launchd plist lives in the app bundle — platform constraint); the agent honors the preference by reporting it and never self-registering |
| `PowerEventSource` / `WorkspacePowerEventSource` | Sleep/wake observation; wake re-probes `dataVersion` because a doorbell rung during sleep is lost |

Shutdown is signal-driven (`SIGTERM`/`SIGINT` → drain → exit 0), which is
exactly what launchd delivers on unload, logout, and update; the agent
carries its own version and the core's contract version in health so the
shell can detect a stale agent after an update. Accounts live inside the
shared database (`AccountScope`), so one agent hosts every account of the
container — the multiple-accounts path never means multiple coordinators.

## The companion shell (TASK-260715-13pxnu)

The package also ships the menu-bar companion app (PLAT-MAC-005):
`GramDriveCompanion` (the view-model + seam library) and
`gramdrive-companion` (the SwiftUI `MenuBarExtra` executable). It hosts no
engine and performs **no Telegram operation itself** — it is a presentation
layer that renders the agent's status and drives it through one seam.

| Type | Owns |
|---|---|
| `CompanionBackend` | The single boundary between shell and agent — the AC's "UI drives the agent via IPC; no Telegram ops from filesystem callbacks" is this seam existing and the shell holding nothing else. `LiveCompanionBackend` wires the reads that exist today (health over the bounded socket, settings over the durable document); commands (authorization, repair, removal) report `ControlChannelUnavailable.notWired` until the agent grows a control channel, because the health socket is read-only by design and the FFI exposes no such surface yet |
| `AuthorizationViewModel` / `CompanionAuthState` … | The sign-in flow (phone → code → optional 2FA, or QR → optional 2FA), a faithful mirror of the core's `gramdrive-source-tdjson::auth` vocabulary (`AuthState`/`AuthInput`/`AuthRejection`/`RetryAdvice`). The state stream from the `AuthorizationSession` seam is the single source of truth for the screen, exactly as TDLib's reported state is for the core machine |
| `CompanionStatusViewModel` | Account, File Provider domain, and diagnostics status — pure projections of the last `AgentHealthSnapshot`, with honest "not reported yet" where the engine has not wired a field |
| `CompanionSettingsViewModel` | The managed-cache quota, global Archive Mode with the POL-2 pre-enable check (projected disk usage + low-disk warning), and launch-at-login reconciled through `LaunchAtLoginPolicy` |
| `RepairViewModel` / `AccountRemovalViewModel` | The repair pass and the irreversible account removal (SEC-004), each gated and rendered by the shell but executed in the agent; removal is behind a typed, echo-the-label confirmation |
| `InMemoryCompanionBackend` / `ScriptedAuthorizationSession` | Preview- and test-support seam implementations (mirroring `gramdrive-testkit`) that make every screen state reachable deterministically |

Every screen state is a deterministic view-model tested via scripted fakes
(`Tests/GramDriveCompanionTests`); the SwiftUI views switch over those
states so every one is reachable. The command-channel wiring lands with the
control-channel story (this story blocks `STORY-260715-2pe5sa`).

## The core dependency is a built artifact

`Package.swift` resolves `GramDriveCore` (XCFramework + generated
bindings) by path from `.temp/packaging/GramDriveCore`, which `make
package` stages — built artifacts are never committed
(`.scripts/packaging/README.md`). Building here without the artifact fails
at dependency resolution:

```sh
make package                                  # stage the core artifact (repo root)
cd apple/GramDriveSupport
swift build                                   # macOS 14+ arm64 (POL-5)
swift test                                    # Swift Testing suite
```

`GRAMDRIVE_CORE_PACKAGE=<path>` overrides the artifact location when
consuming a staged or released artifact elsewhere.

## Verification

- `swift test` — Swift Testing suites: App Group identity and layout
  derivation, role-based open against a substitute container, provider
  quarantine refusal, coordinator corruption recovery through the
  bindings, doorbell post/observe/cancel round-trips, and the agent
  suites (lock contention, launch-policy matrix, health channel including
  beyond-`sun_path` sockets, registry drain semantics, and the full
  lifecycle against real shared state and a real hosted probe transfer);
  and the companion-shell suites — every authorization screen state and
  flow (phone/code/2FA/QR, rejection→advice, invalid-input refusal,
  control-channel-unavailable), status/diagnostics projection, settings
  round-trip with the Archive Mode preflight and launch-at-login reconcile,
  repair/removal outcomes, and `AgentSettings` forward/backward decode
  compatibility.
- `make smoke-agent-lifecycle` (repo root) — the agent as real processes:
  startup with health over the socket, single-instance refusal of a
  second agent, SIGTERM drain (hosted transfer cancelled through its
  token, exit 0, endpoint removed), and instant successor startup after
  SIGKILL (see `.scripts/smoke/run_agent_lifecycle_smoke.py`).
- `make smoke-shared-state` (repo root) — the real multi-process proof:
  a Rust coordinator process seeds, two concurrent Swift provider
  processes must read byte-identical item metadata through the packaged
  artifact, and a watcher process must observe the doorbell plus the
  data-version probe across a foreign commit. The Rust-side stress and
  SIGKILL crash tests live in `crates/gramdrive-state/tests/multiprocess.rs`.

## Substitute containers

Product processes resolve the real App Group container (which requires
GramDrive signing and entitlements). Tests and the smoke pass a substitute
container directory through the same `AppGroup.dataRootURL(containerURL:)`
rule — the layout code path is identical; only the root differs.
