# Implement gotd account/session worker

## Description
Manage authorization, encrypted session persistence, DC migration, connection lifecycle, bounded scheduler, and revocation.

## Scope
Multi-account service boundary and operator-safe diagnostics.

## Acceptance Criteria
Workers isolate accounts, recover after restart, never log keys/content, and expose actionable health.
