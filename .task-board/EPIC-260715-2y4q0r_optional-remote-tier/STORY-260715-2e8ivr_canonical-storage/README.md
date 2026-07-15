# Canonical metadata and blob storage

## Description
Implement service database schema, migrations, content-addressed blobs, provenance, deduplication, retention, quotas, and backup/restore.

## Scope
PostgreSQL or selected store plus local/S3-compatible blob interface.

## Acceptance Criteria
Transactional invariants and migration/backup/restore tests pass; tenant/account isolation and hash/integrity are enforced.
