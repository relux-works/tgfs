# Implement ordering projection modes

## Description
Implement stable-name/order-metadata and approved numeric-prefix behavior without identity churn.

## Scope
Chat position snapshots, order metadata file, renames, and migration between modes.

## Acceptance Criteria
Reorder fixtures produce expected metadata/path changes while stable IDs and cached content remain intact.
