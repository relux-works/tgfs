# Provider-oriented Drive API

## Description
Serve normalized item enumeration, durable changes, metadata versions, authenticated byte ranges, thumbnails, and health for RemoteDriveSource.

## Scope
HTTP transport, pagination, caching headers, streaming/backpressure, cancellation, errors, and version negotiation.

## Acceptance Criteria
Remote source passes full conformance; stale/unauthorized ranges fail safely; large responses stream within budgets.
