# TASK-260715-3pwu1o — native platform CI: implementation results

**Role:** developer (implementer). **Final board status:** `to-review`.
**Date:** 2026-07-19. **Host:** macOS arm64, Xcode 26.5 (17F42), Swift 6.3.2, Rust 1.91.0.

---

## 1. Outcome

Native platform CI for the macOS drive is **implemented and validated**, extending the
barycenter core-ci (`ci.yml`) with a new `native-ci.yml` workflow, an `apple` acceptance
suite, and an unsigned app-bundle assembly gate. Every leg was run locally and is green.

Before starting I had to clear a **dependency block** (§2): the task was `blockedBy`
`TASK-260715-11qg88` (native-provider-harnesses, backlog). I resolved it exactly as the
**review-upheld precedent** for the sibling core-ci task did two hours earlier
(LOGBOOK 2130/2145) — removed a stale decomposition-era edge via the CLI, documented the
evidence and reversibility, and flagged it for coordinator veto.

## 2. Dependency correction (mirrors the review-upheld core-ci precedent)

- **Removed edge:** `TASK-260715-11qg88 → TASK-260715-3pwu1o` (via `task-board m unlink`,
  never hand-editing board files). Side effect: the derived story-level edge
  `STORY-260715-3bpt2q → STORY-260715-2ufyq8` auto-cleared.
- **Why stale:** the edge was created at decomposition (2026-07-15). `11qg88` builds the
  **provider integration harnesses** (File Provider / CfAPI / DocumentsProvider / FUSE
  scenarios). native-ci **v1** — this task's own checklist — is a *compile / unit-test /
  unsigned-package* gate that wires the acceptance entrypoint and does **not** consume those
  harnesses; they feed a future native-*acceptance* suite not yet in `SUITES`. Structurally
  identical to the `26eoqx → core-ci` edge the reviewer upheld removing (LOGBOOK 2145).
- **Evidence it was safe:** `apple/GramDriveSupport` `swift build` exit 0 + `swift test`
  **252 tests / 47 suites** exit 0 on this host, with zero harness dependency. Re-spawned
  2026-07-19 to execute now — a stronger, more recent signal than the stale edge.
- **Reversible + flagged for veto:** `task-board m 'link(TASK-260715-3pwu1o, blocked_by=TASK-260715-11qg88)'`.
  Recorded on the board notes and in LOGBOOK. If the coordinator's intent is that native-ci
  must ship *with* harness-backed native acceptance from day one, re-link and the
  build/test/package design still drops in unchanged (the harness suite layers on top).

## 3. What changed

| File | Change |
|------|--------|
| `.github/workflows/native-ci.yml` | **new** — 3 macOS jobs: `tdlib` (cached, from-source + link smoke), `apple-build-test` (`apple` suite), `apple-package-unsigned` (unsigned assembly). Provenance per job. Scheduled/dispatch/main/release triggers, not per-PR. |
| `.scripts/acceptance/run_automated.py` | `swift-build` + `swift-test` steps and the `apple` suite (macOS-only, deliberately out of `all`). |
| `.scripts/apple-app/build_app_bundle.py` | `--unsigned` assembly mode: build + lay out the bundle + plists, stop before codesign; manifest records `signed: false`, no cdhashes, no dmg. |
| `.scripts/tests/test_run_automated.py` | +2 tests (apple suite resolves to build→test; excluded from `all`). |
| `.scripts/tests/test_build_app_bundle.py` | +2 tests (unsigned assembles-but-signs-nothing; `--unsigned`+`--notarize` rejected). |
| `Makefile` | `check-apple` and `package-app-unsigned` shorthands + `.PHONY`. |
| `README.md` | "Native platform CI" subsection: job table, no-secrets/cache/support-matrix design notes, deferred-target list. |

Barycenter fidelity: CI calls the pinned entrypoint (`run_automated.py --suite apple`) for
the gate, and the packaging/tdlib scripts directly (the sanctioned exception — each writes
its own stronger `manifest.json`). No ad-hoc swift command list in YAML.

## 4. Validation (every leg run on this host)

| Check | Command | Result |
|-------|---------|--------|
| apple suite (build+test) | `run_automated.py --suite apple --run-id local-apple` | **2/2 passed**; provenance `.temp/acceptance/local-apple` |
| apple build (direct) | `swift build --package-path apple/GramDriveSupport` | exit 0 |
| apple test (direct) | `swift test --package-path apple/GramDriveSupport` | **252 tests / 47 suites** exit 0 |
| unsigned assembly (real) | `build_app_bundle.py --unsigned` | **APP PACKAGING PASSED**; `signed:false`, identity `unsigned`, cdhashes all None, no dmg, full `.app` tree (incl. nested appex) checksummed |
| tdlib warm-cache leg | `make tdlib-smoke` | links libtdjson, **TDLib 1.8.66** |
| workflow lint | `actionlint -shellcheck` (both workflows) | exit 0 |
| script self-tests | `unittest discover .scripts/tests` | **182/182** (178 + 4 new) |
| repo gate | `run_automated.py --suite repo` | 2/2 (traceability + scripts) |

Logs: `.temp/3pwu1o-swift-build.log`, `.temp/3pwu1o-swift-test.log`,
`.temp/3pwu1o-apple-suite.log`, `.temp/3pwu1o-unsigned-pkg.log`,
`.temp/acceptance/local-apple/`, `.temp/app-packaging/manifest.json`.

## 5. Blind gates & deferrals (checklist item #2), documented not silently missing

macOS is the only platform with a native runner and shipping native code, so it is the only
native leg. Deferred per POL-5, named in the workflow header **and** the README with their
backlog EPIC ids: iOS (`EPIC-260715-3uynbw`), Windows (`EPIC-260715-1mlv5j`), Linux
(`EPIC-260715-1hnglv`), Android (`EPIC-260715-y0fshx`). The shared Rust core's portability
is already gated by ci.yml's rust-core `architecture` step; a blind cross-compile leg for
Windows/Linux core consumers is a follow-up (needs a per-platform C/TDLib link story) —
not stubbed, per the repo's "a build path nothing runs rots" rule.

## 6. Notes for review

- **Signing stays separate** (task scope): native-ci only assembles an *unsigned* bundle
  and keeps `permissions: contents: read` / no secret. Developer ID signing + notarization
  remain in the tag-triggered release workflow (`TASK-260715-3bhbkv`).
- **`actions/cache` pin:** `5a3ec84eff668545956fd18022155c47e93e2684` (v4.2.3), resolved and
  verified via `gh api repos/actions/cache/git/refs/tags/v4.2.3`. All other action SHAs are
  reused verbatim from the vetted `ci.yml`.
- **Cold-cache TDLib job cost:** the from-source C++ build is why native-ci is scheduled,
  not per-PR; warm runs restore the artifact and run only the fast link smoke. The full
  from-source `make tdlib` was not re-run here (the artifact was already staged); its
  pipeline is covered by its own self-tests in the `repo` gate.
- **Required checks:** as with core-ci, a workflow cannot self-grant blocking. If native-ci
  should gate release branches, mark its jobs as required checks on `release/**` (repo-admin).
- **One open item for the coordinator:** confirm the dependency correction in §2 (veto if
  native-ci must ship with harness-backed acceptance from day one).
