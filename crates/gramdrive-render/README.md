# gramdrive-render

Deterministic projections of chat history: lossless `messages.ndjson` and
human-readable Markdown, plus the shared `civil` calendar the incremental
render planner reuses. Pure functions of canonical records — identical input
yields byte-identical output; no I/O policy lives here.

## Ownership

STORY-260715-1oq9jg (deterministic-rendering), EPIC-260715-1poogc
(shared-rust-core). Populated by TASK-260715-2tq5sk (NDJSON),
TASK-260715-hmmiay (Markdown), and TASK-260715-22l8zy (incremental planner —
whose orchestration lives in `gramdrive-engine::render_plan`; this crate
provides the `civil` calendar and the document identities/versions it plans
against, so the planner never disagrees with a renderer about a month
boundary).

## Dependencies

Internal: `gramdrive-model`. Platform-specific code: forbidden.
See `crates/README.md`.

## `messages.ndjson` schema (v1)

Newline-delimited JSON (RFC 8259 objects, one per line, `\n`-terminated,
UTF-8). Byte-stable: field order is fixed by construction, numbers are
integers only, strings escape exactly the two mandatory characters and the C0
controls. The renderer emits it from [`ndjson::render_messages`] /
[`ndjson::write_messages`]; the input contract is the `ndjson::MessageHistory`
record set plus a `RetentionMode`.

Versioning: `schema` = `gramdrive.messages`, `schema_version` = `1`,
`renderer_version` = `1`, schema family `1`. A schema change is a version bump
with a new golden fixture (`tests/golden/`), never a mutation of v1 — a reader
keyed on `schema_version` migrates deterministically (SYNC-030).

### Header line

The first line. Self-describing provenance (DOM-006):

```json
{"type":"header","schema":"gramdrive.messages","schema_version":1,
 "renderer_version":1,"schema_family":1,"document_id":"gd…",
 "account_id":7,"namespace_version":2,"chat_id":-1001234567890,
 "partition":{"kind":"chat"},"retention_mode":"mirror",
 "input_watermark_seq":13,"content_version":"gramdrive.messages/s1/r1/w13"}
```

- `document_id` — text form of the `GeneratedDocKey`; joins the file to its item.
- `partition` — `{"kind":"chat"}`, `{"kind":"year","year":Y}`, or
  `{"kind":"month","year":Y,"month":M}`.
- `retention_mode` — `mirror` or `audit` (POL-3).
- `input_watermark_seq` — the event-log watermark the records reflect (SYNC-024).
- `content_version` — composite of schema, renderer, and watermark.

### Message line

One per emitted record. Every field is always present, `null` when
inapplicable, so every line carries the same key set in the same order:

| Field | Meaning |
|---|---|
| `type` | `"message"` |
| `message_id` | Telegram message id |
| `state` | `present` (current), `superseded` (older revision, Audit), `deleted` (tombstone, Audit) |
| `revision` | 0-based ordinal over observed revisions |
| `sender` | `{"id":i64}` or `null` (missing sender) |
| `date_ms` / `edited_ms` / `observed_ms` | send, edit, and observation times (ms) |
| `text` | message text/caption or `null` |
| `entities` | array of `{"kind":…, "offset":…, "length":…, …}` (kind-specific extra fields: `url`, `user_id`, `language`, `document_id`, `raw_kind`) |
| `reply_to_message_id` / `thread_top_message_id` / `topic_id` / `album_id` | relationships or `null` |
| `reactions` | array of `{"emoji"\|"custom_emoji_id":…, "count":…, "chosen":…}` |
| `attachments` | array (see below) |
| `service` | service action `{"action":…, …}` or `null` |
| `protected` | `true` when Telegram forbids saving the content (POL-4) |
| `deleted_ms` | deletion time on a tombstone, else `null` |
| `provenance` | `{"schema_family":…, "event_seq":…}` |

### Attachment object

```json
{"index":0,"item_id":"gd…","media_kind":"photo","name":"IMG.jpg",
 "mime_type":"image/jpeg","size":204800,"availability":"fetchable",
 "content":{"hash_algo":"sha256","hash_hex":"…"}}
```

