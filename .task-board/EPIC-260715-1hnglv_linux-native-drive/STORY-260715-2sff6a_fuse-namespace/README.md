# FUSE namespace and identity

## Description
Implement lookup/readdir/getattr/statfs and durable inode mapping over shared items.

## Scope
Pagination, multiple appearances, rename/reorder, large folders, xattr policy, and read-only flags.

## Acceptance Criteria
Common fixtures enumerate identically; inode identity survives path changes/restart; unsupported operations return correct errors.
