# Implement schema migration framework

## Description
Support forward migrations, resumable long work, crash checkpoints, compatibility checks, and repair markers.

## Scope
SQLite and serialized durable formats.

## Acceptance Criteria
Every migration has fixture from prior schema, interruption test, idempotent resume, and clear incompatible-version error.
