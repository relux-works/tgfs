# TASK-260715-3s6cpe — Review Verdict: ACCEPTED

Reviewer: reviewer (claude). Read-only review; no code modified.

## Gates
- make check: 8/8 (toolchain, format, lint, test, architecture, supply-chain, traceability, scripts)
- Tests rerun green: 12 promotion integration + 8 hash KAT + 4 cache lib

## AC verification
1. Corrupt/truncated fails closed — truncation (journal claims whole, staging short) -> read-past-written -> IntegrityFailed; unreadable/vanished staging -> IntegrityFailed; version drift -> VersionDeparted. All publish nothing and hand untrusted/intact staging back for disposal.
   - Design note: integrity = completeness(extent) + version-pin + readability, with SHA-256 as content identity. No source-provided expected digest exists (Telegram exposes none; AttachmentFacts/FileFacts carry no digest; blob_hash is the download result). Length-preserving in-transit corruption is caught below this layer (MTProto/TDLib). Correct for the domain.
2. Distinct attachments preserve provenance — content-addressed dedup: identical bytes keep one blob row + one on-disk object; each attachment keeps its own identity/name/version and links; attachments_referencing_blob returns both; two cache entries share materialization_ref.
3. Crash-safe + idempotent — file-before-row ordering; real reconcile findings OrphanCacheObject/LeakedStaging/MissingCacheObject (interrupted_promotion test converges); content-addressed promote idempotent (deduplicated no-op); AlreadyMaterialized after commit never re-reads consumed staging; host storage refusal writes no row (staging intact for retry).

## Architecture fit
- hash in gramdrive-model (layer 0, ContentHash identity). cache module in gramdrive-engine layers over transfer::CompleteOutcome::Promoted via PromotionHost port, mirroring the fetch StagingHost seam. EngineError::Storage is a clean extension. Zero new deps: vendored SHA-256 respects the deliberate build-script audit (POL-6, deny.toml [bans.build]); KATs pinned to genuine FIPS 180-4 / NIST vectors validate against ground truth. No gramdrive-state change needed — the state APIs (record_blob idempotent, link_attachment_blob, upsert_cache_entry, blob, attachments_referencing_blob, pin/set_cache_pin) pre-existed.

## Minor observations (non-blocking)
- attachment_linked:false when the projection has no attachment row is a reasonable degradation (blob still cached/servable, backreference re-linkable later), surfaced on the outcome.
- Cross-account on-disk dedup shares one object across account-scoped blob rows; eviction must delete an object only when no cache entry references it — already flagged for TASK-260715-11abx8 and encoded by reconcile orphan rule.

Verdict: accepted -> done.