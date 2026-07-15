# gotd Telegram ingest and Takeout

## Description
Implement encrypted session workers, authorization handoff, user-approved Takeout backfill, incremental updates, gaps, flood waits, and deletion/edit mapping.

## Scope
One logical worker per account with durable jobs/checkpoints.

## Acceptance Criteria
Backfill/restart is idempotent, updates converge, flood waits are honored, sessions are isolated/encrypted, and restrictions are enforced.
