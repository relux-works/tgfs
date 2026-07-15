# Implement Android engine lifecycle

## Description
Initialize/reuse/shutdown TDLib/core across app/provider entry points and process recreation.

## Scope
Concurrency, WorkManager/background constraints, cancellation, network/power state, and logout.

## Acceptance Criteria
Lifecycle stress tests avoid duplicate clients, leaked sessions, deadlocks, or lost durable work.
