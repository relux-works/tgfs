# gramdrive-source

Provider-neutral `DriveSource` contract (DEC-003): the async trait plus
paging, change-cursor, retry-classification, and cancellation semantics that
every backend must satisfy. Implementations live in separate crates
(`gramdrive-source-tdjson`, `gramdrive-source-remote` — future; fake source
in `gramdrive-testkit`), never behind feature flags here (DEC-005).

## Ownership

STORY-260715-255sa3 (drive-source-contract), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-1j4ij3 (source contract types),
validated by TASK-260715-3e8q4m (conformance suite).

## Dependencies

Internal: `gramdrive-model`. Platform-specific code: forbidden.
See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-source
```
