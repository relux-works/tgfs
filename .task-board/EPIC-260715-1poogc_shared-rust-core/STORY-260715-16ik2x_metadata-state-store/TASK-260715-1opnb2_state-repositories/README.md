# Implement transactional state repositories

## Description
Provide typed operations for snapshots, changes, versions, transfers, cache, and render watermarks.

## Scope
Short transactions, cancellation boundaries, and no FFI leakage.

## Acceptance Criteria
Repository tests cover atomic cursor application, idempotent replay, version conflict, and concurrent readers/writers.
