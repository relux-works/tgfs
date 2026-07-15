# Implement safe asynchronous tdjson wrapper

## Description
Wrap client creation/send/receive/execute/destroy, request IDs, update dispatch, cancellation, and error conversion.

## Scope
One receive owner, bounded queues, shutdown coordination, and test doubles.

## Acceptance Criteria
Concurrency/lifecycle tests pass under cancellation and shutdown; no returned C pointers outlive their validity.
