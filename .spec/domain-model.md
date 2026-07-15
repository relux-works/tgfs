# Domain Model

Status: planning baseline
Last updated: 2026-07-15

## Invariants

- **DOM-001:** Every provider-visible item has a stable opaque `ItemId` independent of title, order, filename, and path.
- **DOM-002:** Source identity and virtual appearance identity are separate. One chat or blob may have multiple virtual appearances.
- **DOM-003:** Every mutable item has a monotonic or content-derived `Version` sufficient to detect stale metadata/content.
- **DOM-004:** Every change page is anchored by an opaque durable `ChangeCursor`; cursors are scoped to source/account and contract version.
- **DOM-005:** A path is derived presentation state, never a foreign key.
- **DOM-006:** Generated text is derived from structured message records and renderer/schema versions.
- **DOM-007:** Telegram remote locators/file references are refreshable source metadata, not durable item identity.
- **DOM-008:** Local cache state never changes Telegram source state.

## Core entities

### Account

Represents one configured source identity.

Key fields: `account_id`, source kind, display name, authorization state, namespace version, created/updated timestamps. Secrets are references to platform secure storage, never database plaintext fields.

### Source

Implements the normalized drive contract. V1 implementations are `local_tdlib`; optional implementation is `remote_http`. Source-specific records stay behind the adapter.

### Item

Provider-neutral file or directory metadata:

- `item_id`
- `account_id`
- `parent_id` for one virtual appearance
- `kind` (`root`, `list`, `folder_view`, `chat`, `year`, `month`, `media_dir`, `file`, `generated_file`)
- display and safe names
- MIME type and logical size
- metadata/content version
- created/modified timestamps
- capabilities
- materialization hints
- provenance reference

The same canonical object can appear through multiple `Item` appearances with distinct `item_id` values derived from stable appearance context.

### Chat

Canonical Telegram cloud chat metadata: stable Telegram chat/peer ID, type, title, username, list positions, folder memberships, protected-content state, last observed update, and deletion/left state.

### Chat appearance

Maps a canonical chat into Main, Archive, or a custom folder view. It owns display order metadata but not chat content.

### Message record

Current observed message state plus optional locally observed event history:

- chat and message IDs
- sender identity snapshot/reference
- timestamp and edit timestamp
- text/caption and entities
- reply/thread/topic/album relationships
- reactions and service action
- attachment references
- save/protection flags
- observed deletion tombstone if enabled
- raw-schema/version metadata needed for lossless migration

Historical revisions that were never observed are not implied.

### Attachment

A virtual downloadable object tied to a message and attachment index. It records original metadata, logical content identity, Telegram locator/file ID/reference, safe display name, size, MIME type, availability/saveability, and last verification time.

### Blob

Locally materialized or remotely canonical bytes identified after complete download by strong content hash. Blob identity does not replace attachment identity. Partial downloads use temporary transfer IDs and are not blobs.

### Generated document

An NDJSON/Markdown/chat metadata view with `renderer_version`, `schema_version`, bounded source range, input watermark/version, content hash when materialized, and deterministic logical size when known.

### Transfer

Durable hydration/download operation: item, requested ranges, source version, temporary path/handle, completed ranges, retry state, priority, cancellation state, and terminal result.

### Cache entry

Maps an item/version to materialized bytes and tracks size, access time, pin intent, eviction eligibility, verification state, and platform-specific materialization reference.

## Identity scheme

- **DOM-020:** IDs must be opaque at the provider boundary and stable across database rebuilds from unchanged source data.
- **DOM-021:** Telegram-derived canonical keys include account ID, peer/chat ID, message ID, attachment index, and source namespace version as applicable.
- **DOM-022:** Virtual appearance IDs additionally include list/folder view identity; moving between views creates/removes appearances without changing canonical chat identity.
- **DOM-023:** Generated-file IDs include chat identity, partition key, format, and schema family, but not the current chat title.
- **DOM-024:** Windows file identity payloads, Apple item identifiers, Android document IDs, and Linux inode mapping all resolve through the same stable `ItemId` namespace.

## Versioning

- Metadata version changes when provider-visible metadata or parent membership changes.
- Content version changes when bytes returned for the same item may change.
- Renderer/schema upgrades change generated-document content versions even if Telegram messages are unchanged.
- A fetch started for version A must not be published as version B. The adapter either completes A consistently or restarts against B.

## Canonical ownership

In local mode, TDLib/source state plus TGFS structured metadata are canonical; provider placeholders and generated files are projections. In remote mode, the service database/blob store are canonical for the normalized archive; client databases remain cache/provider state.
