# Deterministic chat rendering

## Description
Implement versioned NDJSON, Markdown, chat metadata, partitioning, atomic publication, and renderer fixtures.

## Scope
PRD-020 through PRD-024 and SYNC-030 through SYNC-034.

## Acceptance Criteria
Same structured input/version produces byte-identical output; affected partitions update on edits/deletes; large histories remain bounded.
