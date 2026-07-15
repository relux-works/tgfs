# Implement Windows provider host runtime

## Description
Build callback executor, async bridge, cancellation, durable queues, TDLib lifecycle, and structured logging.

## Scope
Rust process architecture and concurrency invariants.

## Acceptance Criteria
Stress/restart tests pass without use-after-free/deadlock; callbacks complete within deadlines or durable continuation.
