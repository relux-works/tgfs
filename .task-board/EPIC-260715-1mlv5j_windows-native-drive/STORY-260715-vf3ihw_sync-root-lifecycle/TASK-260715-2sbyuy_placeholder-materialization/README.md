# Implement placeholder creation and updates

## Description
Batch child placeholders with stable identity, size/timestamps/attributes, partial directories, and version updates.

## Scope
Large folders, rename/reorder, delete from source, collisions, and long paths.

## Acceptance Criteria
Fixture namespace matches shared tree; identity survives path changes; no transient normal file is exposed.
