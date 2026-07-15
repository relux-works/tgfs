# Implement sync-root repair and removal

## Description
Disconnect/unregister roots and reconcile placeholders/cache/session according to user action.

## Scope
Crash/reboot, stale registrations, logout, uninstall, and multiple account roots.

## Acceptance Criteria
Flows are idempotent and leave no broken Explorer namespace or orphan credential.
