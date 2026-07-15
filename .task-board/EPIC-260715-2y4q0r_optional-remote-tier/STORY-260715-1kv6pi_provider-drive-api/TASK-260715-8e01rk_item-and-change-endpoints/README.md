# Implement item enumeration and change endpoints

## Description
Serve roots/items/children/pages and durable account-scoped changes with opaque cursors and version negotiation.

## Scope
Authorization, ETags/versions, invalid cursor recovery, and large folders.

## Acceptance Criteria
Conformance pagination/cursor/version tests pass and no cross-account metadata is observable.
