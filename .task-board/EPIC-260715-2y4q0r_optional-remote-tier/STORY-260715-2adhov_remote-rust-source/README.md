# Rust RemoteDriveSource client

## Description
Implement authenticated HTTP source, streaming ranges, cursors, retries, offline/error mapping, token storage, and conformance.

## Scope
Shared core integration for desktop/mobile and iOS cold hydration.

## Acceptance Criteria
Passes same source conformance as fake/local; secrets use platform storage; network loss/retry/cancel/version tests are deterministic.
