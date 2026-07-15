# Build crash and restart fault-injection harness

## Description
Interrupt checkpoints in cursors, DB migrations, rendering, transfers, promotion, eviction, provider callbacks, and logout.

## Scope
Deterministic seeds and invariant verification.

## Acceptance Criteria
Every injected interruption converges or produces explicit repair state without partial publication or Telegram writes.
