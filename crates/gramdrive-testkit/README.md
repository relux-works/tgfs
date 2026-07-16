# gramdrive-testkit

Test support shared across the core: the deterministic fake `DriveSource`,
the source conformance suite, and shared fixture trees including
cross-platform filename fixtures (PLAT-021). Product crates may use it only
as a `dev-dependency` — it never ships in a product artifact.

## Ownership

STORY-260715-255sa3 (drive-source-contract), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-3uft8j (deterministic fake
source), TASK-260715-3e8q4m (conformance suite).

## Dependencies

Internal: `gramdrive-model`, `gramdrive-source` (`gramdrive-render` allowed
for golden-file helpers). Platform-specific code: forbidden.
See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-testkit
```
