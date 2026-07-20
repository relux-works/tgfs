## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-19T21:51:07Z

## Last Update
2026-07-20T10:25:57Z

## Blocked By
- (none)

## Blocks
- TASK-260720-31nw0w

## Checklist
- [x] Live control channel over App Group unix socket: auth commands/events, status, settings, repair, removal; typed protocol shared with agent
- [x] Companion ensures agent is running (SMAppService/spawn of bundled gramdrive-agent, login-item preference honored); starting state in UI, notWired unreachable in shipped bundle
- [x] End-to-end auth proven against Telegram test DC from the packaged bundle; session persists across app restart
- [x] swift test + make check green; packaging (make app-bundle) still assembles and signs
- [x] Code written per task description and AC
- [x] Relevant tests written for new or changed behavior and passing
- [x] Lint clean
- [x] Relevant build/validation commands run after changes and build not broken
- [x] New outcome artifact attached on the board with a task-scoped name when the work produces notes, logs, screenshots, or other deliverables
- [x] Important findings, decisions, anomalies, or regressions recorded in logbook when relevant
- [x] Implementation matches AC
- [x] Solution fits project architecture
- [x] Tests green
- [x] If review does not accept the work — verdict evidence added and status routed by the explicit verdict branches

## Notes
spawn queued: [implementer] developer (claude) (run=RUN-260719-f02962, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260719-f02962)
Recon done. Plan: (1) FFI contract 0.6.0: export auth session (wrap tdjson AuthMachine+TdRuntime, real under cfg(real_tdjson), mock path for hermetic builds), account-row persistence on Ready, removal export; (2) Swift control channel over agent unix socket family (control.sock, NDJSON like hydration): server in GramDriveAgentCore wired into AgentLifecycle, client + LiveAuthorizationSession in companion, LiveCompanionBackend rewired, notWired removed from live paths; (3) ensure-agent-running (SMAppService/spawn seam, honest starting state); (4) packaging: tdjson-linked core + libtdjson.dylib in bundle + signing; (5) E2E smoke vs Telegram test DC (keychain creds present locally). Rust arch check already allows ffi->source-tdjson. Baseline cargo tests green; core+tdlib artifacts staged.
Progress: Rust done (FFI 0.6.0 auth+removal, purge_account in state crate, 43 ffi tests green, make check-core green, artifact restaged). Swift done: ControlContract/Server/Client in AgentCore, lifecycle wiring (control.sock always on, reload-settings, health accounts field), CoreControlBackends (KeychainSecretVault + CoreAuthorizer/Remover/Repairer FFI-backed), AgentMain seams (+--telegram-test-dc), companion LiveAuthorizationSession + AgentEnsurer (SMAppService/spawn per login pref) + LiveCompanionBackend rewired (notWired unreachable), CompanionMain domain-cleanup injection, CompanionAuthState.failed added. All targets build. Next: Swift test suites, swift test, packaging tdjson, E2E smoke vs test DC.
agent completed: [implementer] developer (claude) (exit=1)
spawn run completed: claude (run=RUN-260719-f02962, pid=26728, exit=1)
spawn queued: [implementer] developer (claude) (run=RUN-260719-6e3641, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260719-6e3641)
Resumed run (prior spawn died mid-smoke). Verified prior work: swift test 275 green, bundle packaged+signed 02:49. Found real bug in E2E smoke: agent died exit=-13 (SIGPIPE) mid-auth-flow against test DC — agent IPC sockets set SO_NOSIGPIPE per-socket, but TDLib network sockets inside libtdjson carry the process default; peer reset mid-write killed the agent. Fix: signal(SIGPIPE, SIG_IGN) process-wide at agent start (AgentMain.runAgent) + SIGPIPE-immunity probe added to agent lifecycle smoke. rustfmt drift in ffi auth.rs fixed (was the only make check failure). Re-running: make check, package-app (fixed agent into shipped bundle), lifecycle smoke, control-auth E2E smoke.
READY FOR REVIEW. Control channel implemented+verified: swift test 275 green (ControlChannelTests+LiveControlTests), make check 8/8, package-app signs, lifecycle smoke green incl. new SIGPIPE probe. Fixed this run: rustfmt drift; agent SIGPIPE death from libtdjson sockets (SIG_IGN at start + smoke regression probe); keychain consent hang (partition-lock — new .scripts/keychain/provision_telegram_credentials.py, signed tool, silent agent reads verified). notWired unreachable from live backend. Checklist item 3 (E2E ready + session persistence) blocked EXTERNALLY: Telegram retired shared-test-number auto-code (tdlib/td#3361, proven also via raw tdjson probe on test DC2 149.154.167.40 — PHONE_CODE_INVALID for all patterns/suffixes/DCs). Live evidence covers every hop except Telegram accepting a code. Path implemented: smoke --phone operator mode — human creates test-DC account once via official app, one interactive code entry, kept session makes restart/repair legs + future re-runs unattended. Reviewer decision: run the one-time interactive bootstrap (runbook in results artifact) or accept layered evidence and spawn a successor provisioning task (TASK-260716-1iypv4 assumption is dead). Full details: BUG-260720-3i74u1_results.md + LOGBOOK 0320/0330/0400.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-6e3641, pid=86644, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260719-5b6015, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260719-5b6015)
REVIEW VERDICT: CHANGES REQUESTED → to-dev. Quality high, architecture fit clean; all dev claims re-verified independently (swift test 275 green, make check 8/8, signed bundle + @rpath libtdjson verified, notWired unreachable, tdlib/td#3361 quotes verified verbatim via GitHub API, live E2E evidence real). NOT accepted: 3 confirmed majors — (1) AuthSession lacks Drop→close (leaked pump thread + slot scope held forever, auth.rs); (2) persist_account finalizes the real account under only the SIGN_IN_SLOT scope guard — races concurrent probe/remove of the same account (auth.rs:919-951); (3) keychain provisioning tool --exec mode is a promptless secret-exfiltration primitive with zero callers — remove it. Plus 1 should-fix (post-embed otool -L assertion in build_app_bundle.py) and minors listed in BUG-260720-3i74u1_review-verdict.md. External Telegram blocker STANDS as recorded (human-only one-time --phone bootstrap or AC amendment + successor provisioning task) — status is to-dev because rework is ordinary and autonomous; next review cycle inherits the blocker decision. Full evidence: BUG-260720-3i74u1_review-verdict.md + LOGBOOK 0338.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260719-5b6015, pid=58900, exit=0)
spawn queued: [implementer] developer (claude) (run=RUN-260720-a3a2ac, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260720-a3a2ac)
DECISION 2026-07-20 (owner): live-Telegram E2E is closed by the owner signing in on the released v0.1.1 build, not by the agent. Telegram killed shared-test-number auto-code (tdlib/td#3361). Agent proves all hops to Telegram acceptance vs test infra; checklist item 3 reframed as such. This unblocks the bug once the 3-major rework is reviewer-accepted.
Rework in progress: (1) AuthSession Drop->close; (2) persist_account acquires real-account scope; (3) removed keychain --exec exfil mode + dropped tool self-trust from ACL; should-fix otool -L post-embed assertion; minors: AuthCommand redacting Debug, Starting emitted on pump thread, canonicalized scope key, bounded terminate_session inner waits, smoke failed-event handling + phone/account masking, provision --check before delete, new build-script + FFI tests. OnceLock start-failure caching left as-is (claim() one-shot + start() consumes halves -> no clean retry without source redesign, which rework forbids); documented. Live Telegram E2E NOT run (external blocker); checklist item 3 stays unchecked.
Rework COMPLETE, ready for review. 3 majors fixed: (1) AuthSession Drop->close (auth.rs) + test; (2) persist_account acquires real-account scope, contention->finalize-account-busy fail-safe (auth.rs) + test; (3) removed keychain --exec exfil mode + readItem + dropped tool self-trust from ACL (provision-telegram-credentials.swift). Should-fix: post-embed otool -L assertion in build_app_bundle.py (assert_no_absolute_runtime_refs) + tests, verified live on real signed bundle. Minors: AuthCommand redacting Debug; Starting on pump thread; canonicalized scope key; bounded terminate_session waits; smoke failed-event handling + phone/account masking; provision --check before delete; +7 build-script tests, +2 FFI tests (Cancel-in-Unsupported, Failed identity path). OnceLock start-failure caching left as-is with documented rationale (claim() one-shot + start() consumes halves; source API redesign forbidden by scope). Verification: make check 8/8 (47 ffi tests, incl new; scripts self-tests incl new); core restage PASSED (real_tdjson compiles, contract 0.6.0 unchanged); swift test 2/2; make package-app PASSED + signs, otool assertion active + shipped Mach-Os @rpath-only. Live Telegram E2E NOT run (external blocker tdlib/td#3361 + owner decision closes it on release build). Evidence: BUG-260720-3i74u1_rework-results.md; LOGBOOK 1530.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260720-a3a2ac, pid=87742, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260720-0a1831, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260720-0a1831)
REWORK ACCEPTED → done (reviewer, 2026-07-20 1600). All 3 majors + should-fix + minors from the 0338 verdict correctly resolved; control-channel design untouched. Independently re-verified: cargo test -p gramdrive-ffi 47/0 incl. all 4 new tests (dropping_a_session_without_close_frees_the_sign_in_slot, finalization_fails_safe_when_the_account_scope_is_held, cancel_is_accepted_in_the_unsupported_state, finalization_reports_failed_when_the_identity_read_fails); make check 8/8 exit 0; make check-apple 2/2 exit 0. Adversarial pass on both Rust majors: Drop->close is non-blocking + never on the pump thread (no deadlock); persist_account acquires the real-account scope first, non-blocking HashSet so slot+account keys cannot deadlock, happy path still reaches Complete. Security major fully eliminated: swift tool has NO read path (readItem/--exec gone), no self-trust in the ACL. otool assertion exercised by clean+dirty+brew+signing-order tests. Live Telegram E2E correctly NOT attempted — owner-owned final hop per the 2026-07-20 decision (tdlib/td#3361); AC sentence 1 (every agent-ownable hop) fully proven. Evidence: BUG-260720-3i74u1_rework-review-verdict.md + LOGBOOK 1600.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260720-0a1831, pid=32414, exit=0)

## Precondition Resources
- [BUG-260720-3i74u1_rework-scope.md](file://BUG-260720-3i74u1/BUG-260720-3i74u1_rework-scope.md) — Rework: 3 majors + otool assertion, no live E2E

## Outcome Resources
- [BUG-260720-3i74u1_spawn-log_-implementer--developer--claude-.log](file://BUG-260720-3i74u1/BUG-260720-3i74u1_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [BUG-260720-3i74u1_results.md](file://BUG-260720-3i74u1/BUG-260720-3i74u1_results.md) — Implementation + verification results: control channel live, SIGPIPE + keychain-partition fixes, Telegram test-number retirement evidence and the --phone path forward
- [BUG-260720-3i74u1_spawn-log_-reviewer--reviewer--claude-.log](file://BUG-260720-3i74u1/BUG-260720-3i74u1_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [BUG-260720-3i74u1_review-verdict.md](file://BUG-260720-3i74u1/BUG-260720-3i74u1_review-verdict.md) — Review verdict: changes requested (to-dev) — 3 confirmed majors (AuthSession Drop, persist_account scope race, keychain tool --exec); all dev verification claims re-verified green; external Telegram blocker confirmed at primary source
- [BUG-260720-3i74u1_rework-results.md](file://BUG-260720-3i74u1/BUG-260720-3i74u1_rework-results.md) — Rework results: 3 majors + otool should-fix + minors, all verification re-run (make check 8/8, swift test 2/2, package-app signs)
- [BUG-260720-3i74u1_rework-review-verdict.md](file://BUG-260720-3i74u1/BUG-260720-3i74u1_rework-review-verdict.md) — Reviewer verdict on the 3-major rework: ACCEPTED, both suites re-run green, no new bugs
