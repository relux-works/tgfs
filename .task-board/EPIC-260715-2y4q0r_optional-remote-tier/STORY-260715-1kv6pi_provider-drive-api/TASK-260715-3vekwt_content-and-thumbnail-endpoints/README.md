# Implement ranged content and thumbnail endpoints

## Description
Authorize and stream exact ranges/thumbnails with content versions, cancellation, rate limits, and integrity metadata.

## Scope
Local/S3 blobs, source-on-demand policy if any, and cache headers.

## Acceptance Criteria
Range/concurrency/cancel tests pass; URLs/tokens expire; content cannot be fetched across account/device authorization.
