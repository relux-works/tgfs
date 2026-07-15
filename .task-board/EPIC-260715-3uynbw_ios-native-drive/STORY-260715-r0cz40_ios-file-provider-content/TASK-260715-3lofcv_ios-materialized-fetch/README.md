# Serve App Group materialized content

## Description
Return version-validated materialized files and reconcile system/provider/cache state.

## Scope
App terminated, protected data unavailable, stale version, eviction, and concurrent opens.

## Acceptance Criteria
Already hydrated content opens reliably without app; stale/missing state produces repair/fetch behavior, not wrong bytes.
