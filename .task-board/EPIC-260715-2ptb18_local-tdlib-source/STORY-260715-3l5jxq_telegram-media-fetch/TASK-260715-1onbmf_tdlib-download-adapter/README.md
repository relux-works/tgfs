# Implement TDLib download adapter

## Description
Translate ranged/shared-core fetch intent into TDLib file downloads with priority, offset/limit where supported, progress, cancel, and local-file handoff.

## Scope
Version verification and temporary-file ownership.

## Acceptance Criteria
Downloads resume/retry safely, cancellation propagates, locator refresh is invisible to identity, and conformance bytes match.
