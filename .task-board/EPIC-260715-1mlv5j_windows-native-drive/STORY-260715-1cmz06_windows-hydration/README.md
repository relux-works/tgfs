# CfAPI range hydration and pin state

## Description
Implement range FETCH_DATA, progress, cancellation, version validation, pin/offline state, dehydration reconciliation, and read-only failures.

## Scope
Shared transfer/cache engine with native CfExecute operations.

## Acceptance Criteria
Large/random ranges match source bytes; cancel/reboot resumes safely; pinned/in-sync state is correct; write attempts fail predictably.
