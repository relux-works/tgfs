# Implement edit and deletion policy mapping

## Description
Apply the approved retention decision to current state, optional observed tombstones, generated versions, and provider changes.

## Scope
No recovery claims for unseen revisions/deletions.

## Acceptance Criteria
Policy fixtures are deterministic, privacy controls are honored, and cache eviction remains separate.
