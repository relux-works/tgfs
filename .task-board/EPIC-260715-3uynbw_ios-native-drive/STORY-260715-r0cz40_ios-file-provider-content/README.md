# iOS materialized content and cold hydration

## Description
Serve already materialized files, thumbnails, cancellation, and the approved cold-hydration behavior within extension constraints.

## Scope
No TDLib inside extension; App Group handoff and optional remote/minimal fetch path only if approved.

## Acceptance Criteria
Files opens correct versions, failure UX is actionable, memory stays below target, and partial/stale content is never published.
