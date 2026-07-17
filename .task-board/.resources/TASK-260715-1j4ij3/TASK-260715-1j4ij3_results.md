# TASK-260715-1j4ij3 — Source contract types and errors: implementation notes

Status: implementation ready for review. All quality gates green
(`make check`, suite `all`, 8/8 — provenance `.temp/acceptance/local-all`).

## What was built

### gramdrive-model (layer 0) — durable sync vocabulary

Placed in model, not in gramdrive-source, because `gramdrive-state` persists
versions and cursors and may depend only on layer 0 (crates/README.md
dependency allow list already assigned "versions, cursors" to model).

- `version` module — `MetadataVersion` / `ContentVersion` (DOM-003): two
  distinct opaque token types (equality-only, no `Ord` — DOM-003 allows
  monotonic *or* content-derived schemes, so cross-token ordering is
  meaningless). Validated: non-empty, ≤ 256 bytes, no control characters.
  Durable form = token text.
- `cursor` module — `ChangeCursor` (DOM-004, SYNC-004): carries the
  `AccountScope` it was minted under plus an opaque provider payload
  (≤ 4096 bytes). `require_scope` makes SYNC-004's explicit account/schema
  mismatch rejection a first-class typed operation
  (`CursorScopeMismatch`). Serialization format v1 is versioned (leading
  format byte; unknown version → `UnsupportedVersion`, distinct from
  corruption) and frozen by golden fixtures (`tests/cursor_golden.rs` —
  expected strings computed independently in Python). Text form
  `"gdc-" + base32`; property suite (`tests/cursor_properties.rs`) proves
  round-trip, injectivity, parse totality, and canonicality
  (one-spelling-per-cursor).
- `base32` module — the identity codec's strict text codec extracted to a
  crate-private shared module (parameterized prefix, neutral error type);
  identity codec now delegates and maps errors 1:1. Behavior-preservation
  proven by the untouched identity golden + property suites.

### gramdrive-source (layer 1) — the DriveSource contract

- `item` — `SourceItem` with the `ItemContent` enum
  (`Directory(DirectoryKind)` | `File(FileFacts)`): directory-with-bytes
  and file-with-children are unrepresentable. `DirectoryKind`/`FileKind`
  partition `NodeKind` with total bridges back. `FileFacts` carries
  `ContentVersion`, optional size/MIME, `ContentAvailability`
  (`Fetchable`/`Restricted` per POL-4). Capabilities are **derived**, never
  stored: read-only in v1 by construction (DEC-007/SYNC-060), and a
  restricted placeholder cannot advertise `read_content`.
- `page` — snapshot paging (SYNC-003): `ItemPage` carries the enumeration's
  snapshot (`MetadataVersion`), opaque source-minted `PageToken`
  (validated non-empty/≤1024B/no controls; explicitly NOT durable, unlike
  change cursors), `PageRequest` with `NonZeroU32` max_items. Change feed:
  `ChangePage { changes, next: ChangeCursor, more_available }`,
  `ItemChange::{Upserted(SourceItem), Removed(ItemId)}` (SYNC-022/025).
- `fetch` — `FetchRequest` pinned to `ContentVersion` + `ByteRange`
  (SYNC-041/042); `ContentChunk` (non-empty, offset+len overflow-free by
  construction) delivered into caller's `ContentSink` returning
  `SinkControl::{Continue,Stop}` (in-band cancellation, SYNC-043);
  `FetchProgress` verified accounting rejects gaps/overlaps/overruns at the
  first bad chunk (`DeliveryViolation`) — the tool the engine and the
  conformance suite share; `ThumbnailSpec`/`Thumbnail` (both
  cannot-be-empty by construction).
- `error` — `SourceError` taxonomy (11 variants) with `retry_advice()`
  derived in one exhaustive match (`RetryAdvice::{Never, AfterBackoff,
  AfterReauth, AfterRefresh, AfterRebaseline}`) so category and retry class
  cannot drift. Coverage of the specified failure classes:
  - auth → `AuthRequired` (retry after reauth)
  - flood-wait/backoff → `RateLimited { retry_after }` (backoff, honors minimum)
  - restricted content → `Restricted` (never retry; POL-4)
  - unavailable/expired reference → `StaleReference` (refresh then retry; DOM-007/SYNC-045)
  - transient network → `Unavailable` (backoff)
  - cancellation → `Cancelled`
  - plus SYNC-044's source deletion → `NotFound`, version race →
    `VersionConflict` (refresh), rejected cursor/page anchor →
    `CursorRejected` (re-baseline; SYNC-004), `InvalidRequest`, `Internal`.
- `source` — the `DriveSource` trait: `scope`, `root`, `children`,
  `latest_cursor`, `changes`, `fetch`, `thumbnail`, all returning
  `SourceFuture<'_, T>` (boxed Send futures) so the trait stays
  **dyn-compatible** for runtime source selection (local TDLib vs remote,
  per architecture.md) with zero new dependencies. Cancellation = dropping
  the future (documented, SYNC-005/NFR-025) or `SinkControl::Stop`.
  Dyn-compatibility and the full call flow are exercised in tests through
  `Box<dyn DriveSource>` with a stub impl and a no-dependency noop-waker
  executor.

## Deliberate scope decisions

1. **Disk-full and integrity failures are absent from `SourceError`** —
   SYNC-044 names them as retry classes, but they are local (state/engine)
   failures, not something a backend reports. Documented in error.rs and
   README; the cross-layer taxonomy is TASK-260715-3b9w8x (which this task
   blocks).
2. **No uniffi dependency here.** gramdrive-ffi owns the boundary and
   mirrors exposed types as its own records/enums/callbacks (its existing
   DriveError/TransferProgress pattern). All contract types are kept
   mechanically mappable (owned data, epoch-ms integers, string tokens;
   NonZeroU32 → u32 validated at the boundary). Documented in lib.rs +
   README.
3. **PageToken is not versioned** — it is snapshot-scoped and never
   durable; only `ChangeCursor` persists, and it is versioned + golden-
   frozen.
4. **`ItemChange` keeps `Upserted(SourceItem)` unboxed**
   (allow(large_enum_variant) with rationale): upserts dominate real
   feeds; boxing would add an allocation to nearly every element to shrink
   the rare `Removed`.

## Verification

- `cargo test -p gramdrive-model -p gramdrive-source` — 168 passed, 0 failed
  (25 new in gramdrive-source; new model tests: version, cursor unit +
  golden + property suites; all pre-existing identity/tree/naming/ordering
  suites untouched and green, proving the base32 extraction is
  behavior-preserving).
- `make check` (suite `all`): toolchain, format, lint (clippy -D warnings),
  test (workspace), architecture, supply-chain (cargo deny), traceability,
  scripts — 8/8 ok.
- No new dependencies; no cargo features introduced; dependency direction
  unchanged (source → model only).

## Follow-ups for dependent tasks

- TASK-260715-3uft8j (deterministic fake source): implement `DriveSource`
  in gramdrive-testkit; the stub in `source.rs` tests shows the minimal
  shape.
- TASK-260715-3e8q4m (conformance suite): `FetchProgress`/`DeliveryViolation`
  and `ItemPage.snapshot` equality are the intended assertion tools for
  SYNC-003/042/046 cases.
- TASK-260715-3b9w8x (error taxonomy): map `SourceError` + `RetryAdvice`
  onto the ffi `DriveError` categories; `Restricted`, `StaleReference`,
  `VersionConflict`, `CursorRejected` currently have no ffi counterpart.
