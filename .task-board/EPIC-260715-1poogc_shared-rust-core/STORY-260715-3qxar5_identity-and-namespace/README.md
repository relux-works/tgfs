# Identity and virtual namespace

## Description
Implement stable IDs, canonical/appearance separation, safe names, ordering metadata, and virtual tree construction.

## Scope
DOM-001 through DOM-024, PRD-010 through PRD-014, and cross-platform fixtures.

## Acceptance Criteria
Unchanged source data rebuilds identical IDs/tree; rename/reorder does not change canonical identity; collision fixtures pass on all targets.
