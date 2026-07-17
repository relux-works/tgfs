# gramdrive-engine

Transfer and cache engine — the orchestration layer: hydration, pin/offline
state, resumable ranged downloads, integrity and cache promotion, quota
accounting, LRU eviction of unpinned content (POL-2). Drives any
`DriveSource` through the contract; persists durable transfer state via
`gramdrive-state`.

## Modules

- `transfer` — the durable transfer state machine (TASK-260715-g4k3zm;
  SYNC-040..046). `TransferMachine` is stateless policy over the journal
  `gramdrive-state` persists: request/coalesce with a version pin, claims
  with a resume plan computed from persisted ranges, monotonic progress
  under one staging handle, a promotion gate that never publishes
  incomplete or stale content, deterministic version-race invalidation,
  a bounded retry budget with parking for precondition-blocked faults,
  and two-phase cancellation. Crash-resume composes with
  `StateStore::reconcile`: reconcile first, then claim.

## Ownership

STORY-260715-2hs8cf (transfer-and-cache-engine), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-22fh09 (ranged fetch
coordinator), TASK-260715-g4k3zm (durable transfer state), TASK-260715-3s6cpe
(integrity/promotion), TASK-260715-11abx8 (quota/eviction).

## Dependencies

Internal: `gramdrive-model`, `gramdrive-source`, `gramdrive-state`
(`gramdrive-render` allowed, not yet used). Platform-specific code:
forbidden. See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-engine
```
