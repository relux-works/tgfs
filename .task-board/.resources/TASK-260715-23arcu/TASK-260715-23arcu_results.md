# TASK-260715-23arcu — Map attachment metadata and capabilities

Status: ready for review (board `to-review`).

## Scope delivered

Turns the raw `AttachmentDescriptor` that message normalization (TASK-260715-1ynmct)
already produced into the DOM § Attachment record a consumer keys and names by:
stable identity, original + safe filename, MIME, size, TDLib file locator,
thumbnail descriptor, availability and saveability — for documents, photos,
video, audio, voice, animation, stickers, and (via degradation) unknown types.

## Changes

| File | What |
|---|---|
| `crates/gramdrive-source-tdjson/src/message.rs` | New `ThumbnailFormat`, `ThumbnailDescriptor`, `Minithumbnail` types; `thumbnail`/`minithumbnail` fields on `AttachmentDescriptor`; extraction wired into `media_attachment`/`photo_attachment` (per-kind member names, smallest `photoSize` for photos); `MessageContent::attachment()` accessor; POL-4 fail-closed gating; 7 new unit tests |
| `crates/gramdrive-source-tdjson/src/attachment.rs` (new) | `MappedAttachment` (identity + safe name + album provenance + can_be_saved + descriptor); `map_message_attachments(record, scope)` and `map_attachment(...)`; safe-name derivation via `gramdrive_model::naming::sanitize(File)` with kind+MIME fallback for nameless media; 8 unit tests |
| `crates/gramdrive-source-tdjson/src/lib.rs` | Re-exports the new module and message types |
| `crates/gramdrive-source-tdjson/tests/attachment_metadata.rs` (new) | 6-fixture AC corpus: every downloadable kind, restricted, view-once, albums |
| `LOGBOOK.md` | Decision + finding entry (0626) |

## Metadata contract (per attachment)

- **Stable identity** — `AttachmentKey` = account scope + chat + message + ordinal (DOM-021); never a Telegram locator, so a reference refresh (SYNC-045) can't change it.
- **Original + safe name** (PRD-032) — original preserved verbatim in the descriptor; `safe_name` is `sanitize(original_or_kind_default, NameKind::File)`. Nameless media (photo/voice/video-note/sticker) gets a deterministic kind+MIME default. Sibling collisions are the media directory's job (`resolve_siblings`), not settled here.
- **MIME, size, locator** — verbatim from the descriptor (`mime_type`, `size`, `file_id` + `remote_id` + `remote_unique_id`).
- **Thumbnail descriptor** — downloadable `ThumbnailDescriptor` (format + dims + locator) and inline `Minithumbnail` (base64 as tdjson delivers; decoding deferred to the thumbnail source, keeping this crate base64-dep-free).
- **Availability + saveability** (POL-4) — descriptor's derived `AttachmentAvailability` plus Telegram's raw `can_be_saved` (distinct facts).
- **Provenance** — `album_id` carried; PRD-033 dedup key (`remote_unique_id`) rides along, never merged.

## Key decision — POL-4 fail-closed previews

Thumbnails **and** minithumbnails are captured **only for `Fetchable`** attachments.
Restricted (`can_be_saved=false`) and ViewOnce (self-destruct/secret) carry **no
preview bytes or locators at all** — a non-fetchable attachment is a pure
placeholder that can never leak or persist protected/ephemeral content.

This is conservative (withholds data, never adds ToS risk), so it does not trip
the POL-8 owner-escalation. Forward note for the blocked-by task
TASK-260715-3nl3mu (thumbnail-source): Telegram's own clients show minithumbnails
for protected content; relaxing this to expose restricted-content minithumbnails
would be an owner decision (new DEC row), not a code call.

## Verification

- `make check` — **8/8 green** (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts); provenance `.temp/acceptance/local-all`. Log: `.temp/TASK-260715-23arcu/make-check-01.log`.
- New tests: 6 integration + 15 unit, all pass. Full workspace suite green, no regressions.
- Lint: `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.

## Acceptance criteria

- *Fixtures preserve provenance and capability restrictions* — `restricted_content_is_unavailable_and_never_fetchable`, `view_once_media_is_unavailable_and_never_fetchable`, `document_preserves_original_metadata_and_locator`.
- *Multiple attachments/albums remain distinct* — `album_items_are_distinct_identities_sharing_one_album_id` (distinct `AttachmentKey`s, shared `album_id`, distinct dedup keys).
