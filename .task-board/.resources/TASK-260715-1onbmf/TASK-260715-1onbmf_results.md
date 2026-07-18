# TASK-260715-1onbmf — TDLib download adapter (ranged reads)

Implemented the `DriveSource::fetch` side of the tdjson source: fetch intent
becomes TDLib file downloads with priority passthrough, offset/limit, progress,
cancellation, reference refresh, and local-file handoff — and the shared
conformance suite runs ranged reads through the real adapter over the mock
tdjson. `make check` 8/8 green; `make tdjson-smoke` (real linkage) green.

## What landed

### `crates/gramdrive-source-tdjson/src/download.rs` (new)

- **`DownloadMachine`** — deterministic sans-IO machine (crate convention:
  `history`/`snapshot`/`live`): pre-network gates, TDLib request protocol,
  response validation, failure classification, delivery geometry. 35 unit
  tests.
- **`TdDownloader`** — the composing driver; `fetch(request, sink)` has
  `DriveSource::fetch`'s exact signature so the future full adapter delegates
  unchanged. Owns per-file serialization, local reads, sink delivery, cancel
  guard.
- **`FetchCatalog` seam** — ItemId → `FileTarget {file_id, chat_id,
  message_id, availability, remote_unique_id, size, version}`. The state
  layer's projection supplies it at composition; the adapter consumes and
  re-verifies it.
- **`DownloadPriority`** — TDLib's 1..=32, validated passthrough into every
  `downloadFile`.

### Key decisions (details in LOGBOOK 2026-07-18 0709)

- **Protocol**: synchronous `downloadFile{offset, limit, priority,
  synchronous:true}` per fetch — the engine already grids fetches to 512 KiB
  chunks (SYNC-041), so per-range sync download is the right grain; delivery
  itself is the progress signal (SYNC-046), `progress()` mirrors accounting
  (NFR-033). `updateFile` deliberately not consumed.
- **Local-file handoff**: bytes read directly from `file.local.path` in
  bounded slices (default 256 KiB; no whole-file buffering — story AC), the
  documented TDLib streaming pattern. `readFilePart` avoided (base64 dep).
- **Temporary-file ownership**: TDLib owns its files directory; the adapter
  opens read-only, never moves/renames/deletes. Handoff to the engine is
  bytes into the sink, never a path.
- **Per-file lock**: TDLib keeps ONE download conversation per file (a second
  `downloadFile` with different offset/limit displaces a synchronous wait);
  concurrent fetches of one file serialize internally. Mock-only tests could
  never catch this — load-bearing for the real backend.
- **POL-4 first**: Restricted/ViewOnce rejected with `SourceError::Restricted`
  before any lock or network work (test pins zero requests sent).
- **Version verification** (task scope): pin checked at the gate
  (`VersionConflict{current}`) and re-resolved before every delivered slice —
  TDLib file_ids name immutable content, so verified-locator bytes can never
  be another version's; the re-check closes the catalog-level race.
- **Reference refresh** (SYNC-045/DOM-007): `FILE_REFERENCE_*` → `getMessage`
  (TDLib re-learns the reference for the same file_id) → verify the refreshed
  attachment still names the pinned content (`remote_unique_id` drift ⇒
  VersionConflict; restricted ⇒ Restricted; message gone ⇒ NotFound) →
  surface `StaleReference`; the caller's retry succeeds and identity never
  moves.
- **Cancellation** (SYNC-005/043): drop-at-await + self-waking yield per
  delivered slice; a guard fires `cancelDownloadFile{only_if_pending:false}`
  when a fetch is abandoned mid-download; sink `Stop` ⇒ `Cancelled`.
- **Failure taxonomy** (DEC-003/SYNC-044): flood wait ⇒ `RateLimited` with the
  stated delay intact; 500 ⇒ `Unavailable`; 401 ⇒ `AuthRequired`;
  ClientClosed/Shutdown ⇒ `Unavailable`; unclassified ⇒ `Internal`. No
  `TdError` crosses the boundary.

## Verification

- **Unit** (35, in `download.rs`): gates in contract order, request shapes,
  coverage validation, classification, refresh outcomes, mid-fetch
  verification, read accounting, per-file lock.
- **Integration** (`tests/file_download.rs`, 16): driver over the real runtime
  + mock — exact ranged bytes, priority/offset/limit on the wire, POL-4
  zero-request pin, all gate errors, flood/transport/auth recovery, refresh
  then retry success, mid-fetch conflict, race cadence (exactly the pinned
  version's prefix delivered), sink-stop cancel, abandon fires
  `cancelDownloadFile`, missing/truncated local file ⇒ `Unavailable`,
  concurrent same-file fetches serialize and stay intact.
- **Conformance** (`tests/fetch_conformance.rs`): the one SYNC-002 suite runs
  whole with zero skips — fetch through the real adapter (mock tdjson + real
  temp files), enumeration via the embedded testkit fake (its own conformance
  the testkit already proves; the harness name states the split). All fetch,
  fetch-failure, and cancellation cases exercise the real code path.
- **Real-link smoke** (`tests/real_tdjson_smoke.rs`, behind the
  `GRAMDRIVE_TDLIB_ARTIFACT_DIR` gate): extended with a probe that submits the
  machine's actual `downloadFile`/`getMessage`/`cancelDownloadFile` payloads
  to the real library — the real parser rejects a mistyped wire field, which
  the mock cannot. Green against the staged artifact.
- **Gates**: `make check` 8/8 (provenance `.temp/acceptance/local-all`);
  `make tdjson-smoke` green.

## Findings

- **BUG-260718-17hzcx** (STORY-260715-3qxar5): latent `sanitize()`
  idempotence violation in `gramdrive-model` naming, surfaced by a random
  proptest seed during this task's `make check`; unrelated to this diff.
  Root cause, deterministic repro seed, and fix directions recorded on the
  bug and in LOGBOOK 2026-07-18 0705. The proptest regression entry was
  deliberately not committed here to keep this diff scoped.

## Files

- `crates/gramdrive-source-tdjson/src/download.rs` (new)
- `crates/gramdrive-source-tdjson/tests/file_download.rs` (new)
- `crates/gramdrive-source-tdjson/tests/fetch_conformance.rs` (new)
- `crates/gramdrive-source-tdjson/tests/real_tdjson_smoke.rs` (probe added)
- `crates/gramdrive-source-tdjson/{Cargo.toml, src/lib.rs, README.md}`
- `crates/README.md`, `LOGBOOK.md`, `Cargo.lock`

Nothing committed (workflow: review first). Working tree left for review.
