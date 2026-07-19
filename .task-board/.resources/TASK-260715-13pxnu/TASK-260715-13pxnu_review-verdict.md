# TASK-260715-13pxnu — Review verdict: ACCEPTED

Reviewer: [reviewer] reviewer (claude). Read-only review; no code modified.

## Verdict
**ACCEPTED → done.** Implementation matches AC and DoD, fits the project
architecture, and all gates are green (re-run independently by the reviewer).

## AC check — "All required states/actions accessible and tested; no Telegram operations occur from filesystem callbacks."
- **All six surfaces present, reachable, tested.** `CompanionRootView` sidebar
  exposes account/authorization/storage/diagnostics/repair/removal; each renders
  a dedicated view over a tested view model.
- **Every screen state tested.** View-model suites drive each state through the
  real consume path (scripted `AuthorizationSession`, `InMemoryCompanionBackend`),
  not private setters: authorization phone/code/2FA/QR + rejection→advice +
  invalid-input refusal + control-channel-unavailable; status/diagnostics
  projections incl. honest-nil; settings round-trip + Archive Mode preflight
  (fits/low-disk/unknown) + launch-at-login reconcile; repair/removal outcomes;
  removal typed-confirmation gate; AgentSettings fwd/back decode.
- **No Telegram ops from filesystem callbacks — structurally guaranteed.** The
  shell is not the File Provider extension, imports no FileProvider/TDLib
  framework (imports: Foundation, SwiftUI, GramDrive{AgentCore,Support,Companion}
  only), and holds no filesystem callbacks. "Telegram" appears only in
  user-facing UI strings. Commands route through the `CompanionBackend` IPC seam.

## Architecture fit — key decision independently verified (not a forced fit)
The `CompanionBackend` seam (reads wired live; commands report honest
`ControlChannelUnavailable.notWired`) is a legitimate boundary, confirmed against
the codebase:
1. Health IPC is **read-only by design** — `HealthChannel.swift`: "no request
   vocabulary at all … control operations stay where they belong … not an IPC
   verb." Server never reads from the peer. TRUE.
2. `gramdrive-ffi` exposes **no** auth/repair/account-removal surface (only
   DriveCore probe/config, CancellationToken, shared-state reads, and
   coordinator-only `quarantine_corrupt_state` DB-corruption recovery — unrelated
   to accounts). Authorization surfaces only as `DriveError::AuthRequired`. TRUE.
3. Auth view-state vocabulary is **isomorphic** to Rust
   `gramdrive-source-tdjson::auth` (AuthState/Input/Rejection/RetryAdvice, incl.
   `kind` strings and `advice` mapping); one intentional, documented additive
   Swift-only `.idle` pre-session state. TRUE.
So commands do not hack a vocabulary onto the read-only health socket nor fake an
FFI auth surface — both belong to the downstream control-channel story. The AC
does not require live command execution.

## Policy conformance
- **POL-2**: cache quota (10 GB base-10 default) + global Archive Mode with the
  pre-enable preflight (projected usage + low-disk warning over an injectable
  `DiskSpaceProbe`). Persisted via the durable settings document.
- **POL-1**: honest "not reported yet" projections for unwired engine fields
  (account status, provider domain, snapshot optionals) — no fabricated readings.
- **SEC-004**: irreversible removal gated behind a typed, echo-the-label,
  acknowledge-irreversible confirmation; the wipe runs in the agent, not the shell.

## Gates — re-run by the reviewer
- `swift build` (all targets) — clean, exit 0 (macOS 14 arm64 floor declared).
- `swift test` — **90/90 passed**, 20 suites (10 new companion suites), exit 0.
- `make check` — 8/8 green per attached log (Rust/repo gates; Swift package not
  covered by make check, so build+test were verified independently above).
- Blocked-by dependency (TASK-260715-51n6jb authorization-state-machine) is `done`.
- Supporting `AgentSettings` change is additive and backward-compatible
  (missing-key-tolerant decode), existing tests stay green.

## Non-blocking observations (no rework required)
1. No XCUITest UI automation — deferred, needs a signed app bundle + entitlements
   (packaging, explicitly out of scope for this story). The deterministic
   per-screen-state view-model contract is an acceptable substitute; the DoD's
   "unit/UI tests" is satisfied by the unit path.
2. Launch-at-login toggle reconciles launchd immediately but persists to
   settings.json only on Save. Minor UX wrinkle; launchd registration is the
   authoritative state and the agent reports it from health. Not a defect.
3. Command surfaces honestly report `notWired` today — by design; live wiring
   lands with the control-channel story this task blocks.
