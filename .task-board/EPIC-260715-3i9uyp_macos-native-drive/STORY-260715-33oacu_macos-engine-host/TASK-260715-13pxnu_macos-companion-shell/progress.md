## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-15T00:52:47Z

## Last Update
2026-07-19T11:15:43Z

## Blocked By
- TASK-260715-51n6jb

## Blocks
- (none)

## Checklist
- [x] macOS companion app (SwiftUI menu-bar or minimal window): authorization flow UI over the auth state machine (phone/code/2FA/QR), account status, provider/domain status, cache quota + Archive Mode settings, diagnostics, repair, account removal
- [x] No Telegram operations from filesystem callbacks (UI drives the agent via IPC); state rendering follows POL-1/POL-2 semantics; unit/UI tests for each screen state
- [x] All quality gates green (make check); app builds on macOS 14 arm64
- [x] Code written per task description and AC
- [x] Relevant tests written for new or changed behavior and passing
- [x] Lint clean
- [x] Relevant build/validation commands run after changes and build not broken
- [x] New outcome artifact attached on the board with a task-scoped name when the work produces notes, logs, screenshots, or other deliverables
- [x] Important findings, decisions, anomalies, or regressions recorded in logbook when relevant
- [x] Implementation matches AC
- [x] Solution fits project architecture
- [x] Tests green
- [ ] If review does not accept the work — verdict evidence added and status routed by the explicit verdict branches

## Notes
spawn queued: [implementer] developer (claude) (run=RUN-260719-e36ff7, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260719-e36ff7)
Plan: add GramDriveCompanion (SwiftUI menu-bar shell) to apple/GramDriveSupport package as library + gramdrive-companion executable + tests. Architecture decision: the bounded health IPC is read-only by design (HealthChannel.swift: control ops are not IPC verbs) and the FFI exposes no auth/repair/removal surface yet, so the shell drives command operations (auth flow, repair, account removal) through a CompanionBackend protocol SEAM rather than hacking a command vocabulary onto the health socket. Real, available-now paths wired for real: status/diagnostics via AgentHealthClient; cache-quota + Archive Mode + launch-at-login settings via AgentSettings (extended per POL-2) and LaunchAtLoginPolicy. Auth view-state machine mirrors Rust auth vocabulary (AuthState/Input/Rejection/RetryAdvice) so future command-channel wiring is a thin mapping. Every screen state is a deterministic view-model tested via scripted fakes. No Telegram ops in the shell and none from filesystem callbacks. Live command backend returns honest control-channel-unavailable until the command channel story lands (this story blocks STORY-260715-2pe5sa).
READY FOR REVIEW. Implemented GramDriveCompanion (SwiftUI menu-bar shell) + gramdrive-companion executable + 40 tests, new products of apple/GramDriveSupport. All six surfaces present, reachable, tested: authorization (phone/code/2FA/QR mirroring the Rust auth vocabulary), account/provider status, cache-quota + Archive Mode settings (POL-2 preflight), diagnostics, repair, account removal. Architecture: commands drive the agent through a CompanionBackend protocol SEAM (not a new IPC verb on the read-only health socket, not a faked FFI auth surface); reads (health, settings) wired for real via LiveCompanionBackend; commands report honest ControlChannelUnavailable.notWired until the control-channel story (this story blocks STORY-260715-2pe5sa). No Telegram ops in the shell; no filesystem callbacks (not the FP extension), so AC holds. Additive supporting changes: AgentSettings +cacheQuotaBytes/+archiveModeEnabled with missing-key-tolerant decode (older settings.json still loads; existing tests green); AgentHealthSnapshot public memberwise init. Verification: swift build all targets clean/no-warnings (macOS 14 arm64); swift test 90/90 (40 new); make check 8/8 green (provenance .temp/acceptance/local-all). No swift-format/swiftlint gate in repo (Rust-only lint); matched existing 4-space convention. Nothing committed (no-auto-commit). Details: TASK-260715-13pxnu_results.md; LOGBOOK 2026-07-19 1507.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-e36ff7, pid=80918, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260719-462422, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260719-462422)
REVIEW: ACCEPTED. AC + DoD met; solution fits architecture; gates green (reviewer re-ran: swift build clean, swift test 90/90 in 20 suites, macOS 14 arm64 floor; make check 8/8 per log). All six surfaces present, reachable, and tested per screen state (authorization phone/code/2FA/QR, account/provider status, cache-quota+Archive Mode POL-2 preflight, diagnostics, repair, SEC-004 typed-confirmation removal). No Telegram ops from filesystem callbacks — structurally guaranteed: shell is not the FP extension, imports no FileProvider/TDLib framework, holds no fs callbacks; commands route through the CompanionBackend IPC seam. Key architecture decision independently VERIFIED (not a forced fit): health IPC is read-only by design (HealthChannel.swift: control is not an IPC verb), gramdrive-ffi exposes no auth/repair/removal surface, and the auth view-state vocabulary is isomorphic to gramdrive-source-tdjson::auth (one intentional additive .idle) — so commands honestly report ControlChannelUnavailable.notWired instead of faking a cross-boundary op; live wiring belongs to the downstream control-channel story. Additive AgentSettings change is backward-compatible (missing-key-tolerant decode), existing tests green. Non-blocking notes: no XCUITest (deferred, needs signed app bundle/entitlements = packaging, out of scope; unit per-state contract suffices); launch-at-login persists to settings.json on Save while reconciling launchd immediately (launchd authoritative). Verdict evidence: TASK-260715-13pxnu_review-verdict.md.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-462422, pid=90462, exit=0)

## Precondition Resources
(none)

## Outcome Resources
- [TASK-260715-13pxnu_spawn-log_-implementer--developer--claude-.log](file://TASK-260715-13pxnu/TASK-260715-13pxnu_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-13pxnu_results.md](file://TASK-260715-13pxnu/TASK-260715-13pxnu_results.md) — Companion shell implementation notes: surfaces, CompanionBackend seam decision, POL-2 settings, test/gate results
- [TASK-260715-13pxnu_make-check.log](file://TASK-260715-13pxnu/TASK-260715-13pxnu_make-check.log) — make check 8/8 green (Rust/repo gates) after Swift-only companion shell change
- [TASK-260715-13pxnu_spawn-log_-reviewer--reviewer--claude-.log](file://TASK-260715-13pxnu/TASK-260715-13pxnu_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-13pxnu_review-verdict.md](file://TASK-260715-13pxnu/TASK-260715-13pxnu_review-verdict.md) — Reviewer verdict: ACCEPTED — AC/DoD met, architecture seam independently verified (health IPC read-only, FFI no auth surface, auth vocab isomorphic), swift build clean + 90/90 tests
