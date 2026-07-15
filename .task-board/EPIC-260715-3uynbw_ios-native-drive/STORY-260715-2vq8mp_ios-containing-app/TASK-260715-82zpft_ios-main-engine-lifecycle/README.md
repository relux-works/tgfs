# Implement iOS main-app engine lifecycle

## Description
Initialize TDLib/core, schedule metadata sync within platform limits, service queued materialization requests, and persist checkpoints.

## Scope
Foreground/background transitions, termination, protected data availability, network changes, and power constraints.

## Acceptance Criteria
Lifecycle tests recover without duplicate clients or lost durable requests; unsupported work waits in explicit state.
