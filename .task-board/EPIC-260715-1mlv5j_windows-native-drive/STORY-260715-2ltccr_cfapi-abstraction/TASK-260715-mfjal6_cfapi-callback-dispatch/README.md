# Implement callback dispatch and cancellation

## Description
Translate FETCH_DATA, CANCEL_FETCH_DATA, FETCH_PLACEHOLDERS, and relevant notifications into cancellable core requests.

## Scope
Concurrency, range requests, progress, teardown, and duplicate callbacks.

## Acceptance Criteria
Synthetic native harness verifies exact completion/cancel behavior and safe disconnect under load.
