# Implement remote incremental update pipeline

## Description
Apply normal MTProto updates/history recovery after Takeout with durable per-account checkpoints.

## Scope
Edits/deletes/chat positions/file references and gap recovery.

## Acceptance Criteria
Replay/crash tests converge and never advance checkpoints without normalized state.
