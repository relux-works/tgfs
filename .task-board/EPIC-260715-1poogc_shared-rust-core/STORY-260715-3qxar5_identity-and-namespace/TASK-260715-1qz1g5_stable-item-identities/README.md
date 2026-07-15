# Implement stable item identities

## Description
Define typed canonical and appearance keys and their opaque provider serialization.

## Scope
Account, list/folder, chat, generated document, message attachment, and blob-related identities.

## Acceptance Criteria
Round-trip/property tests prove determinism, namespace separation, version compatibility, and no path/title dependence.
