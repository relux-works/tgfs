# Implement stable inode mapping

## Description
Map opaque ItemId values to durable/reconstructible inode numbers with collision handling and account namespace.

## Scope
Restart, database rebuild, multiple appearances, and deleted/recreated items.

## Acceptance Criteria
Property/restart tests preserve mapping and never treat paths as identity.
