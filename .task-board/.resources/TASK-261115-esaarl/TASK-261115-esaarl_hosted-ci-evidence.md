# TASK-261115-esaarl: hosted CI producer evidence

PR: https://github.com/relux-works/tgfs/pull/67
Source candidate: 3ab4d1048e25fd84a540b2eddae2a328e671a973, bot signature G.
Run: https://github.com/relux-works/tgfs/actions/runs/43697525097 — failure.

Both requested jobs ran on GitHub-hosted macos-latest; no local Mac state is
required. The preserved bootstrap installs Rust 1.91.0, rustfmt and clippy;
cargo-deny remains 0.20.2. The gitleaks 8.30.1 ARM64 asset and SHA-256 match the
upstream release metadata. YAML semantic comparison confirms every other key,
step and assertion is unchanged. Other workflows are byte-identical to main.

Hosted secret-scan passed, including checksum verification and full-history
scan. The all suite passed toolchain, format, lint, test, architecture,
traceability and script self-tests (all exit 0). Only cargo deny failed (exit 1):
RUSTSEC-2026-0459 in libsqlite3-sys 0.37.0 and RUSTSEC-2026-0386 in tokio 1.52.4.
Cargo.toml, Cargo.lock and deny.toml are byte-identical to origin/main. The
source build and tests run successfully on a clean hosted Mac; the remaining
acceptance blocker is the unchanged dependency inventory. No gate was weakened.

Local validation: 579 Python tests, 4 skips, exit 0; focused workflow and
existing bootstrap tests green; git diff --check green; staged-delta and
canonical full-history gitleaks scans green. Directory-mode hygiene failed
on four unchanged pre-existing locations; it is not presented as passing.
Actual command exits and logs are in ../.temp/commands.jsonl and ../.temp/.
Hosted provenance is downloaded under out/initial-ci/.

| Mutant | Gate narrowed to | Named failing test | Bound |
|---|---|---|---|
| Remove installer checksum verification | Reject an altered valid archive before extraction/PATH publication | test_checksum_mismatch_never_installs_or_publishes_path | Killed: actual installer exits 0 instead of required 1. Does not prove upstream binary compatibility. |

No surface table was supplied (brief gap); no product behavior changed.
Existing reviewer tests were kept and passed. Out of contract per owner item 4:
native-ci, candidate-build and release migration, including their six dead-label
jobs. Main ruleset 20670428 requires only rust-core and secret-scan. Both runner
inventory endpoints returned 403, so missing-runner inventory is owner-provided.
No rework or module expansion occurred; README, board tracking, this outcome,
and UNRESOLVED_QUESTIONS.md document the requested change and observed blocker.

This is blocked producer evidence, not a green handoff. No acceptance or landing
is claimed. A separate dependency-remediation owner/scope is required; the
advisory links and patched ranges are recorded in UNRESOLVED_QUESTIONS.md.
The delivery report will name the final published head and its hosted result
after this evidence/status metadata is committed; CI source remains unchanged.
