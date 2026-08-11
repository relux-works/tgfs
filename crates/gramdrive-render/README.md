# gramdrive-render

Deterministic projections of chat metadata and history: privacy-bounded
hidden `.chat.json`, lossless monthly `Messages.ndjson`, and human-readable Markdown,
plus the shared `civil` calendar the incremental render planner reuses. Pure
functions of canonical records — identical input yields byte-identical output;
no I/O policy lives here.

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

## `.chat.json` schema (v1)

Compact RFC 8259 JSON with a trailing newline, rendered by
`chat_json::render`. Versioning is `schema = gramdrive.chat`,
`schema_version = 1`, `renderer_version = 1`, schema family `1`. The content
version hashes the exact bytes, so arbitrary titles or usernames never enter a
version token while every byte-shaping metadata change moves the pin.

```json
{"schema":"gramdrive.chat","schema_version":1,"renderer_version":1,"chat":{"type":"supergroup","title":"Example","username":"public_name","is_protected":false,"archive_mode":false,"left_at_ms":null,"deleted_at_ms":null,"last_update_at_ms":1784116800000}}
```

The fixed `chat` fields are type, current title, optional public username,
protected-content state, per-chat Archive Mode state, left/deleted observation
times, and last metadata update time. The renderer has no inputs for account or
namespace identity, Telegram chat ids, authorization state, secret references,
local paths, or message content; those values therefore cannot appear in this
document.

## `Messages.ndjson` schema (v2)

Newline-delimited JSON (RFC 8259 objects, one per line, `\n`-terminated,
UTF-8). Byte-stable: field order is fixed by construction, numbers are
integers only, strings escape exactly the two mandatory characters and the C0
controls. The renderer emits it from [`ndjson::render_messages`] /
[`ndjson::write_messages`]; the input contract is the `ndjson::MessageHistory`
record set plus a `RetentionMode`.

Versioning: `schema` = `gramdrive.messages`, `schema_version` = `3`,
`renderer_version` = `4`, schema family `1`. V3 adds the orthogonal attachment
representation/fidelity/source-name/exact-size contract; renderer v4 makes a
protected record a body-free placeholder. A schema change is a version bump
with a new golden fixture (`tests/golden/`), never a mutation of v1 — a reader
keyed on `schema_version` migrates deterministically (SYNC-030).

### Header line

The first line. Self-describing provenance (DOM-006):

```json
{"type":"header","schema":"gramdrive.messages","schema_version":3,
 "renderer_version":4,"schema_family":1,"document_id":"gd…",
 "account_id":7,"namespace_version":2,"chat_id":-1001234567890,
 "partition":{"kind":"chat"},"retention_mode":"mirror",
 "display_timezone":"UTC","input_watermark_seq":13,"render_generation":0,
 "content_version":"gramdrive.messages/s3/r4/w13/g0/retention-mirror/tz-UTC"}
```

- `document_id` — text form of the `GeneratedDocKey`; joins the file to its item.
- `partition` — `{"kind":"chat"}`, `{"kind":"year","year":Y}`, or
  `{"kind":"month","year":Y,"month":M}`.
- `retention_mode` — `mirror` or `audit` (POL-3).
- `display_timezone` — persisted civil-partition policy; timestamps stay UTC.
- `input_watermark_seq` — the event-log watermark the records reflect (SYNC-024).
- `render_generation` — monotonic byte-shaping account-policy generation.
- `content_version` — composite of schema, renderer, message watermark,
  render generation, retention mode, and display timezone.

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
| `protected` | `true` when Telegram forbids saving the content (POL-4); every body field is `null` or empty and Audit emits no earlier revisions |
| `deleted_ms` | deletion time on a tombstone, else `null` |
| `provenance` | `{"schema_family":…, "event_seq":…}` |

### Attachment object

```json
{"index":0,"item_id":"gd…","media_kind":"photo",
 "telegram_representation":"original_document","fidelity":"original",
 "source_name":"IMG.jpg","mime_type":"image/jpeg","exact_size":204800,
 "availability":"fetchable",
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

## Monthly Markdown transcript (v2)

Human-readable Markdown, one `Messages.md` per direct `YYYY-MM/` folder, from
the same `MessageHistory` record set — `markdown::render_transcript` /
`markdown::write_transcript` (SYNC-031). Byte-stable: rendering is a pure
function of the records and the frozen versions, so a rerun over unchanged
input rewrites nothing. The document is blocks separated by one blank line and
closed by a single trailing newline.

Versioning: `schema` = `gramdrive.transcript`, `schema_version` = `2`,
`renderer_version` = `4`, schema family `1` (per-format lineage, independent of
the NDJSON family). A format change is a version bump with a new golden fixture,
never a mutation of v1.

### Front matter

A leading YAML block carries the same self-describing provenance the NDJSON
header does (DOM-006); every value is renderer-controlled, so none is escaped:

```yaml
---
schema: gramdrive.transcript
schema_version: 2
renderer_version: 4
schema_family: 1
document_id: gd…            # text form of the Markdown GeneratedDocKey
account_id: 7
namespace_version: 2
chat_id: -1001234567890     # the title is never part of identity (DOM-023)
partition: 2023-11          # chat | YYYY | YYYY-MM
retention_mode: mirror      # mirror | audit (POL-3)
timezone: UTC               # UTC | UTC±HH:MM[:SS] — the explicit render offset
input_watermark_seq: 13
render_generation: 0
content_version: gramdrive.transcript/s2/r4/w13/g0/retention-mirror/tz-UTC
---
```

### Body

- `# Chat <id>` title and an italic subtitle (range · timezone · retention).
- `## YYYY-MM-DD` per civil day (in the header's timezone); messages fall under
  their send day, in input order.
- Per message: a bold `**HH:MM:SS · <sender> · #<id>**` header (with `· edited …`
  / `· deleted` markers), an optional italic relationship line
  (reply/thread/topic/album), the text, attachments, and reactions. A protected
  message emits only a non-content POL-4 placeholder after its header; no
  relationship, text/caption, entity, reaction, attachment, service payload,
  or Audit revision is rendered. Service messages render as an italic note.
- Audit adds a `_Deleted …_` note and an `_Earlier revisions:_` list.

### Timezone (SYNC-031)

Every date and time is computed in the persisted account `DisplayTimeZone` and
its IANA name is declared in the header. Bundled transition rules make civil
month boundaries repeatable across hosts without consulting the host locale;
fixed-offset zones remain available for fixtures. Timezone, retention mode,
and the monotonic policy generation are part of `content_version_token`, so a
policy-only render never reuses a message-watermark version for different
bytes.

### Injection safety (SYNC-031)

Untrusted text, file names, titles, and reaction emoji are escaped so they
cannot alter structure: Markdown block/inline syntax is backslash-escaped, `&`
`<` `>` become HTML entities, C0 controls become U+FFFD, multi-line text is
joined with hard breaks (one inert paragraph), and attachment links are
percent-encoded.

### Attachment links (SYNC-032)

An attachment with a resolved on-disk name (`Attachment::media_name`) links
directly to `<percent-encoded name>` in the same month folder. Anything not downloaded is described with an explicit availability note
(`not downloaded yet`, `restricted by Telegram`, `unavailable`, `view-once`)
and no link. The engine supplies `media_name`; collision-resolved names are the
naming policy's, not the renderer's.

## Test command

```sh
cargo test -p gramdrive-render
# Regenerate golden fixtures intentionally, then review the diff:
UPDATE_GOLDEN=1 cargo test -p gramdrive-render
```
