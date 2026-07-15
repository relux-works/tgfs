# Implement incremental render planning

## Description
Compute affected generated documents from normalized changes and renderer/schema versions.

## Scope
Watermarks, edits/deletes, partition changes, and atomic version publication.

## Acceptance Criteria
Only affected partitions regenerate; interrupted regeneration leaves old valid version or resumes safely; no partial file is published.
