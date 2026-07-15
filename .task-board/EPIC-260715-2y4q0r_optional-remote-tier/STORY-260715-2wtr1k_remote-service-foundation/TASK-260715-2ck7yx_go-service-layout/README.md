# Define Go service module boundaries

## Description
Create modules for account/session worker, ingest, normalized store, blob store, API/auth, jobs, and observability.

## Scope
Dependency direction, interfaces, generated Rust/Go contract, and configuration.

## Acceptance Criteria
Architecture checks prevent cycles/leakage; modules have clear ownership and test seams.
