# TASK-260715-2tq5sk — Versioned NDJSON renderer — implementation notes

Status: ready for review (board: `to-review`).

## What was built

The lossless, deterministic `messages.ndjson` renderer in `gramdrive-render`
(STORY-260715-1oq9jg). New files:

- `src/json.rs` — deterministic compact JSON writer (`Json` value tree with
  ordered object fields; integers-only; RFC 8259 §7 string escaping). Follows
  the `gramdrive_model::ordering` `order.json` precedent: hand-rolled, no serde.
- `src/ndjson/record.rs` — the renderer's **input contract**: `MessageHistory`,
  `Revision`, `MessageBody`, `Entity`/`EntityKind`, `Reaction`/`ReactionKey`,
  `Attachment`/`MediaKind`/`Availability`, `ServiceAction`, `Sender`,
  `Deletion`, `RetentionMode`. Covers every domain-model *Message record* field
  with `Other` escape hatches for forward-compat losslessness.
- `src/ndjson/render.rs` — the line builders (structured records → JSON lines).
- `src/ndjson/mod.rs` — public API + frozen schema constants + docs.
- `tests/support/mod.rs` — a dependency-free JSON parser (proves output is
  *parseable* independent of the writer) and the SYNC-034 fixture corpus.
- `tests/ndjson_unit.rs`, `tests/ndjson_golden.rs` + `tests/golden/*.ndjson`.

## Key design decision (non-obvious, recorded in logbook)

`gramdrive-render` may depend on `gramdrive-model` **only** (crate layering,
enforced by `.scripts/check_crate_architecture.py`). It **cannot** read
`gramdrive-state`. And **no normalized message-record type exists upstream** —
`message_events.payload` is an opaque `Vec<u8>` the state layer never
interprets. So this task necessarily **defines** two things:

1. the versioned `messages.ndjson` **output schema** (schema `gramdrive.messages`,
   `schema_version=1`, `renderer_version=1`, schema family `1`), and
2. the renderer's **input record contract** (`ndjson::MessageHistory` …).

The renderer is a pure function. The DoD phrase "renders from state
repositories via render watermarks" is realized by the engine (a later task,
TASK-260715-22l8zy + engine wiring): it reads a chat's messages/events/
attachments up to a watermark, builds `MessageHistory` records, and calls
`render_messages` / `write_messages`. The renderer already threads the watermark
(`MessagesInput::input_watermark_seq`) into the header and content-version token
so it lines up with `gramdrive-state::WriteTxn::publish_render`.

This is a legitimate design (defining the output schema *is* this task's job),
not a forced fit. The normalizer that fills `payload` must target this schema.

## POL-3 / POL-4 semantics implemented (in the renderer)

- **Mirror**: one `present` record per live message; deleted messages omitted;
  prior revisions not shown.
- **Audit**: every revision (`superseded` … `present`); an observed deletion
  becomes a content-preserving `deleted` tombstone carrying the last-known
  content + `deleted_ms`.
- **POL-4**: attachment `availability` is explicit (`fetchable`/`restricted`/
  `unavailable`/`view_once`); `content` is `null` unless downloaded; `protected`
  flag surfaced.

## Determinism

- Field order fixed by construction (ordered `Json::Object`).
- Integers only (no f64 rounding of int64 ids, per the `order.json` rationale).
- Revisions sorted by `event_seq` (unique, watermark-safe) → revision input
  order does not change output. Message order is trusted from the caller (the
  state layer's time-windowed queries already return canonical order); the
  renderer streams one line at a time (bounded memory).
- Golden fixtures (`tests/golden/`) freeze v1; a schema change is a
  `SCHEMA_VERSION` bump + new golden, never a mutation of v1.

## Tests (all green)

- `tests/ndjson_unit.rs` (14): byte-exact header + record anchors, header schema
  freeze, full field-set coverage, Mirror vs Audit projection, content-preserving
  tombstone, missing sender, attachment availability/content gating + stable
  `item_id`, service action with list, byte-stability, revision-order
  independence, streaming == string form, `Other` raw-tag preservation, and
  control-chars-in-text never split an NDJSON record.
- `tests/ndjson_golden.rs` (3): Mirror/Audit corpus goldens + re-render stability.
- `src/json.rs` unit tests (5) + crate lib test.

## Gates

`make check` → 8/8 green: toolchain, format, lint (clippy `-D warnings`), test,
architecture, supply-chain (cargo-deny; **zero new dependencies**), traceability
(PRD-020/SYNC-030/SYNC-032 already mapped to this task), scripts.

## Not in scope here (follow-ups)

- Engine wiring state→records (bridges `message_events.payload` decode →
  `MessageHistory`); atomic publication is SYNC-033 / TASK-260715-22l8zy.
- The normalizer that produces `payload` must emit content matching this schema.
