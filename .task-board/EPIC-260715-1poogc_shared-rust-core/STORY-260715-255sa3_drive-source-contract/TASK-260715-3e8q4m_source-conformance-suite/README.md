# Build DriveSource conformance suite

## Description
Test pagination, cursor durability, version races, range correctness, retries, cancellation, capabilities, and account/schema mismatches.

## Scope
Runs unchanged against fake, tdjson, and remote implementations.

## Acceptance Criteria
Suite reports backend-independent failures and covers all SYNC-001 through SYNC-005 acceptance cases.
