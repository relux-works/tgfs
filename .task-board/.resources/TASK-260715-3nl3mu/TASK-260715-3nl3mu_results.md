# TASK-260715-3nl3mu — Thumbnail and preview source

Status: ready for review (board: to-review)

## What was built

The `DriveSource::thumbnail` side of the local TDLib source, in a new module
`crates/gramdrive-source-tdjson/src/thumbnail.rs`, following the crate's
sans-IO-machine + thin-driver convention (mirrors `download.rs`):

- **`ThumbnailMachine`** (sans-IO) — every decision, no I/O: the POL-4 gate,
  preview selection, in-crate base64 decode of the inline blur, the download
  request, response validation, and the byte cap.
- **`TdThumbnailer`** (driver) — resolves the item through a
  `ThumbnailCatalog`, submits the machine's request on a `TdClient`, reads
  TDLib's local preview file read-only, returns the finished `Thumbnail`.
  `thumbnail()` has the trait method's exact signature so the future full
  `DriveSource` adapter delegates to it unchanged.
- **`ThumbnailCatalog`** seam — `ItemId → Option<ThumbnailTarget>`.
  `ThumbnailTarget::from_descriptor` is the single projection from a
  normalized `AttachmentDescriptor` (the seam to the metadata store / the
  attachment-mapping task, 23arcu).

## Acceptance criteria — how each is met

| AC clause | How |
|---|---|
| **eager small thumbnails per POL-2, via TDLib thumbnail files** | Downloads the preview `file_id` (photo's smallest stored size, or a video/document `thumbnail` member) whole-file synchronously; inline minithumbnail as a zero-network fallback. |
| **distinct from full-content hydration** | The preview `file_id` is a different file than the media `file_id`. Integration test asserts the download carries the preview id (701), never the media id (700). |
| **restriction-aware (POL-4)** | Restricted / view-once → `SourceError::Restricted`, before any request (zero network — the mock has no responder in that test, so any call would panic). Certified through the shared conformance suite's POL-4 "every door" case, now routed through the real adapter. |
| **bounded** | `ThumbnailConfig::max_preview_bytes` (default 4 MiB) caps twice — a known-oversize preview is skipped pre-request (falls back to inline/None); a download response past the cap is refused. A mis-projected `file_id` can never become a full-media download. |
| **correctly typed** | `ThumbnailFormat → MIME` mapping (jpeg/png/webp/gif/mp4/webm/tgs); an undecodable `Unknown` format is not served (falls back). Inline blur is `image/jpeg`. |
| **never force full media download** | Only the preview file is ever downloaded; the byte cap is the backstop. |

## Deliberate decisions

- **No in-adapter `getMessage` refresh for thumbnails.** A thumbnail is a
  secondary, best-effort preview; a stale preview reference surfaces as
  `StaleReference` (retryable-after-refresh) and the caller re-resolves via
  the catalog once the owning message's references are re-learned by the
  ranged-fetch / live-update path. Keeps the refresh protocol in one place
  (`download.rs`). Not required by the AC or the required test surface.
- **In-crate base64 decoder** (`decode_base64`, RFC 4648, padded + unpadded)
  so serving the inline blur adds no dependency — "free of a base64
  dependency" honored as no external crate.
- **Shared helpers** made `pub(crate)` in `download.rs`
  (`FileLocks`, `CancelGuard`, `read_exact_at`, `classify_runtime_error`,
  `is_stale_reference`) and reused — identical per-file serialization and
  cancel-on-abandon across both adapters, no duplication.

## Tests

- **Unit** (`src/thumbnail.rs`): base64 round-trip/reject, MIME mapping,
  preview selection (restricted/view-once, downloadable, unknown-format
  fallback, oversize skip, inline-covers, large-box prefers download, inline
  fallback, no-preview), download-response validation
  (covering/empty→None/non-covering/past-cap/wrong-file/no-path), read
  accounting (short read, read error), error classification (stale-ref,
  flood), `from_descriptor` projection, cancel-request shape.
- **Integration** (`tests/thumbnail_source.rs`, 12 cases): photo / video /
  document classes (via real `normalize_message`), restricted, view-once,
  missing → None, unknown-item → None, inline fallback, flood → RateLimited,
  stale-ref → StaleReference, abandon → `cancelDownloadFile`, per-preview
  serialization, read-only temp-file ownership.
- **Conformance** (`tests/fetch_conformance.rs`): the POL-4 "every door"
  thumbnail clause now runs the real adapter; suite passes whole, zero skips.

## Validation

`make check` — 8/8 green (toolchain, format, lint, test, architecture,
supply-chain, traceability, scripts). Provenance: `.temp/acceptance/local-all`.

## Files

- `crates/gramdrive-source-tdjson/src/thumbnail.rs` (new)
- `crates/gramdrive-source-tdjson/src/lib.rs` (module + exports + docs)
- `crates/gramdrive-source-tdjson/src/download.rs` (`pub(crate)` shared helpers)
- `crates/gramdrive-source-tdjson/tests/thumbnail_source.rs` (new)
- `crates/gramdrive-source-tdjson/tests/fetch_conformance.rs` (real thumbnail routing)
- `crates/gramdrive-source-tdjson/README.md`
