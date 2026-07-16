# Product Specification

Status: planning baseline
Last updated: 2026-07-17

## Product statement

GramDrive presents a user's accessible Telegram cloud chats as a native, read-only cloud drive. Chats are folders; media and documents are files; chat history is exposed as deterministic text and structured exports. The system file manager is the primary interface, with a minimal native companion for authorization, status, cache, diagnostics, and account removal.

The product should feel like Dropbox or Google Drive at the operating-system boundary, while preserving Telegram-specific identity, ordering, update, and content-policy constraints.

## Target users

1. A user who needs a durable, browsable personal archive of Telegram files and conversations.
2. A user who wants Telegram media available to ordinary desktop/mobile applications through the system file picker.
3. A researcher or professional who needs deterministic chat exports without manually exporting each conversation.
4. A self-hosting user who may later prefer a canonical remote archive shared by multiple devices.

## V1 principles

- Native-drive experience before a rich application UI.
- Local-first on macOS, Windows, Android, and Linux.
- Shared Rust engine with thin native integration layers.
- Read-only Telegram semantics.
- Dataless/on-demand files by default; explicit offline pinning.
- Honest handling of content Telegram does not make accessible or saveable.
- No mandatory web application or hosted infrastructure.

## Primary journeys

### J1 — Connect an account

The user installs the companion application, enters the Telegram authorization flow, completes any required code/password challenge, selects initial synchronization options, and sees a drive root appear in the native file manager.

### J2 — Browse chats as folders

The user opens the drive root, navigates Main/Archive/custom Telegram folder views, and sees chats in a deterministic representation of Telegram order. Renamed chats retain item identity.

### J3 — Open a Telegram file

The user opens a dataless media/document placeholder. The provider fetches the requested content, reports progress/cancellation through native mechanisms, materializes it atomically, and reuses the cache on subsequent opens.

### J4 — Read exported conversation text

The user opens monthly Markdown or lossless NDJSON files generated from current synchronized message records. A rerun with unchanged source records produces byte-identical output.

### J5 — Keep content offline

Where the OS exposes pin/offline intent, the user requests local availability. GramDrive hydrates the requested content within quota and preserves it until unpinned or explicitly removed.

### J6 — Observe ongoing changes

New messages, edits, deletes, chat title changes, archive moves, and ordering changes appear through the provider's native change mechanism without changing stable item identity.

## Functional requirements

### Account and lifecycle

- **PRD-001 (V1):** Support at least one Telegram user account per installation and preserve a design path to multiple accounts.
- **PRD-002 (V1):** Support phone/code authorization and Telegram two-step verification through the containing application, never through a filesystem callback.
- **PRD-003 (V1):** Provide explicit logout/account removal that revokes or closes the local session and removes provider registration and local sensitive state according to user choice.
- **PRD-004 (V1):** Expose authorization-expired, flood-wait, offline, source-unavailable, and storage-full states without blocking provider callbacks indefinitely.

### Namespace and ordering

- **PRD-010 (V1):** Expose Main and Archive chat lists; custom Telegram folders are V1 if the selected Telegram source exposes them reliably on the platform.
- **PRD-011 (V1):** Represent each chat as a folder using a filesystem-safe display name and a path-independent stable identifier.
- **PRD-012 (V1):** Preserve Telegram ordering inside the product UI and metadata; the filesystem view supports a configurable order-prefix mode because native file managers control sort order.
- **PRD-013 (V1):** Allow the same canonical chat to appear in multiple virtual list/folder views without duplicating canonical message or blob records.
- **PRD-014 (V1):** Resolve name collisions and filesystem-reserved names deterministically on every platform.

### Chat exports

- **PRD-020 (V1):** Expose lossless current-state message data as NDJSON with stable schema versioning.
- **PRD-021 (V1):** Expose bounded human-readable Markdown files, partitioned by month by default.
- **PRD-022 (V1):** Include message identity, time, sender, text/caption entities, replies, topics, albums, reactions, edits, service actions, and attachment references when accessible.
- **PRD-023 (V1):** Generated exports are deterministic views and are never the canonical editable source.
- **PRD-024 (V1):** Clearly represent unavailable, deleted-after-observation, protected, or unsupported content without fabricating recoverability.

### Media and files

- **PRD-030 (V1):** Enumerate accessible and saveable documents, photos, video, audio, voice, animation, stickers, and other downloadable message attachments supported by the source.
- **PRD-031 (V1):** Hydrate content on demand and support cancellation and resume where the source and provider permit it.
- **PRD-032 (V1):** Preserve original filename/MIME/size metadata while exposing a deterministic safe filename.
- **PRD-033 (V1):** Deduplicate stored content internally without merging distinct virtual items or losing Telegram provenance.
- **PRD-034 (V1):** Respect Telegram protected-content and `can_be_saved` restrictions.

### Cache and offline behavior

- **PRD-040 (V1):** Maintain a configurable local cache quota and expose current use in the companion application.
- **PRD-041 (V1):** Distinguish dataless, hydrating, materialized, pinned/offline, stale, failed, and evictable states.
- **PRD-042 (V1):** Never interpret OS cache eviction as Telegram message deletion.
- **PRD-043 (V1):** Recover interrupted hydration without publishing partial content as a valid materialized file.

### Diagnostics

- **PRD-050 (V1):** Provide native status for account state, last successful update, active transfers, cache use, and actionable failures.
- **PRD-051 (V1):** Provide a privacy-scrubbed diagnostic export suitable for support.
- **PRD-052 (V1):** Permit the user to rescan/reconcile provider metadata without redownloading unchanged content.

### Optional remote tier

- **PRD-060 (Optional tier):** Implement a remote source with the same normalized drive contract as the local TDLib source.
- **PRD-061 (Optional tier):** Support a self-hosted gotd/td service with Takeout backfill, incremental updates, canonical metadata, and range-addressable blob delivery.
- **PRD-062 (Optional tier):** Use revocable per-device product tokens; remote clients never receive the service's Telegram authorization key.
- **PRD-063 (Optional tier):** Permit iOS to use the remote source for cold hydration when the containing app is unavailable.

## Explicitly out of V1 scope

- Web client or Telegram Web replacement.
- Sending messages or media from arbitrary filesystem writes.
- Rename/move/delete mapping back to Telegram.
- Editing generated Markdown/NDJSON as a way to edit Telegram.
- Recovering messages deleted before first observation or historical edit revisions unavailable from Telegram.
- Globally reconstructing secret chats from other devices.
- Circumventing protected-content, view-once, or self-destruct restrictions.
- Mandatory hosted SaaS.

## Product success gates

V1 is product-complete only when at least macOS and Windows support native placeholders, deterministic enumeration, on-demand hydration, restart recovery, and the same conformance fixtures. Android/Linux may follow the same core contract. iOS release scope must explicitly resolve or disclose cold hydration when the containing app is unavailable.
