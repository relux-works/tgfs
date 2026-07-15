# Metadata state and migrations

## Description
Implement local SQLite schema, repositories, transactions, migrations, checkpoints, reconciliation metadata, and multi-process-safe access patterns.

## Scope
Canonical/local projections, cache/provider state, transfer journal, generated-document watermarks, and account namespaces.

## Acceptance Criteria
Crash tests preserve invariants; migrations are resumable; cursors advance atomically with normalized state; Apple multi-process patterns are measured.
