# Enforce Linux read-only operations

## Description
Return correct errors for create/write/truncate/rename/unlink/mkdir/rmdir and avoid Telegram mutation.

## Scope
Kernel/application variations and cache-only explicit actions outside mount.

## Acceptance Criteria
Integration tests prove no write path and stable errno/reconciliation behavior.