- `item_id` — stable `AttachmentKey` identity, the link a reader resolves (SYNC-032).
- `availability` — `fetchable`, `restricted`, `unavailable`, or `view_once` (POL-4).
- `content` — `{hash_algo, hash_hex}` once downloaded, else `null` (dataless
  placeholder or never-fetched/unavailable).
- Unknown `media_kind` renders `"other"` with `media_kind_raw` preserved.

### Retention projection (POL-3)

- **Mirror** — one `present` record per live message; deleted messages omitted;
  prior revisions purged.
- **Audit** — every revision (`superseded` then `present`), or, when a deletion
  was observed, the latest revision as a content-preserving `deleted` tombstone.

## Monthly Markdown transcript (v1)

Human-readable Markdown, one document per calendar month (`YYYY/MM.md`), from
the same `MessageHistory` record set — `markdown::render_transcript` /
`markdown::write_transcript` (SYNC-031). Byte-stable: rendering is a pure
function of the records and the frozen versions, so a rerun over unchanged
input rewrites nothing. The document is blocks separated by one blank line and
closed by a single trailing newline.

Versioning: `schema` = `gramdrive.transcript`, `schema_version` = `1`,
`renderer_version` = `1`, schema family `1` (per-format lineage, independent of
the NDJSON family). A format change is a version bump with a new golden fixture,
never a mutation of v1.

### Front matter

A leading YAML block carries the same self-describing provenance the NDJSON
header does (DOM-006); every value is renderer-controlled, so none is escaped:

```yaml
---
schema: gramdrive.transcript
schema_version: 1
renderer_version: 1
schema_family: 1
document_id: gd…            # text form of the Markdown GeneratedDocKey
account_id: 7
namespace_version: 2
chat_id: -1001234567890     # the title is never part of identity (DOM-023)
partition: 2023-11          # chat | YYYY | YYYY-MM
retention_mode: mirror      # mirror | audit (POL-3)
timezone: UTC               # UTC | UTC±HH:MM[:SS] — the explicit render offset
input_watermark_seq: 13
content_version: gramdrive.transcript/s1/r1/w13
---
```

### Body

- `# Chat <id>` title and an italic subtitle (range · timezone · retention).
- `## YYYY-MM-DD` per civil day (in the header's timezone); messages fall under
  their send day, in input order.
- Per message: a bold `**HH:MM:SS · <sender> · #<id>**` header (with `· edited …`
  / `· deleted` markers), an optional italic relationship line
  (reply/thread/topic/album), the text, a protected-content note (POL-4),
  attachments, and reactions. Service messages render as an italic note.
- Audit adds a `_Deleted …_` note and an `_Earlier revisions:_` list.

### Timezone (SYNC-031)

Every date and time is computed in one caller-supplied `UtcOffset` (fixed
seconds east of UTC — no time-zone database, POL-6) and the offset is declared
in the header. Like the retention mode, it is a render configuration held
constant per account and is not part of `content_version_token`.

### Injection safety (SYNC-031)

Untrusted text, file names, titles, and reaction emoji are escaped so they
cannot alter structure: Markdown block/inline syntax is backslash-escaped, `&`
`<` `>` become HTML entities, C0 controls become U+FFFD, multi-line text is
joined with hard breaks (one inert paragraph), and attachment links are
percent-encoded.

### Attachment links (SYNC-032)

An attachment with a resolved on-disk name (`Attachment::media_name`) links to
`media/<percent-encoded name>` — the sibling media directory of the month's
year. Anything not downloaded is described with an explicit availability note
(`not downloaded yet`, `restricted by Telegram`, `unavailable`, `view-once`)
and no link. The engine supplies `media_name`; collision-resolved names are the
naming policy's, not the renderer's.

## Test command

```sh
cargo test -p gramdrive-render
# Regenerate golden fixtures intentionally, then review the diff:
UPDATE_GOLDEN=1 cargo test -p gramdrive-render
```
