# gramdrive-ffi

The FFI boundary — the only crate native consumers link. Will expose the
UniFFI API surface (async operations, records, errors, cancellation,
progress) for Swift and Kotlin; Windows/Linux hosts consume it as a plain
Rust dependency. Builds as `rlib` + `staticlib` + `cdylib`. Provider-neutral
by contract: no Telegram or OS-native types cross this boundary.

## Ownership

STORY-260715-2p879f (workspace-and-bindings), EPIC-260715-1poogc
(shared-rust-core). UniFFI wiring: TASK-260715-265gqq; artifact packaging:
TASK-260715-3akqs8.

## Dependencies

Internal: `gramdrive-engine`, `gramdrive-model` (any core crate allowed;
nothing may depend on this crate). Platform-specific code: forbidden.
See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-ffi
```
