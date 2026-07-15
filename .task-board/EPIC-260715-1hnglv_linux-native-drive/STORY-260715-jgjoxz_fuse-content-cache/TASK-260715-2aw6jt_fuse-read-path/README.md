# Implement FUSE open/read/release path

## Description
Bridge file handles and byte ranges to shared transfer/materialization while respecting kernel caching.

## Scope
Concurrent/random/sequential reads, stale versions, short reads, and source unavailable.

## Acceptance Criteria
Native tests verify exact bytes, errno mapping, cancellation, descriptor cleanup, and version safety.
