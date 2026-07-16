# Implement shared core CI

## Description
Stand up GitHub Actions CI mirroring the relux-works/barycenter pattern (its ci.yml is the canonical reference): per-component jobs, a pinned acceptance-runner script invoked as scripts/acceptance/run_automated.py-style with require-clean semantics and a run-id, acceptance provenance uploaded from .temp/acceptance/<run-id> as artifacts with retention, minimal read-only permissions. For the Rust core job: format/lint/unit/property/conformance/migration/benchmark smoke, cargo audit/deny, license/SBOM policy checks (POL-6), secret scanning, doc checks.

## Scope
Host matrix and caching with pinned toolchains.

## Acceptance Criteria
Pull requests cannot merge with required failure; cache cannot alter results; logs contain no secrets.
