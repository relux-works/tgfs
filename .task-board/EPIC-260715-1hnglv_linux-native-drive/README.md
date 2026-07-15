# Linux native drive

## Description
Deliver a local-first read-only FUSE mount and user service over the shared Rust core and TDLib.

## Scope
Daemon lifecycle, mount/namespace/IO semantics, cache, packaging, diagnostics, and acceptance.

## Acceptance Criteria
Mount passes stable inode, read, restart, cache, error, and packaging gates on the selected reference distribution.
