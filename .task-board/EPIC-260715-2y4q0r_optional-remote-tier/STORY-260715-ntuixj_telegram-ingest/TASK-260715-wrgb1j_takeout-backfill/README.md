# Implement Takeout backfill job

## Description
Run user-confirmed Takeout session, handle security delay, paginate selected dialogs/history/media, persist checkpoints, and close session.

## Scope
Private/groups/channels/files selection, size policy, retry, cancellation, and notification UX state.

## Acceptance Criteria
Interrupted job resumes idempotently, delay is surfaced, no duplicate records/blobs result, and session closes on terminal paths.
