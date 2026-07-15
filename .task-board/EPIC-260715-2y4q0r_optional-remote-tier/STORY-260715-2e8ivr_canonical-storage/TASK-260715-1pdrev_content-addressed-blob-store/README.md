# Implement blob storage abstraction

## Description
Support local and S3-compatible complete/partial objects, hash verification, range reads, dedup, quarantine, and garbage collection.

## Scope
Tenant authorization/provenance, multipart transfer, and backup semantics.

## Acceptance Criteria
Corrupt/truncated blobs fail closed; range bytes match; GC never removes referenced/pinned/retained content.
