# gramdrive-render

Deterministic projections of chat history: lossless `messages.ndjson`,
human-readable Markdown, and the incremental render planner. Pure functions
of canonical records — identical input yields byte-identical output; no I/O
policy lives here.

## Ownership

STORY-260715-1oq9jg (deterministic-rendering), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-2tq5sk (NDJSON),
TASK-260715-hmmiay (Markdown), TASK-260715-22l8zy (incremental planner).

## Dependencies

Internal: `gramdrive-model`. Platform-specific code: forbidden.
See `crates/README.md`.

## Test command

```sh
cargo test -p gramdrive-render
```
