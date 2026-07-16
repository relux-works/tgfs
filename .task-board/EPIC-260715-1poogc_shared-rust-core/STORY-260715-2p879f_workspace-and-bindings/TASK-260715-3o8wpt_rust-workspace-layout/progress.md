## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-15T00:52:44Z

## Last Update
2026-07-16T23:43:24Z

## Blocked By
- TASK-260715-1czb40

## Blocks
- TASK-260715-265gqq
- TASK-260715-1qz1g5
- TASK-260715-rxjkpi
- TASK-260715-299aw0
- TASK-260715-3gzi1v
- TASK-260715-e90vvr

## Checklist
- [x] Cargo workspace with crates for: domain model, DriveSource contract, SQLite state, NDJSON/Markdown render, transfer/cache engine, UniFFI FFI layer, test-support; dependency direction documented and cycle-free (enforced by a check, e.g. cargo-deny or a script)
- [x] Feature policy: no platform-specific code in core crates; tdjson/remote source implementations behind features or separate crates per DEC-003/DEC-005
- [x] License gate wired: cargo-deny license policy allowing permissive only per POL-6, running clean
- [x] cargo build + cargo test green on macOS arm64 host; each crate README documents ownership and test command
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
spawn queued: [implementer] developer (claude) (run=RUN-260716-8e4c6b, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260716-8e4c6b)
Workspace implemented: 7 crates (gramdrive-{model,source,state,render,engine,ffi,testkit}) at crates/, layering + feature policy in crates/README.md, enforced by .scripts/check_crate_architecture.py (negative-tested, 4 violation classes). POL-6 license gate: deny.toml, cargo deny check licenses — clean, fail-closed to the exact POL-6 set. Build/test/clippy/fmt green on macOS arm64 (Rust 1.91, edition 2024). Key decisions in logbook 0323: source impls = separate crates not features (DEC-003/DEC-005); gramdrive-* naming per POL-7; engine->render edge allowed-unused. Left to siblings: toolchain pin+lint config (2cn768), UniFFI (265gqq), packaging (3akqs8). Outcome resources: results.md, verification.log, negative-checks.log. Nothing committed to git — awaiting review.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260716-8e4c6b, pid=9816, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260716-d96b39, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260716-d96b39)
REVIEW 2026-07-17: changes requested -> to-dev. Accepted as designed: workspace layout, 7-crate layering, direction rules, POL-6 deny gate, per-crate READMEs, docs — all gates independently re-run by reviewer and green (build/test/fmt/clippy/deny/arch-check; Cargo.lock = 7 workspace crates only). Confirmed defects, both in .scripts/check_crate_architecture.py (this task owns architecture checks): D1 cfg scan misses #[cfg(all(unix,..))], #[cfg(not(windows))], #[cfg_attr(windows,..)], cfg!(target_os=..) — all injected into gramdrive-model, check passed exit 0; D2 [target.cfg(...).dependencies] sections invisible (dep target field ignored) — target-gated dep in gramdrive-state passed exit 0. This falsifies the enforced-by-check claim in the script docstring and crates/README.md. Required: broaden predicate detection (fail-closed line match on (cfg|cfg_attr)[!(] + predicate word), error on non-null dep target in platform-neutral crates, extend negative-check log with these forms, sync doc claims. Recommended non-blocking: consider std::os:: path scan. Evidence: TASK-260715-3o8wpt_review-verdict.md + TASK-260715-3o8wpt_review-probe.log (board), .temp/TASK-260715-3o8wpt/review-probe-01.log. No product-code rework wanted; scope is the check script + evidence/doc sync only.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260716-d96b39, pid=21295, exit=0)
spawn queued: [implementer] developer (claude) (run=RUN-260716-dc6f4e, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260716-dc6f4e)
Rework pass 2026-07-17 addressing review verdict D1/D2. Scope honored: only .scripts/check_crate_architecture.py + crates/README.md changed; no crates/ product code touched.
D1 (cfg forms): replaced the cfg( + optional any( regex with a two-part fail-closed scan — find every cfg/cfg!/cfg_attr invocation, then search its balanced-paren argument span for target_os/target_family/target_vendor/windows/unix. Went one step past the suggested line match (verdict said at minimum): the span scan also catches predicates rustfmt wraps across lines, and has fewer false positives since the word must sit inside the cfg args.
D2 (target-gated deps): platform-neutral crates now error on any non-null cargo metadata dep target (any section incl. dev) — leakage regardless of dep name, so external crates outside the ban list are covered too.
Optional taken: std::os:: source scan (compiles per-platform with no cfg and no dep).
Doc sync: crates/README.md Everything in this document is enforced by was false both ways — now scoped to crate set/direction/platform neutrality, names cargo deny as the license enforcer, and lists what is convention-not-enforced (sources-as-crates, no-features baseline, layer numbering, cross-target -> TASK-260715-2cn768). Script docstring documents the fail-closed tradeoffs.
Evidence: TASK-260715-3o8wpt_negative-checks.log regenerated — 12 injected-violation cases (all exit 1) + 2 controls (exit 0), reproducible via TASK-260715-3o8wpt_negative-check-harness.py. All 8 reviewer-proven bypasses now fail closed; controls prove the scan does not fire on the crates own //! docs that name predicates in prose. NEG-4 (license) carried over verbatim, not re-run — needs a GPL/MPL crate in tree; deny.toml untouched and cargo deny check licenses green.
Gates re-run on real tree, all green: build, test (14 suites, 0 failures), fmt, clippy (0 warnings), deny licenses, arch-check (OK: 7 crates conform). See TASK-260715-3o8wpt_rework-results.md.
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260716-dc6f4e, pid=24418, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260716-95bd6c, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260716-95bd6c)
REVIEW #2 2026-07-17: ACCEPTED -> done. Rework scope honored (mtime-verified: only .scripts/check_crate_architecture.py + crates/README.md changed; crates/ product code untouched, no probe leftovers). All gates independently re-run by reviewer on the real tree, green: build, test (14 suites, 10 passed, 0 failed), fmt, clippy, deny licenses, arch-check, plus full make check exit 0. Board negative-check harness re-run verbatim -> reproduces the log 1:1 (10 injections exit 1, 2 controls exit 0). 10/10 independent adversarial probes beyond the harness behaved as claimed: any()-regression, bare cfg!(windows), target-gated dev/build deps, plain-triple [target.x86_64-pc-windows-msvc] gate (no cfg syntax), banned dep in dev section, renamed target-gated dep (original probe C form), multi-line cfg_attr, string-literal control. D1 span-scan is strictly stronger than the verdict minimum (multi-line coverage, fewer false positives) - accepted as improvement. D2 name-independent non-null-target check verified any section incl. dev. Doc claims synced (docstring checks 1-10 + tradeoffs match implementation; crates/README.md scopes enforcement claims, lists conventions, names cross-target gap -> TASK-260715-2cn768); LOGBOOK 0352 records rework. Non-blocking observations in verdict: target_arch/target_env outside scanned set (matches required scope); harness needs to run from depth-2 under repo root (parents[2] resolution); rework-results 12-case phrasing vs 10-fresh-cases log reconciles via NEG-1 double injection + NEG-4 carry-over. Evidence: TASK-260715-3o8wpt_review2-verdict.md + TASK-260715-3o8wpt_review2-probes.log (board), .temp/TASK-260715-3o8wpt/review2/.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260716-95bd6c, pid=26946, exit=0)

## Precondition Resources
- [TASK-260715-3o8wpt_rework-scope.md](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_rework-scope.md) — Narrow rework scope from review verdict

## Outcome Resources
- [TASK-260715-3o8wpt_spawn-log_-implementer--developer--claude-.log](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-3o8wpt_results.md](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_results.md) — Workspace implementation notes: crate layout, enforcement, license gate, decisions, verification evidence
- [TASK-260715-3o8wpt_negative-checks.log](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_negative-checks.log) — Negative checks: 12 injected-violation cases + 2 controls proving the architecture check fails closed (regenerated after D1/D2 rework)
- [TASK-260715-3o8wpt_verification.log](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_verification.log) — Final gate run on the real tree after D1/D2 rework: build, test, fmt, clippy, deny licenses, arch check — all green
- [TASK-260715-3o8wpt_spawn-log_-reviewer--reviewer--claude-.log](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-3o8wpt_review-verdict.md](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_review-verdict.md) — Reviewer verdict: to-dev. All gates independently re-verified green; 2 confirmed enforcement bypasses in check_crate_architecture.py (cfg edge forms, target-gated deps) with required fixes.
- [TASK-260715-3o8wpt_review-probe.log](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_review-probe.log) — Reviewer probe log: probe A (baseline violations caught), probe B (4 cfg edge forms NOT caught), probe C (target-gated dep NOT caught), clean-tree re-verification.
- [TASK-260715-3o8wpt_negative-check-harness.py](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_negative-check-harness.py) — Reproducible harness that generates the negative-check log: copies workspace to scratch, injects one violation per case, runs the check
- [TASK-260715-3o8wpt_rework-results.md](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_rework-results.md) — Rework notes for review verdict D1/D2: broadened cfg detection, target-gated dep check, std::os scan, doc-claim sync, all gates re-run
- [TASK-260715-3o8wpt_review2-verdict.md](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_review2-verdict.md) — Review verdict #2 (rework D1/D2): accepted -> done; independent gate re-runs + 10 adversarial probes
- [TASK-260715-3o8wpt_review2-probes.log](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_review2-probes.log) — Reviewer's independent adversarial probe log (10 cases beyond the implementer harness, all as expected)
- [TASK-260715-3o8wpt_review2-probe-harness.py](file://TASK-260715-3o8wpt/TASK-260715-3o8wpt_review2-probe-harness.py) — Reviewer's reproducible probe harness (10 cases beyond implementer harness); run from two levels below repo root, e.g. .temp/<dir>/
