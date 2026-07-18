# TASK-260715-3nl3mu — Review verdict: ACCEPTED

Reviewer: reviewer (claude). Read-only review; no product code modified.

## Verdict
ACCEPTED → done. Implementation matches AC, fits the crate architecture, and all
quality gates pass on an independent re-run.

## Gates (independently re-run, not merely trusted)
- `make check`: 8/8 green (toolchain, format, lint, test, architecture,
  supply-chain, traceability, scripts). Provenance: `.temp/acceptance/local-all`.
- `clippy --workspace --all-targets --all-features -D warnings`: clean.
- Tests: 32 lib unit (thumbnail machine / base64 / mime / plan / validation),
  12 integration (`tests/thumbnail_source.rs`), 2 conformance
  (`tests/fetch_conformance.rs`) — all pass.

## AC verification
- **bounded**: `ThumbnailConfig::max_preview_bytes` caps twice — a known-oversize
  preview is skipped before any request (falls back to inline / None), and a
  download whose response reports a size past the cap is refused (`Internal`). A
  mis-projected `file_id` can never become a full-media download.
- **correctly typed / versioned**: complete `ThumbnailFormat` → MIME mapping;
  undecodable `Unknown` format is not served (degrades to inline / None). No
  version parameter exists on `DriveSource::thumbnail`; staleness is handled by
  `StaleReference` → catalog re-resolution, consistent with the trait contract.
- **restriction-aware (POL-4)**: refusal at two independent layers. (1) The
  normalizer fails closed — `message.rs` `previews()` / `photo_attachment()`
  force `(thumbnail, minithumbnail) = (None, None)` for any non-fetchable
  attachment, with its own regression test. (2) `ThumbnailMachine`'s POL-4 gate
  refuses with `SourceError::Restricted` before any network. The authoritative
  conformance "every door" case (`shape.rs`) now runs the REAL adapter and
  certifies zero-request refusal.
- **never force full media download**: the download only ever carries the preview
  `file_id` (701), never the media `file_id` (700); `serving_responder` asserts
  this on every request. The byte cap is the backstop.

## POL alignment
- **POL-2** (thumbnails always eager, small): serves the dedicated TDLib
  thumbnail file (photo smallest size / video-document `thumbnail` member) or the
  inline minithumbnail (decoded in-crate, no new dependency). `inline_covers`
  serves the tiny blur directly when the requested box is already filled — a
  bounded round-trip saving.
- **POL-4** (restricted / view-once = unavailable placeholder): the source
  correctly returns `Restricted`; the unavailable placeholder is a downstream
  presentation concern (the conformance suite forbids the source answering or
  reporting missing for restricted content).

## Architecture fit
Mirrors `download.rs` (sans-IO `ThumbnailMachine` + thin `TdThumbnailer` driver).
Shared per-file serialization and cancel-on-abandon reuse `download.rs` helpers
made `pub(crate)` (`FileLocks` / `CancelGuard` / `read_exact_at` /
`classify_runtime_error` / `is_stale_reference`) — no duplication. The
`ThumbnailCatalog` seam parallels `FetchCatalog`. `thumbnail()` has the exact
`DriveSource::thumbnail` signature, ready for the full adapter to delegate to
when enumeration lands.

## Non-blocking observations
- `inline_covers` serves the blurred minithumbnail for boxes ≤ its declared dims
  (~40px Telegram cap): a minor blur-vs-sharp tradeoff at very small boxes,
  defensible under POL-2.
- No in-adapter `getMessage` refresh for previews (surfaces `StaleReference`
  instead): a documented, correct scope boundary — the reference rides on the
  owning message's refresh path.
- `decode_base64` is lenient on a mid-string `=` (breaks): acceptable for trusted
  TDLib input.

## Tests cover the required surface
photo / video / document thumbnail classes (via real `normalize_message`),
restricted + view-once refusal (zero requests), missing → None, unknown-item →
None, inline fallback, flood → RateLimited, stale-ref → StaleReference, abandon →
`cancelDownloadFile`, per-preview serialization, read-only temp-file ownership.
