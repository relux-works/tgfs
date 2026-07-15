# Implement document open and streaming

## Description
Bridge ParcelFileDescriptor/proxy/pipe strategy to shared ranged transfer and materialized cache.

## Scope
Random/sequential reads as supported, cancellation, source errors, and version changes.

## Acceptance Criteria
Client integration tests verify bytes, closure, cancellation, no descriptor leaks, and restart recovery.
