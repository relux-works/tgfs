# Implement iOS File Provider domain lifecycle

## Description
Register/remove/repair per-account domains and coordinate auth unavailable states.

## Scope
First run, reinstall/upgrade, logout, multiple account path, and stale system state.

## Acceptance Criteria
Domain lifecycle is idempotent and leaves no broken Files location after removal.
