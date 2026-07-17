## Status
done

## Assigned To
[reviewer] reviewer (claude)

## Created
2026-07-15T00:52:45Z

## Last Update
2026-07-17T20:12:49Z

## Blocked By
- TASK-260715-3o8wpt

## Blocks
- TASK-260715-2ulon7
- TASK-260715-3l6a0g
- TASK-260715-2r00ho

## Checklist
- [x] Reproducible TDLib build for macOS arm64 (pinned TDLib commit/version, BSL-1.0 license recorded per POL-6): static or dylib artifact + headers, cached so rebuilds are incremental; build script is a make target
- [x] Artifact versioned and checksummed; build documented (deps: cmake/openssl/zlib via brew or vendored, exact commands); CI-reusable per barycenter pattern
- [x] Link smoke test: a Rust bin links tdjson and calls td_create_client_id/td_json_client, printing TDLib version
- [x] All quality gates green (make check)
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
spawn queued: [implementer] developer (claude) (run=RUN-260717-1bba63, max_parallel=1)
spawn run started: [implementer] developer (claude) (run=RUN-260717-1bba63)
Approach (mirrors .scripts/packaging pattern): reproducible-build pipeline .scripts/tdlib/build_tdlib.py as a make target (tdlib). Pin TDLib to commit 022d60202e446ad1287b9fb68e687c8a0760788b (github.com/tdlib/td master resolved 2026-07-17). v1 target = macOS 14 arm64 only (POL-5/DEC-017); Windows/Android/Linux/iOS documented+deferred, same posture as packaging README. Deps: cmake, openssl@3 (brew), zlib (macOS SDK), gperf, clang (Xcode). Artifact: libtdjson.dylib + public C headers + BSL-1.0 license (POL-6), versioned+checksummed manifest.json + CHECKSUMS.sha256 (NFR-052). Link smoke: standalone Cargo crate .scripts/tdlib/link-smoke (own [workspace], NOT in gramdrive workspace so make check stays green without the artifact) links tdjson, calls td_create_client_id/td_execute/td_send/td_receive + references legacy td_json_client_* symbols, prints runtime TDLib version. Self-test .scripts/tests/test_build_tdlib.py (faked subprocess, no network/Xcode) runs in the repo gate scripts step. CI-reusable per barycenter (make target = shorthand, no second command copy).
agent completed: [implementer] developer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-1bba63, pid=84734, exit=0)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-a5a484, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-a5a484)
spawn queued: [reviewer] reviewer (claude) (run=RUN-260717-812cbd, max_parallel=1)
spawn run started: [reviewer] reviewer (claude) (run=RUN-260717-812cbd)
REVIEW VERDICT: ACCEPTED (reviewer/claude, 2026-07-18). Read-only review; gates and artifact re-run independently, not trusted from the results doc.
AC -> evidence (all confirmed on my own run):
- Pinned revision: TDLIB_COMMIT=022d60202e446ad1287b9fb68e687c8a0760788b in one constant; commit not tag (justified: last tag v1.8.0 is 2022). Fetch is offline+incremental when already checked out.
- Reproducible/attributable (NFR-052): manifest.json records pin, toolchain (cmake/clang/OpenSSL 3.6.3/zlib 1.2.12/gperf), target, linkage (otool -L), sha256 of every file; path_independent is DERIVED from clean_build_tree, not asserted (verified by unit test + code). Honest scope: same-machine byte-identical, cross-machine best-effort. Matches NFR-052 (attributability, not cross-machine bytes).
- Versioned + checksummed: dylib sha256 on disk (1735ce83...) == manifest == make tdlib-verify digest; shasum -c CHECKSUMS.sha256 OK for all 5 files.
- Licensed (POL-6): staged LICENSE_1_0.txt is genuine Boost Software License 1.0; BSL-1.0 recorded in manifest; POL-6 explicitly allows BSL-1.0.
- Consumable: ran make tdlib-smoke myself -> links -ltdjson via @rpath, calls td_create_client_id, reads runtime version out of the LIVE library -> TDLib version: 1.8.66; deprecated td_json_client_* referenced for link proof only (TDLib forbids mixing client interfaces; justified).
Gates (re-run by me, not trusted): make check = 8/8 (toolchain/format/lint/test/architecture/supply-chain/traceability/scripts). 20 new faked-subprocess self-tests pass and are wired into the repo gate scripts step.
Architecture fit: mirrors .scripts/packaging/ pipeline; single make target = shorthand (barycenter). link-smoke crate has its own [workspace] table and is absent from workspace metadata -> libtdjson never leaks into cargo build --workspace, so make check stays green without Xcode/network/artifact. macOS arm64-only scope is spec-correct (POL-5/DEC-017 fix v1 at macOS 14+ arm64); Windows/Linux documented as host-built, Android/iOS deferred, same posture the packaging README records -- a legitimate scope decision, not a forced fit.
Hygiene: build artifacts (link-smoke/target, __pycache__, .temp/tdlib) all gitignored; only source files (build_tdlib.py, README.md, tests, link-smoke sources + Cargo.lock, Makefile) would be tracked. No workspace crate touched.
No defects found. Downstream items (static OpenSSL/notarization -> TASK-260715-3bhbkv; wrapper consumption -> TASK-260715-2ulon7) correctly deferred out of scope.
agent completed: [reviewer] reviewer (claude) (exit=0)
spawn run completed: claude (run=RUN-260717-812cbd, pid=20980, exit=0)

## Precondition Resources
(none)

## Outcome Resources
- [TASK-260715-rxjkpi_spawn-log_-implementer--developer--claude-.log](file://TASK-260715-rxjkpi/TASK-260715-rxjkpi_spawn-log_-implementer--developer--claude-.log) — System spawn log captured by task-board
- [TASK-260715-rxjkpi_results.md](file://TASK-260715-rxjkpi/TASK-260715-rxjkpi_results.md) — Deliverable summary: reproducible TDLib tdjson build pipeline, AC->evidence, smoke output, gates, reproducibility digest
- [TASK-260715-rxjkpi_manifest.json](file://TASK-260715-rxjkpi/TASK-260715-rxjkpi_manifest.json) — Built artifact manifest: TDLib 1.8.66 @022d602, pin/toolchain/linkage/checksums (NFR-052)
- [TASK-260715-rxjkpi_link-smoke.log](file://TASK-260715-rxjkpi/TASK-260715-rxjkpi_link-smoke.log) — Rust link-smoke output: links libtdjson, td_create_client_id + legacy symbols, prints TDLib version 1.8.66
- [TASK-260715-rxjkpi_spawn-log_-reviewer--reviewer--claude-.log](file://TASK-260715-rxjkpi/TASK-260715-rxjkpi_spawn-log_-reviewer--reviewer--claude-.log) — System spawn log captured by task-board
