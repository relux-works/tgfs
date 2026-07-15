# Design and implement SQLite schema

## Description
Implement versioned tables/indexes for account, item, appearances, chats/messages/attachments, transfers, cache, cursors, and render state.

## Scope
Local-mode canonical metadata plus provider/cache state; remote clients may use a subset.

## Acceptance Criteria
Schema enforces key invariants, supports required queries without full scans, and has synthetic large-account fixtures.
