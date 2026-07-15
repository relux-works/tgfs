# Implement shared core CI

## Description
Run Rust format/lint/unit/property/conformance/migration/benchmark smoke, audit/deny/license, secret, and doc checks.

## Scope
Host matrix and caching with pinned toolchains.

## Acceptance Criteria
Pull requests cannot merge with required failure; cache cannot alter results; logs contain no secrets.
