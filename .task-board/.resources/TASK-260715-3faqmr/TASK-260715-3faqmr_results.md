# TASK-260715-3faqmr — Implement shared core CI — results

Status handed to review. Branch base: `0a5cace`.

## Summary

Stood up GitHub Actions CI (`.github/workflows/ci.yml`) on the relux-works/barycenter
pattern: per-component jobs, each running the ONE pinned acceptance entrypoint
(`.scripts/acceptance/run_automated.py --suite <x> --require-clean --run-id ci-<x>`),
each uploading its provenance from `.temp/acceptance/<run-id>` as an artifact.
CI assembles no cargo/gitleaks commands of its own, so a check that fails in CI
fails identically under `make check` / `make check-security`.

## Deliverables

| File | Change |
|------|--------|
| `.github/workflows/ci.yml` | New. Two jobs: `rust-core` (macos-15) + `secret-scan` (ubuntu-24.04). |
| `.gitleaks.toml` | New. Pinned secret-scan config (extends default ruleset). |
| `.gitleaksignore` | New. One verified false-positive fingerprint, documented. |
| `.scripts/acceptance/run_automated.py` | Added `secret-scan` step + `security` suite; docstring note. |
| `.scripts/tests/test_run_automated.py` | +4 tests (security suite, all-excludes-secret-scan, --redact, --config). |
| `Makefile` | Added `make check-security`; clarified `make check` scope. |
| `README.md` | New "Continuous integration" section; security-suite prereq (gitleaks). |
| `LOGBOOK.md` | Dependency-correction decision + CI milestone. |

## CI design

| Job | Runner | Entrypoint | Covers | Artifact (retention 14d) |
|-----|--------|-----------|--------|--------------------------|
| `rust-core` | `macos-15` (arm64 — POL-5/DEC-017 reference host) | `--suite all --require-clean --run-id ci-all` | toolchain pin, fmt, clippy `-D warnings`, `cargo test --workspace --all-features`, crate-architecture, cargo-deny (POL-6 licenses + RustSec advisories + bans + sources), traceability validator, script self-tests | `acceptance-ci-all` |
| `secret-scan` | `ubuntu-24.04` (portable) | `--suite security --require-clean --run-id ci-security` | gitleaks over committed history, redacted | `acceptance-ci-security` |

`rust-core` runs `--suite all` (not just `core`) because the DoD checklist puts
fmt/clippy/tests/deny/architecture **and the traceability validator** in one
rust-core job; `all` = core + repo delivers exactly that plus the script
self-tests, on the reference host where the suite was verified. Secret scanning
is its own component (only gitleaks-dependent gate; merge-boundary check), kept
out of `all` so the everyday pre-push `make check` needs no gitleaks.

## Acceptance-criteria mapping

- **"PRs cannot merge with required failure"** — workflow triggers on
  `pull_request`; both jobs are the merge gate. A workflow file cannot make
  itself blocking, so the one-time repo-admin step is to mark `rust-core` and
  `secret-scan` as **required status checks on `main`** (documented in README).
- **"cache cannot alter results"** — toolchain pinned by `rust-toolchain.toml`,
  deps by `Cargo.lock`, cargo-deny 0.20.2 + gitleaks 8.30.1 by exact version
  (gitleaks also by sha256), every action by commit SHA. The cargo cache
  (Swatinem/rust-cache) is keyed on toolchain+lockfile, so a hit only ever holds
  artifacts from identical inputs — a stale/poisoned cache can change nothing but
  wall-clock.
- **"logs contain no secrets"** — neither job consumes a repository secret;
  gitleaks runs with `--redact` so a matched value never enters the uploaded
  provenance log. False positives are pinned by fingerprint, not by printing
  them.

## Pinning (exact)

- Rust 1.91.0 (rust-toolchain.toml), cargo-deny 0.20.2, gitleaks 8.30.1
  (sha256 `551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb`,
  linux x64).
- Actions by SHA: `actions/checkout` v4.2.2
  (`11bd71901bbe5b1630ceea73d27597364c9af683`), `actions/upload-artifact` v4.6.2
  (`ea165f8d65b6e75b540449e92b4886f43607fa02`), `Swatinem/rust-cache` v2.8.0
  (`98c8021b550208e191a6a3145459bfc9fb29c4c0`), `taiki-e/install-action` v2
  (`07b4745e0c39a41822af610387492e3e53aa222b`).

## Dependency correction (flagged for coordinator)

`core-ci` was board-blocked by `TASK-260715-26eoqx` (synthetic-fixture-corpus,
backlog, unassigned, itself blocked). Removed that stale edge — the CI wiring
runs the existing acceptance entrypoint and does not consume the fixture corpus:
`--suite all` passes 8/8 clean on `0a5cace` with no corpus present, and the
runner's single-entrypoint contract means future conformance/migration/benchmark
suites are picked up automatically once added to `SUITES` (CI calls `--suite`,
never a hardcoded command list). Reversible: `link(TASK-260715-3faqmr,
blocked_by=TASK-260715-26eoqx)`. See LOGBOOK 2026-07-19 2130.

## Verification evidence (this host)

- `actionlint -shellcheck=shellcheck .github/workflows/ci.yml` → exit 0 (shell in
  the gitleaks-install step linted clean).
- Full CI-entrypoint simulation: `--suite all --run-id ci-all` → 8/8 passed;
  `--suite security --run-id ci-security` → 1/1 passed. Provenance under
  `.temp/acceptance/ci-all` and `.temp/acceptance/ci-security`.
- Runner self-tests: `python3 -m unittest discover -s .scripts/tests` → 178
  passed (was 174; +4 new).
- gitleaks history scan: 60 commits, 0 leaks with `.gitleaks.toml` +
  `.gitleaksignore`; the single finding was a confirmed false positive (a board
  checklist line "key roundtrip …") suppressed by fingerprint.
- `--require-clean` not exercisable in the dirty working tree during dev, but its
  refusal path is unit-tested (`test_require_clean_*`) and CI checks out a clean
  commit.

## Out of scope / follow-ups

- **Required-check branch protection** on `main` (repo admin) — the only step CI
  YAML cannot self-grant.
- **conformance / migration / benchmark suites** and **doc checks** named in the
  task *description* are not in the task *checklist* and are not yet steps in the
  runner; when added to `SUITES` they gate in CI automatically with no workflow
  edit.
