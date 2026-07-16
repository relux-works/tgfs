# gramdrive-state

Durable local metadata: SQLite schema and migrations, repositories over
items/messages/cursors/pins, startup reconciliation. Short transactions,
multi-process safe (Apple app + extension share this database).

## Ownership

STORY-260715-16ik2x (metadata-state-store), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-1ceq7h (schema),
TASK-260715-18l9xz (migrations), TASK-260715-1opnb2 (repositories),
TASK-260715-21clwh (reconciliation).

## Dependencies

Internal: `gramdrive-model`. Platform-specific code: forbidden — the
database location is chosen by the embedding host. See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-state
```
