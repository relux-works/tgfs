# Transfer, hydration, and cache engine

## Description
Implement durable ranged hydration, resume, coalescing, hashing, promotion, cache accounting, pinning, eviction, and recovery.

## Scope
SYNC-040 through SYNC-054 across local and remote sources.

## Acceptance Criteria
Large files stream without whole-file buffering; cancellation/restart/version races are safe; quota/pin invariants pass property and crash tests.
