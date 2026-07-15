# Finder hydration, pinning, and content lifecycle

## Description
Implement content fetch/range, progress, cancellation, materialization, eviction reconciliation, thumbnails, and offline intent.

## Scope
Shared transfer/cache engine and system-managed local copies.

## Acceptance Criteria
Open/pin/cancel/retry/restart fixtures return correct versions and never publish partial content.
