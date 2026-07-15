# Synchronization and Filesystem Semantics

Status: planning baseline
Last updated: 2026-07-15

## Source contract

- **SYNC-001 (V1):** Local and remote sources expose the same provider-neutral item, page, change, fetch, thumbnail, cancellation, and error semantics.
- **SYNC-002 (V1):** A conformance suite runs against a deterministic fake source and every source implementation.
- **SYNC-003 (V1):** Enumeration is paginated and repeatable for a declared snapshot/version; duplicate or missing children across pages are contract failures.
- **SYNC-004 (V1):** Change cursors survive normal process restart and reject account/schema mismatches explicitly.
- **SYNC-005 (V1):** Provider callbacks have bounded deadlines; long work is cancellable or converted into durable background/transfer state.

## Tree layout

Default logical view:

```text
Account/
  Main/
    Chat/
      chat.json
      messages.ndjson
      2026/
        07.md
        media/
  Archive/
  Telegram Folders/
```

- **SYNC-010 (V1):** The layout is virtual; duplicate appearances reference shared canonical records/blobs.
- **SYNC-011 (V1):** Numeric order prefixes are an optional presentation mode. Stable-name mode stores exact order in metadata/`order.json` and does not rename folders merely because the Telegram position changed.
- **SYNC-012 (V1):** Collision suffixes are deterministic from stable identity, not discovery order.
- **SYNC-013 (V1):** Reserved names, separators, control characters, trailing dots/spaces, Unicode normalization, and path-length budgets are handled for the strictest supported target.

## Message synchronization

- **SYNC-020 (V1):** Initial local mode discovers chat metadata first; it does not eagerly download every media object.
- **SYNC-021 (V1):** History traversal is resumable per chat/range and idempotent by Telegram message identity.
- **SYNC-022 (V1):** Incremental updates apply in source order and persist a checkpoint transactionally with normalized state.
- **SYNC-023 (V1):** Detected gaps trigger source-specific recovery before advancing the durable cursor.
- **SYNC-024 (V1):** Message edits replace current rendered state and change affected generated-document versions.
- **SYNC-025 (V1):** Deletions observed after synchronization remove or tombstone current records according to the selected product policy; source deletion and cache eviction remain distinct.
- **SYNC-026 (V1):** Chat title/list/folder/order changes update appearances without changing canonical chat, attachment, or blob identity.

## Deterministic rendering

- **SYNC-030 (V1):** NDJSON has an explicit schema version and deterministic field/record order.
- **SYNC-031 (V1):** Markdown partitioning is bounded, deterministic, timezone-explicit, and reproducible from the same structured inputs and renderer version.
- **SYNC-032 (V1):** Rendering uses stable attachment paths/links and handles missing/unavailable content explicitly.
- **SYNC-033 (V1):** Atomic publication prevents readers from observing a partially regenerated document.
- **SYNC-034 (V1):** Renderer fixtures cover Unicode, entities, replies, albums, topics, edits, reactions, service messages, missing senders, and deleted/unavailable media.

## Hydration and transfer

- **SYNC-040 (V1):** Dataless placeholders are default; enumeration must not hydrate content.
- **SYNC-041 (V1):** Fetch accepts byte ranges even if a source internally downloads larger aligned chunks.
- **SYNC-042 (V1):** Partial data is stored under a transfer identity and promoted atomically only after version and integrity checks.
- **SYNC-043 (V1):** Cancellation stops network and disk work promptly where supported and leaves resumable or safely disposable state.
- **SYNC-044 (V1):** Retries classify flood wait, transient network, expired file reference, authorization, source deletion, unsupported/protected content, disk full, and integrity failure.
- **SYNC-045 (V1):** File-reference refresh never changes provider item identity.
- **SYNC-046 (V1):** Concurrent requests for the same item/version coalesce where safe and do not corrupt range/accounting state.

## Cache and pinning

- **SYNC-050 (V1):** Cache accounting includes materialized blobs, partial transfers, generated documents, thumbnails, and required metadata separately.
- **SYNC-051 (V1):** Eviction never removes explicitly pinned/offline content unless the user chooses an emergency policy that is clearly disclosed.
- **SYNC-052 (V1):** LRU-like eviction operates only on eligible verified content and preserves source/item metadata needed to rehydrate.
- **SYNC-053 (V1):** Provider/system eviction is reconciled into TGFS cache state.
- **SYNC-054 (V1):** Quota changes are durable and immediately produce an actionable plan/status rather than silent data loss.

## Read-only behavior

- **SYNC-060 (V1):** Create, modify, rename, move, and delete capabilities are not advertised through native providers.
- **SYNC-061 (V1):** Attempts from clients that ignore capabilities fail with a stable read-only error.
- **SYNC-062 (V1):** Companion actions distinguish “remove local copy,” “unpin,” “remove from archive view,” and any future Telegram delete.
- **SYNC-063 (Future):** Write support, if introduced, uses explicit product actions and a separately approved conflict/deletion specification.

## Reconciliation

- **SYNC-070 (V1):** Startup recovery reconciles durable transfers, item versions, provider registrations, and missing/extra cache files.
- **SYNC-071 (V1):** A user-triggered repair rebuilds projections from structured state without changing Telegram data.
- **SYNC-072 (V1):** Database migration and renderer migration are resumable and crash-safe.
- **SYNC-073 (V1):** Clock changes do not reorder identity or corrupt cursors; source timestamps remain explicit.
