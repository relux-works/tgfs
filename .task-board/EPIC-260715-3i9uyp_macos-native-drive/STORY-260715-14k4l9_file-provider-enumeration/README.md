# Finder namespace enumeration and changes

## Description
Implement File Provider items/enumerators, pagination, working set, metadata versions, and remote-change signaling over the core.

## Scope
Roots, chats, generated files, media placeholders, duplicate appearances, and order changes.

## Acceptance Criteria
Common fixtures enumerate identically; stable IDs survive rename/reorder; working-set and change anchors recover after restart.
