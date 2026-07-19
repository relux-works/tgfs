# TASK-260715-3faqmr — Implement shared core CI — REVIEW VERDICT

**Verdict: ACCEPTED → done.** Read-only review; reviewer re-verified every claim independently.

## AC mapping (all met)
- **"PRs cannot merge with required failure"** — workflow triggers on `pull_request`; `rust-core` + `secret-scan` are the gate. A workflow file genuinely cannot self-grant blocking; marking the two jobs as required status checks on `main` is a one-time repo-admin step, correctly documented in README + results as the sole out-of-workflow follow-up.
- **"cache cannot alter results"** — toolchain pinned by `rust-toolchain.toml` (1.91.0), deps by `Cargo.lock`, cargo-deny 0.20.2 + gitleaks 8.30.1 by exact version (gitleaks also sha256), every action SHA-pinned. `Swatinem/rust-cache` keyed on toolchain+lockfile → a hit only holds artifacts from identical inputs.
- **"logs contain no secrets"** — neither job consumes a repo secret; gitleaks runs `--redact`; one confirmed false positive pinned by fingerprint in `.gitleaksignore`, not by weakening a rule.

## Architecture fit
- Single-entrypoint contract honored: CI invokes `run_automated.py --suite <x> --require-clean --run-id ci-<x>`, never an ad-hoc command list — a CI failure reproduces identically under `make check` / `make check-security`.
- Barycenter pattern faithfully mirrored: per-component jobs, pinned entrypoint, provenance upload (`if: always()`, `if-no-files-found: error`, retention 14d).
- Runner changes minimal and consistent with the existing Step/SUITES structure; `security` deliberately kept out of `all` (only gitleaks-dependent gate) and that boundary is unit-tested.

## Reviewer-reproduced evidence (this host, rust 1.91.0 / gitleaks 8.30.1 / actionlint)
- `actionlint -shellcheck=shellcheck .github/workflows/ci.yml` → exit 0.
- `--suite all --run-id review-all` → **8/8 passed** (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts).
- `--suite security --run-id review-security` → **1/1 passed** (0 leaks).
- `python3 -m unittest discover -s .scripts/tests` → **178 passed** (+4 new, was 174).
- Pinned gitleaks tarball sha256 independently downloaded + verified against the real `gitleaks_8.30.1_linux_x64.tar.gz` release → **MATCH** (install step will not break CI).

## Dependency correction — reviewed, accepted
Stale decomposition-time edge `26eoqx (synthetic-fixture-corpus) → 3faqmr` removed (bidirectionally consistent via CLI). Justified: CI wires the existing entrypoint and does not consume the corpus; `--suite all` passes clean without it; corpus-dependent suites don't exist in `SUITES` yet and gate automatically once added. Remaining hard block `2cn768 (toolchain-and-quality-config)` is `done`, so the task was legitimately unblocked. Reversible + flagged for coordinator veto.

## Scope calls (accepted)
conformance/migration/benchmark smoke and doc checks named in the task *description* are not in the DoD *checklist* and are not yet steps in the runner; transparently scoped out and gate automatically once added to `SUITES`. cargo-deny covers audit (RustSec) + license/SBOM policy (POL-6). Two-runner matrix (macos-15 + ubuntu-24.04) fits core CI; windows/cross-build belong to packaging/release (separate task).

No changes requested. No stop-the-line boundary.
