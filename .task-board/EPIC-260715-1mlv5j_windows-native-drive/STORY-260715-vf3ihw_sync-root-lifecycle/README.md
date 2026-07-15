# Windows sync-root and placeholder lifecycle

## Description
Register per-account sync roots and create/update/remove stable placeholders from core items.

## Scope
Hydration/population policies, file identity, metadata, read-only flags, reconnect, and cleanup.

## Acceptance Criteria
One stable root per account; placeholders survive reboot/upgrade; repair/removal is idempotent and safe.
