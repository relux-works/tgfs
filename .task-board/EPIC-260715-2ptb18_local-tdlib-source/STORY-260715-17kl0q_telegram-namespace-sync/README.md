# Telegram chat namespace synchronization

## Description
Map TDLib chat lists, positions, folders, titles, usernames, protection flags, and memberships into normalized source items/changes.

## Scope
Main, Archive, custom folders, multiple appearances, paging, and order snapshots.

## Acceptance Criteria
Conformance enumeration passes; position/title/folder changes preserve stable identity; protected capabilities are correct.
