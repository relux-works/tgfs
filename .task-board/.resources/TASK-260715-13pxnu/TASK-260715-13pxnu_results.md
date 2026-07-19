# TASK-260715-13pxnu — Minimal macOS companion shell

Implementation notes, decisions, and verification. Status handed to **review** (`to-review`).

## What shipped

A menu-bar companion shell, added as new products of the existing
`apple/GramDriveSupport` SwiftPM package (PLAT-MAC-005):

- **`GramDriveCompanion`** (library) — view models, the backend seam, and the SwiftUI views.
- **`gramdrive-companion`** (executable) — a SwiftUI `MenuBarExtra` app plus a resizable window over the same root view. Resolves the App Group data root, falling back to a local Application Support root for an unsigned dev run.
- **`GramDriveCompanionTests`** (test target) — 40 tests.

All six required surfaces are present, reachable, and tested:

| Section | View model | Notes |
|---|---|---|
| Sign in (authorization) | `AuthorizationViewModel` + `CompanionAuth*` | phone → code → optional 2FA, and QR → optional 2FA; mirrors the Rust `auth` vocabulary 1:1 |
| Account / provider status | `CompanionStatusViewModel` | pure projections of `AgentHealthSnapshot`; honest "not reported yet" for unwired engine fields |
| Storage & offline (cache quota + Archive Mode) | `CompanionSettingsViewModel` | POL-2: 10 GB default quota, Archive Mode with projected-usage + low-disk preflight; launch-at-login via `LaunchAtLoginPolicy` |
| Diagnostics | `CompanionStatusViewModel` → `DiagnosticsReport` | redacted snapshot fields, versions, events, sleep/wake |
| Repair | `RepairViewModel` | asks the agent, renders outcome |
| Remove account | `AccountRemovalViewModel` | typed, echo-the-label confirmation gate over SEC-004 wipe |

## Key architectural decision — the `CompanionBackend` seam

The AC requires "UI drives the agent via IPC; no Telegram operations from
filesystem callbacks." Two facts about what exists today shape the design:

1. The agent's bounded health IPC is **read-only by design** — `AgentHealthServer`
   states control operations are "not an IPC verb"; adding a command vocabulary
   to it would fight an explicit architecture decision.
2. The FFI contract (`gramdrive-ffi`) exposes **no auth/repair/removal surface**;
   the auth state machine is provider-internal in `gramdrive-source-tdjson` and
   not yet wired across the boundary.

So the shell drives every command through one protocol seam, `CompanionBackend`,
rather than inventing a command channel or an FFI auth surface (both belong to
other stories). The shell performs **no Telegram operation itself** and holds
**no filesystem callbacks** (it is not the File Provider extension), so the AC
holds trivially.

- **Reads wired for real** (`LiveCompanionBackend`): health over the bounded
  socket (`AgentHealthClient`), settings over the durable document
  (`AgentSettingsStore`).
- **Commands** (authorization, repair, removal): report honest
  `ControlChannelUnavailable.notWired` until the control-channel story lands.
  This story blocks `STORY-260715-2pe5sa`, which is where that wiring belongs.

This is the same discipline the sans-IO `AuthMachine` (caller owns the wiring)
and the honest-`nil` health snapshot already follow: build the deterministic,
testable core of the layer; define the seam; do not fake the cross-boundary op.

The authorization view-state vocabulary (`CompanionAuthState`/`Input`/`Rejection`/
`RetryAdvice`) is isomorphic to the Rust `auth` module, so the eventual wiring is
a thin mapping. The `AuthorizationSession` state stream — not inputs — is the
single source of truth for the screen, exactly as TDLib's reported state is for
the core machine.

## Supporting changes (additive, low-risk)

- **`AgentSettings`** (in `GramDriveAgentCore`, owned by TASK-260715-1yx9ly) gained
  POL-2 fields `cacheQuotaBytes` (default 10 GB base-10) and `archiveModeEnabled`,
  with a custom `Decodable` that tolerates a missing key as its default. An older
  `settings.json` (only `launchAtLogin`, or `{}`) still decodes — a shell/agent
  update never orphans the document. The existing `AgentSettingsStore` tests stay
  green; new compatibility tests cover empty/partial/full decode.
- **`AgentHealthSnapshot`** gained a public memberwise initializer (was
  internal-only), so the app shell and its tests can construct one. Production
  still decodes it from the agent's JSON.

## Verification

- `swift build` (all targets) — clean, no warnings (macOS 14 arm64, POL-5).
- `swift test` — **90/90 passed** (50 pre-existing + 40 new), 20 suites.
  - Authorization: every reported state → rendered state; full phone/code/2FA and
    QR flows; rejection→advice mapping; invalid-input refused locally;
    control-channel-unavailable surfaced; cancel-validity table.
  - Status/diagnostics: every readout mapping; honest-nil projection.
  - Settings: load/save round-trip through the seam; base-10 GB binding; Archive
    Mode preflight (fits / low-disk / unknown-capacity); launch-at-login reconcile
    surfacing awaiting-approval.
  - Repair/removal: completed/unavailable/failed outcomes; removal refused without
    a valid typed confirmation.
  - `LiveCompanionBackend`: health `notRunning` with no agent; durable settings
    round-trip; commands report `notWired`.
  - `AgentSettings` forward/backward decode compatibility.
- `make check` — **8/8 green** (toolchain, format, lint, test, architecture,
  supply-chain, traceability, scripts); provenance `.temp/acceptance/local-all`.
  Rust/repo gates unaffected (Swift-only change).

## Lint

No swift-format/swiftlint config exists in the repo; the enforced lint gate is
Rust-only (`make check` `lint` step, green). The existing committed Swift is
4-space-indented by convention (it also fails swift-format's 2-space default),
which the new code matches. Reformatting to swift-format defaults would diverge
from the entire codebase, so it was not done.

## Not in scope / handed to owning stories

- The agent **control channel** that will carry auth/repair/removal commands
  (belongs with `STORY-260715-2pe5sa` / the auth-drive wiring). The seam is
  defined; `LiveCompanionBackend` reports `notWired` until then.
- A signed `.app` bundle with entitlements / App Group / login-item plist
  (packaging). The shell builds and runs as an SPM executable like the existing
  `gramdrive-agent`.
- XCUITest-level UI automation needs an app bundle + test runner; the deterministic
  per-screen-state contract is covered by the view-model suites instead.
</content>
