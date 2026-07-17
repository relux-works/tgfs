# gramdrive-source

Provider-neutral `DriveSource` contract (DEC-003): the asynchronous,
dyn-compatible trait plus the item, paging, change-feed, fetch, progress,
and error types every backend must satisfy — implemented by
TASK-260715-1j4ij3. Implementations live in separate crates
(`gramdrive-source-tdjson`, `gramdrive-source-remote` — future; the
deterministic fake in `gramdrive-testkit`, TASK-260715-3uft8j), never behind
feature flags here (DEC-005). Every implementation must pass the one
conformance suite (SYNC-002, NFR-002; TASK-260715-3e8q4m).

## Ownership

STORY-260715-255sa3 (drive-source-contract), EPIC-260715-1poogc
(shared-rust-core).

## The contract at a glance

| Module | Owns |
|---|---|
| `source` | `DriveSource` trait (`scope`, `root`, `children`, `latest_cursor`, `changes`, `fetch`, `thumbnail`) and the `SourceFuture` boxed-future alias |
| `item` | `SourceItem`, the `DirectoryKind`/`FileKind` split, `FileFacts`, `ContentAvailability`, derived read-only `Capabilities` |
| `page` | `PageRequest`/`PageToken`/`ItemPage` snapshot paging; `ChangePage`/`ItemChange` feed |
| `fetch` | `FetchRequest`, `ContentChunk` delivery into a `ContentSink`, verified `FetchProgress` accounting, `ThumbnailSpec`/`Thumbnail` |
| `error` | `SourceError` failure taxonomy and derived `RetryAdvice` classification |

The durable vocabulary the contract is written in — `ItemId`,
`MetadataVersion`/`ContentVersion`, the serialized and versioned
`ChangeCursor`, `ByteRange`, `Capabilities` — lives in `gramdrive-model`
(layer 0, re-exported here as `model`), so `gramdrive-state` can persist
cursors and versions without depending on this crate.

## Contract semantics

**Enumeration is a snapshot (SYNC-003).** Every page of one enumeration
reports the same `ItemPage::snapshot` (the parent's metadata version), with
no duplicate and no missing child across pages. Page tokens are opaque,
source-minted, and *not* durable; a token the source can no longer serve
fails with `SourceError::CursorRejected`, and the caller restarts the
enumeration. Enumeration never hydrates content (SYNC-040).

**Changes advance a durable cursor (SYNC-004, SYNC-022).** `latest_cursor`
anchors a fresh baseline; `changes` returns events in source order plus the
cursor to persist transactionally with the applied state. Cursors carry
their account/namespace scope; a foreign or retired scope is rejected
explicitly with `CursorRejected` — recovery is a fresh baseline, never a
silent partial answer.

**Fetch is pinned and exact (SYNC-041, SYNC-042).** A `FetchRequest` names
item, observed `ContentVersion`, and `ByteRange`; the source delivers
exactly that range — contiguous, in order — into the caller's `ContentSink`,
whatever block sizes it uses internally. Content that changed away from the
pinned version fails with `VersionConflict`; bytes of version A are never
passed off as version B. `FetchProgress` folds chunks into verified
accounting and catches gap/overlap/overrun violations at the first bad
chunk.

**Cancellation is prompt (SYNC-043, SYNC-005, NFR-025).** Dropping a
returned future is the cancellation signal; a sink returning
`SinkControl::Stop` is the in-band equivalent for callback-style hosts.
Either way the source stops network/disk work promptly and partial state
stays resumable or safely disposable.

**Errors are the taxonomy, retries are derived (SYNC-044, NFR-033).**
Backends normalize every failure into `SourceError` — authorization, flood
wait (`RateLimited` with optional `retry_after`), restricted/protected
content, stale content reference, version conflict, rejected cursor,
transient unavailability, cancellation, not-found (source deletion),
invalid request, internal. `SourceError::retry_advice()` derives the retry
classification (`Never` / `AfterBackoff` / `AfterReauth` / `AfterRefresh` /
`AfterRebaseline`) in one exhaustive match, so a category and its retry
behavior cannot drift apart. Disk-full and integrity failures are
deliberately absent: they are local (state/engine) classes, not source
failures — the cross-layer taxonomy is TASK-260715-3b9w8x.

## UniFFI exposure

Nothing here links UniFFI; `gramdrive-ffi` owns the boundary and mirrors
what it exposes as its own records/enums/callback interfaces (the pattern
its `DriveError`/`TransferProgress` already follow). These types are kept
mechanically mappable: owned data only, integer epoch milliseconds for
times, strings for opaque tokens, `Option`/`Vec` composition, no borrowed
data in any exposed struct (`ContentChunk` is delivery-path-only), no OS or
provider types. `NonZeroU32` fields map as `u32` validated at the boundary.

## Dependencies

Internal: `gramdrive-model`. Platform-specific code: forbidden.
See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-source
```
