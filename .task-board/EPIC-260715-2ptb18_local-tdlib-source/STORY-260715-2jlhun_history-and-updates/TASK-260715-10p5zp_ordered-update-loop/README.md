# Implement ordered update loop and gap recovery

## Description
Persist normalized updates and source checkpoints transactionally and recover TDLib gaps/restarts.

## Scope
Message and chat changes plus new/edited/deleted updates.

## Acceptance Criteria
Crash/replay tests never advance cursor without state; gaps recover before publication; duplicates are idempotent.
