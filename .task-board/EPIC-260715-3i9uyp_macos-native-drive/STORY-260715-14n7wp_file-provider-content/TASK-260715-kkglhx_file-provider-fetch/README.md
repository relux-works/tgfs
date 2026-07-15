# Implement File Provider content fetch

## Description
Bridge fetch requests to shared transfer engine and return atomically materialized content.

## Scope
Range/partial support where applicable, version races, cancellation, and source unavailable.

## Acceptance Criteria
Large-file streaming and cancellation tests pass; stale versions restart/fail safely; memory remains bounded.
