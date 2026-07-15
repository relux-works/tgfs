# Implement ranged fetch coordinator

## Description
Coalesce compatible readers, align backend chunks, refresh source locators, apply retry taxonomy, and stream to sinks.

## Scope
Concurrent opens, cancellation, file-reference refresh, and version changes.

## Acceptance Criteria
Range bytes are correct; cancellation is prompt; stale version cannot publish; duplicate compatible network work is bounded.
