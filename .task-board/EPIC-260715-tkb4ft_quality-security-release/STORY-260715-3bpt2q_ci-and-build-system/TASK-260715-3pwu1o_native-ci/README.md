# Implement native platform CI

## Description
Extend the barycenter-pattern CI (see relux-works/barycenter ci.yml as canonical reference) to native targets: macOS job on macos-15 arm64 building app plus File Provider extension (support matrix POL-5), blind cross-build gates where a runner lacks the target, packaged-probe jobs on real runners where packaging is testable, acceptance provenance artifacts per job. Windows/Android/Linux jobs are added when those platforms enter scope.

## Scope
Signing separated from ordinary PR CI; native harness scheduling and result retention.

## Acceptance Criteria
Each target builds from clean checkout and release branches require native acceptance evidence.
