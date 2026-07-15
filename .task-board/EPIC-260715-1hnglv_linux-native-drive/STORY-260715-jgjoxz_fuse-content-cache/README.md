# FUSE read and cache behavior

## Description
Implement open/read/release with ranged hydration, concurrent readers, interruption, pin/cache policy, and source errors.

## Scope
Read-only filesystem semantics and shared transfer/cache engine.

## Acceptance Criteria
Large/random reads match source, interrupts cancel safely, partial data is not published, and cache survives daemon restart.
