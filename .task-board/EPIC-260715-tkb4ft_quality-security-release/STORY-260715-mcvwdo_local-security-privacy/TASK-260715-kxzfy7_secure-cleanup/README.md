# Verify secure account and cache cleanup

## Description
Test logout/local removal/uninstall/repair across credentials, session DB, metadata, blobs, partials, diagnostics, and provider registrations.

## Scope
Per-platform storage behavior and user retain/delete choices.

## Acceptance Criteria
Interrupted cleanup resumes; no credential remains; retained/exported content matches explicit choice; verification evidence is platform-specific.
