# Optional remote archive and drive source

## Description
Provide a self-hostable gotd/td service and RemoteDriveSource without changing shared drive semantics.

## Scope
Telegram ingest, canonical metadata/blob storage, provider API, auth/security, deployment/operations, and iOS/client integration.

## Acceptance Criteria
Remote source passes shared conformance, Takeout/incremental idempotency, tenant/security controls, range delivery, and operational gates before any hosted use.
