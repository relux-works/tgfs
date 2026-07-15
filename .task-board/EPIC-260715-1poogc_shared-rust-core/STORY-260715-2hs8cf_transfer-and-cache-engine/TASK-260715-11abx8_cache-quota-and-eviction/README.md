# Implement cache quota, pinning, and eviction

## Description
Track metadata/thumbnails/generated/partial/blob usage and enforce eligible LRU-like eviction.

## Scope
Pinned protection, quota changes, system eviction reconciliation, and storage-full state.

## Acceptance Criteria
Pinned data is preserved by default; accounting matches disk fixtures; quota pressure produces deterministic actionable state.
