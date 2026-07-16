# gramdrive-model

Domain vocabulary of the GramDrive core: item identity, the virtual
`chat -> folder -> files` tree, naming/sanitization policy, versions, change
cursors, and byte ranges. Layer 0 — every other crate depends on it; it
depends on nothing inside the workspace.

## Ownership

STORY-260715-3qxar5 (identity-and-namespace), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-1qz1g5 (stable item identities),
TASK-260715-3tjduq (virtual tree builder), TASK-260715-1ffbkg (cross-platform
naming), TASK-260715-1jmsdp (ordering projection).

## Dependencies

Internal: none. Platform-specific code: forbidden. See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-model
```
