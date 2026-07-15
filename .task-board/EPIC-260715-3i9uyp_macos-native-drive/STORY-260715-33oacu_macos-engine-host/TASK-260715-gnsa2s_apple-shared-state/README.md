# Implement Apple shared durable state coordination

## Description
Coordinate app, agent, and File Provider using App Group files/SQLite and narrow services.

## Scope
Locking, short transactions, notification/change signals, migration, and corruption recovery.

## Acceptance Criteria
Multi-process stress and crash tests pass without shared-memory assumptions or database corruption.
