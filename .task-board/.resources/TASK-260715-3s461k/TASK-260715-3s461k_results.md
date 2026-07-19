# TASK-260715-3s461k — Offline pin & eviction reconciliation (macOS surface)

**Status:** ready for review
**Requirement owned:** SYNC-053 (macOS reconciliation surface). Core cache/pin/quota
engine (SYNC-050/051/052/054) is TASK-260715-11abx8, already done.

## What the task is, precisely

Per `docs/TRACEABILITY.md`, this task owns only the **macOS reconciliation surface**
of SYNC-053. The durable pin/quota/eviction accounting and the crash-safe
`StateStore::reconcile` live in the core (11abx8/3s6cpe, done). DEC-006 keeps the
File Provider extension thin and read-only: it never writes shared state. So the
provider-side deliverable is to **project durable core pin state onto the system's
content policy**, which is how a replicated File Provider expresses "keep available
offline" vs "evictable placeholder" (POL-2).

## Changes

### 1. FFI read surface — expose durable pin state (`crates/gramdrive-ffi`)
- `ItemMetadata` gains `pin: Option<PinOrigin>` and a new `PinOrigin { User, ArchiveMode }`
  enum (mirrors `gramdrive_state::repo::PinOrigin`).
- The pin is folded from `ReadTxn::pin` in every provider read path — `item`,
  `children`, `child_by_name`, `item_changes_since` — as a same-snapshot indexed
  point lookup, so the pin and the metadata can never disagree across a concurrent
  commit.
- The field is `#[uniffi(default = None)]`, so adding it is an additive change for
  foreign construction sites. `CONTRACT_VERSION` bumped `0.4.0 → 0.5.0` (additive/minor).

### 2. Provider mapping — content policy (`apple/.../FileProviderItem.swift`)
`GramDriveFileProviderItem.contentPolicy` maps core pin → `NSFileProviderContentPolicy`
(macOS 13+, fully available on the macOS 14 support floor — POL-5):

| Item state | Policy | Why |
|---|---|---|
| Pinned (user/archive) fetchable file, or pinned directory | `.downloadEagerlyAndKeepDownloaded` | POL-2 available-offline: eager, quota-exempt, never evicted (SYNC-051). A pinned dir propagates to inheriting children → Archive-Mode subtree coverage. |
| Unpinned fetchable file | `.downloadLazily` | POL-2 dataless-placeholder default; hydrate on open, evict under disk pressure (SYNC-052 dehydrate on quota pressure). |
| Unpinned directory | `.inherited` | The (lazy) root default flows down; an eager ancestor still reaches unpinned descendants. |
| Restricted / unavailable (POL-4), even if pinned | falls through to lazy/inherited | Bytes are never fetched, so eager would ask the system to retry an impossible fetch. |

The mapping is a pure, total function of durable `ItemMetadata` — the same shape as
the rest of the item projection — so it is exercised entirely from fixtures.

## How the acceptance criteria are met

- **Pinned intent is durable.** The pin lives in the durable `pins` table; the
  provider re-derives content policy from it on every read. A restart rebuilds items
  from the same durable metadata and lands the same eager policy. (Rust reopen test +
  Swift re-derivation test.)
- **Eligible content evicts only per policy.** Pinned → eager (eviction-proof);
  unpinned fetchable → lazy (evictable on pressure). A pin flips a file from evictable
  to kept.
- **Reported state matches Finder/system state.** Pinned items report kept-downloaded
  and unpinned report the evictable placeholder — the states Finder renders.

## Tests

- **Rust (`crates/gramdrive-ffi/src/shared_state.rs`):**
  - `durable_pins_surface_on_every_provider_read_and_survive_a_reopen` — both origins
    surface through item/children/child_by_name/item_changes_since; a **fresh handle
    after the pin write** (the "after restart" read) still sees them; unpinned → `None`.
  - `unpinning_drops_the_projection_back_to_the_evictable_default`.
- **Swift (`.../FileProviderItemTests.swift`, suite "content policy (pinning)"):**
  pinned→eager (both origins), unpinned file→lazy, pinned dir→eager, unpinned
  dir→inherited, restricted/unavailable never eager even if pinned, pin flips
  evictability, pure re-derivation stability, and an exhaustive kind×availability×pin
  policy-in-range sweep.

## Finding (cross-boundary, logged)

`gramdrive-engine/src/cache/pin.rs` `pin`/`unpin` write only the `pins` table (folding
the cache row); they do **not** journal an item change or bump the item's
`metadata_version`. So a pin/unpin does not appear in the working-set change feed, and
the system re-reads the new `contentPolicy` only on a restart / full re-enumeration,
not live. Making pin changes journal a metadata-version bump is an engine/coordinator
write concern outside the read-only provider scope (DEC-006). Flagged for the
engine/coordinator owner; recorded in the logbook.

## Commands run

- `cargo test -p gramdrive-ffi` — green (29 tests, incl. the two pin tests).
- `cargo fmt --all -- --check`, `cargo clippy -p gramdrive-ffi --all-targets` — clean.
- `make package` — regenerates the GramDriveCore bindings the Swift package resolves against.
- `swift test` (apple/GramDriveSupport) — GramDriveFileProviderTests green.
- `make check` — full gate suite.
