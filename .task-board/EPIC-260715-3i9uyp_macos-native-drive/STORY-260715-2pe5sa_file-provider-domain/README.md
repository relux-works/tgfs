# File Provider domain lifecycle

## Description
Register, configure, reconnect, migrate, and remove File Provider domains per account.

## Scope
Domain identifiers, display names, auth unavailable states, upgrades, and cleanup.

## Acceptance Criteria
Registration/removal is idempotent; stale domains repair; logout/uninstall leaves no broken Finder root.
