# TASK-260715-3s6cpe — Integrity verification and atomic promotion

Implementation notes. Status: ready for review.

## What was built

1. **`gramdrive-model::hash`** — a self-contained, streaming SHA-256 (FIPS
   180-4) producing `ContentHash::Sha256`. Pure safe Rust (the crate is
   `#![forbid(unsafe_code)]`), pinned to the FIPS 180-4 §D vectors, the NIST
   one-million-'a' vector, the exact-block-boundary case, and
   streaming-equals-one-shot properties (8 tests).

2. **`gramdrive-engine::cache`** — integrity verification and atomic
   content-addressed promotion:
   - `PromotionHost` port — the host's atomic, content-addressed, fsync-durable
     rename of a verified staging object into cache. `Materialization`
     (reference + `deduplicated`) and `PromotionHostError`.
   - `Promoter` / `PromotionConfig` — the pass that layers over
     `transfer::CompleteOutcome::Promoted`.
   - `Promotion` outcome enum: `Materialized`, `AlreadyMaterialized`,
     `IntegrityFailed`, `VersionDeparted`, `NotWholeContent`.

3. **`EngineError::Storage { detail }`** — a host cache-materialization refusal
   (disk-full etc.), distinct from a `State` failure; the quota/eviction layer
   (11abx8) owns the retry/evict policy.

## The promotion flow (and why the order is the crash-safety design)

`Promoter::promote(store, staging_host, promotion_host, transfer, now)`:

1. **Verify** — hash the whole staged object (`[0, extent)`) with SHA-256 and
   confirm every byte is readable. A short/unreadable object fails closed
   (`IntegrityFailed`); nothing is published. The digest is the blob identity.
2. **Re-check the pin** — if the item's current content version left the pinned
   one, refuse (`VersionDeparted`); bytes for version A are never observable as
   B (SYNC-042).
3. **Promote the file (file-before-row)** — the host atomically renames the
   staging object into content-addressed cache; durable before it returns.
4. **Publish (one transaction)** — re-check the pin under the write snapshot,
   then `record_blob` + `upsert_cache_entry(verified)` + (for an attachment)
   `link_attachment_blob`, folding any existing pin onto the row.

Crash safety rests on reconciliation, not a distributed transaction:
- Crash after the file promote, before the commit → `OrphanCacheObject`
  deletes the unclaimed object; content re-fetches on demand.
- Crash before the file promote → `LeakedStaging` reclaims the intact staging.
- OS/provider eviction of a materialized object → `MissingCacheObject` drops
  the cache row, never the pin (SYNC-053).

## Acceptance criteria

- **Corrupt/truncated fails closed** — the hash loop reads exactly `extent`
  bytes; a short staging object (durable ranges claim more than staged), an
  unreadable/vanished object, and a departed version each refuse to publish and
  hand the untrusted staging back for disposal.
- **Distinct attachments preserve provenance** — content-addressed dedup keeps
  one blob row and one on-disk object for identical bytes, while each
  attachment keeps its own identity, name, and version and links to the shared
  blob; `attachments_referencing_blob` returns both. Two cache entries (one per
  item) share the `materialization_ref`, so eviction of one leaves the other's
  bytes intact.
- **Crash-safe + idempotent** — file-before-row ordering + reconcile backstops;
  the content-addressed handle makes the promote idempotent (dedup no-op), and
  re-invoking `promote` after a committed promotion is a no-op
  (`AlreadyMaterialized`) that never touches the consumed staging.

## Scope decisions

- **Blob = whole content only.** A partial-range transfer streamed its bytes to
  readers and is not a blob (domain-model § Blob); promotion reports
  `NotWholeContent`. The completeness gate is `completed_ranges` covering
  `[0, extent)`.
- **Vendored SHA-256, not `sha2`.** `sha2` would add `typenum`'s build script
  and ~6 crates to a deliberately minimal, build-script-audited tree
  (`deny.toml [bans.build]`, POL-6). A fully-specified hash with published KATs
  trades that dependency for a test obligation the module discharges. Used for
  content identity/integrity of public bytes only — no constant-time claim.
- **fsync policy** documented on `PromotionHost::promote`: fsync the staging
  file, rename onto the content-addressed name (atomic within a filesystem),
  fsync the parent directory; a dedup hit needs none of it.
- **Zero-byte object** promotes to the empty-content hash with no staging read.
- **Stale `temp_ref` on the `done` row** is left inert: after the rename the
  staging handle names nothing a live transfer claims, so reconciliation does
  not flag it. No `gramdrive-state` change was needed.

## Files

- `crates/gramdrive-model/src/hash.rs` (+ `lib.rs` module reg)
- `crates/gramdrive-engine/src/cache/mod.rs`, `src/cache/promote.rs` (+ `lib.rs`)
- `crates/gramdrive-engine/src/transfer/error.rs` (`EngineError::Storage`)
- `crates/gramdrive-engine/tests/promotion.rs` (12 integration tests)
- READMEs (engine, model) + module docs updated

## Verification

- `make check` — 8/8 gates green (toolchain, format, lint, test, architecture,
  supply-chain, traceability, scripts).
- Engine: 27 lib + 12 promotion + 17 fetch + 18 transfer tests pass. Model:
  8 hash KAT tests pass. Supply-chain (`cargo deny`) unchanged — zero new deps.

## Notes for TASK-260715-11abx8 (quota/eviction)

- `EngineError::Storage` is the disk-full signal to act on (evict + retry).
- Promotion already folds pin intent onto the cache row and writes
  `verification = verified`, so eviction eligibility is correct at creation.
- Dedup shares `materialization_ref`; eviction must delete the on-disk object
  only when no cache entry still references it (reconcile's orphan rule already
  encodes this).
