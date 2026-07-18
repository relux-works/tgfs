# TASK-260715-23arcu — Review Verdict: ACCEPTED

Reviewer: reviewer (claude). Read-only; independently re-verified rather than trusting the implementer report.

## Acceptance criteria — both halves proven
- **Provenance + capability restrictions preserved**: restricted_content_is_unavailable_and_never_fetchable, view_once_media_is_unavailable_and_never_fetchable, document_preserves_original_metadata_and_locator.
- **Albums remain distinct**: album_items_are_distinct_identities_sharing_one_album_id — distinct AttachmentKeys, shared album_id, distinct remote_unique_id dedup keys. a_non_album_attachment_has_no_album_provenance confirms the negative.
- **Every PRD-030 kind maps** with identity + safe name: every_downloadable_kind_maps_with_identity_and_safe_name (photo/video/animation/audio/voice/video-note/sticker).

## Metadata contract — complete
Stable AttachmentKey identity (scope+chat+message+ordinal 0; never a Telegram locator, survives SYNC-045 refresh); original name verbatim + deterministic safe_name via naming::sanitize(File) with kind+MIME fallback for nameless media; MIME/size/locator (file_id+remote_id+remote_unique_id); ThumbnailDescriptor + inline Minithumbnail (base64 retained, no base64 dep — decode deferred to TASK-260715-3nl3mu); AttachmentAvailability + verbatim can_be_saved as distinct POL-4 facts; album_id provenance.

## POL-4 fail-closed — correct, not a forced fit
Previews captured ONLY for Fetchable, enforced at both extraction paths (previews() early-return for non-Fetchable; photo_attachment gates on availability==Fetchable). Restricted/ViewOnce carry no preview bytes or locators. Conservative and ToS-safe. The potential relaxation (Telegram clients show minithumbnails for protected content) is correctly parked as an owner DEC for TASK-260715-3nl3mu, not silently coded around — no stop-the-line needed.

## Architecture fit
Module in gramdrive-source-tdjson (deps {model, source} per crates/README allow-list; confirmed by the architecture gate). Documented via rustdoc per the message-module convention (no README table row needed). ThumbnailFormat degrades unknown @type to Unknown{raw_type}, matching the normalizer degrade-dont-omit rule.

## Independent verification (run by this reviewer)
- cargo test -p gramdrive-source-tdjson: 151 lib unit + 6 integration, 0 failed.
- cargo clippy -p gramdrive-source-tdjson --all-targets --all-features -- -D warnings: clean.
- cargo fmt --all --check: clean.
- make check (suite all): 8/8 green — toolchain, format, lint, test, architecture, supply-chain, traceability, scripts.

## Verdict
**done** — implementation matches AC, fits project architecture, all gates green.
