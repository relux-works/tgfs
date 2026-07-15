# Implement integrity verification and atomic promotion

## Description
Hash complete content, validate expected size/version, deduplicate blobs, and promote temporary files atomically.

## Scope
Local materialization and optional remote blob identities.

## Acceptance Criteria
Corrupt/truncated data fails closed; distinct attachments preserve provenance; promotion is crash-safe and idempotent.
