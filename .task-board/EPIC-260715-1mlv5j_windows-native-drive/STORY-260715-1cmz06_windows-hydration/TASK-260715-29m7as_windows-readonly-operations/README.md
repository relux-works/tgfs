# Enforce read-only Explorer semantics

## Description
Handle create/write/rename/move/delete notifications and races without changing Telegram.

## Scope
Capability/attributes, error/status UX, cache-only actions, and source updates.

## Acceptance Criteria
Native tests prove no Telegram write path and stable errors/reconciliation for unsupported local mutations.
