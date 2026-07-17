# Review — TASK-260715-1j4ij3 (source contract types and errors)

**Verdict: ACCEPTED → done.**

## Independently verified

- `make check` (suite all) re-run: 8/8 green — toolchain, format, clippy -D warnings, workspace tests, architecture, cargo-deny, traceability, scripts.
- `cargo test -p gramdrive-model -p gramdrive-source` re-run: 168/168 green, matching the implementer claim.
- Cursor golden fixtures recomputed independently (Python, RFC 4648 base32 over the documented v1 byte layout): both literals in `tests/cursor_golden.rs` match byte-for-byte. The frozen format is now pinned by two independent derivations plus property tests (round-trip, injectivity, parse totality, canonicality).
- base32 extraction reviewed as a diff: 1:1 code move from `identity/codec.rs` to crate-private `base32.rs`; identity golden/property suites untouched and green — behavior-preserving.
- SYNC-044 checked against the spec sentence: all six task-specified classes covered (auth → AuthRequired, flood-wait → RateLimited{retry_after}, restricted → Restricted, stale reference → StaleReference, transient network → Unavailable, cancellation → Cancelled) plus source deletion (NotFound), version race (VersionConflict), rejected anchor (CursorRejected), InvalidRequest, Internal. Disk-full/integrity exclusion is correctly reasoned (local failures a backend cannot report), documented in error.rs + README, and routed to TASK-260715-3b9w8x.
- DEC-003 leakage: none — no Telegram/TDLib/OS types anywhere in the public API; only internal dep is gramdrive-model; architecture check enforces it.
- Layering: versions/cursors in model (layer 0) matches the crates/README.md allow list (state persists them and may depend only on model). Source re-exports model.
- Invalid states structural: ItemContent enum (directory-with-bytes unrepresentable), derived read-only capabilities (DEC-007 — no writable set constructible; restricted placeholder advertises nothing), non-empty ByteRange/ContentChunk/Thumbnail, NonZeroU32 bounds, validated tokens with explicit caps, FetchProgress rejecting gap/overlap/overrun at first bad chunk.
- Dyn-compatibility of DriveSource pinned by test through Box<dyn DriveSource> with a stub impl and no-dependency executor.
- No new dependencies; no commits made (correct per workflow).

## Non-blocking nits (fix opportunistically, no rework cycle)

1. `gramdrive-source/src/lib.rs:40` says no borrowed types in any exposed struct, but `ContentChunk` borrows its bytes. The claim holds for ffi-mirrored records (chunks cross the boundary via a copying callback), yet the sentence overreaches slightly.
2. `SourceItem.parent == None` ⟺ account root is documented, not structural. Acceptable under AC where practical; the conformance suite (TASK-260715-3e8q4m) should hunt violations.

Verdict details also in LOGBOOK.md 2026-07-17 1408.