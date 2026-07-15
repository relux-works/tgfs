# Implement iOS App Group state protocol

## Description
Define multi-process SQLite/files/queue ownership between app and File Provider extension.

## Scope
Schema access, locks, migrations, notifications, materialized file handoff, and logout cleanup.

## Acceptance Criteria
Stress/crash tests avoid corruption and shared-memory assumptions; extension can enumerate while app is unavailable.
