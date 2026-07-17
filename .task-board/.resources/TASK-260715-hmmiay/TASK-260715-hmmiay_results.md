# TASK-260715-hmmiay — Monthly Markdown renderer — implementation notes

**Status:** ready for review. All quality gates green (`make check-core` 6/6,
`make check-repo` 2/2). 643 workspace tests pass; NDJSON golden output byte-unchanged.

## What was built

A deterministic, human-readable monthly Markdown renderer in `gramdrive-render`,
the sibling of the existing lossless NDJSON renderer. Same input contract
(`MessageHistory` record set), a different projection.

New/changed:

- `src/markdown/mod.rs` — public API: `MarkdownInput`, `UtcOffset` /
  `InvalidUtcOffset`, `render_transcript` / `write_transcript`, `document_id`,
  `content_version_token`, frozen `SCHEMA_ID`/`SCHEMA_VERSION`/`RENDERER_VERSION`/
  `MONTH_MARKDOWN_SCHEMA_FAMILY`. Re-exports the shared record types.
- `src/markdown/render.rs` — block builders: YAML front matter, title/subtitle,
  per-day grouping, per-message blocks (header, relationship line, service note,
  text, protected note, attachments, reactions, Audit deletion note + earlier
  revisions). Streams block-by-block through one reused buffer (bounded output).
- `src/markdown/text.rs` — dependency-free helpers: injection-safe escaping,
  media-link percent-encoding, and Howard-Hinnant civil-time conversion.
- Record hoist: `ndjson/record.rs` → crate-level `record.rs`, re-exported by both
  `ndjson` and `markdown`. Existing `gramdrive_render::ndjson::*` paths preserved.
- `Attachment::media_name: Option<String>` added — the resolved on-disk media
  file name the Markdown link needs. NDJSON ignores it (golden unchanged).
- Tests: `tests/markdown_unit.rs` (17), `tests/markdown_golden.rs` (3) with
  `tests/golden/corpus_mirror.md` + `corpus_audit.md`. Shared corpus in
  `tests/support/mod.rs` extended with `media_name`.
- README: added the "Monthly Markdown transcript (v1)" schema section.

## Acceptance criteria mapping

- **Golden fixtures cover specified message types** — the shared SYNC-034 corpus
  (Unicode/entities, edits, deletion, replies/threads/topics, albums, reactions,
  service messages incl. a list action, missing sender, restricted/view-once/
  unavailable/unknown media) renders to frozen Mirror and Audit goldens.
- **Unchanged inputs are byte-identical** — pure function; `render_transcript` is
  idempotent, revision input order does not change output (sorted by `event_seq`),
  streaming form equals string form, and a rerun-stability golden test guards it.
- **Links resolve to stable virtual items** — attachments with a resolved
  `media_name` link to `media/<percent-encoded name>` (the month's sibling media
  dir, per the tree layout `YYYY/{MM.md, media/}`); unavailable/undownloaded
  content is described with an explicit note and no link (SYNC-032, POL-4).

## Key design decisions

1. **Media links are caller-supplied, not renderer-derived.** Collision-resolved
   media file names come from the naming policy (`resolve_siblings`), which needs
   the whole sibling set the renderer does not hold. So `Attachment.media_name`
   is data the engine provides; the renderer only percent-encodes and links it.
2. **Timezone-explicit via a fixed `UtcOffset`** (seconds east of UTC), not an
   IANA zone — deterministic and dependency-free (no tzdata, POL-6). The header
   declares the offset; every civil date/time is computed in it. DST within a
   month is out of v1 scope (engine picks the partition's offset).
3. **Injection safety = one total, audit-able escaping rule.** `& < >` → HTML
   entities; every other CommonMark/GFM-active ASCII punctuation → backslash;
   C0 controls → U+FFFD; multi-line text joined with hard breaks so it stays one
   inert paragraph (no indented-code, no block re-interpretation); media links
   percent-encoded (also blocks `../` traversal). Verified structurally in tests.
4. **Content-version token mirrors NDJSON** (`gramdrive.transcript/s1/r1/w<wm>`):
   retention mode and timezone are account-level render config held constant per
   document, re-rendered by the engine on change — not per-document version state.
5. **Title-independence (DOM-023).** The document uses `chat_id`, never the chat
   title, so a rename never changes identity or bytes. (A future enhancement could
   pass a display title as *content*, folded into the content version.)

## Notes / follow-ups (not blockers)

- Entity-aware rich formatting (rendering bold/italic/links from `entities`) is
  deferred: it needs UTF-16 offset splicing + URL-scheme sanitization and adds
  injection surface. v1 renders safe escaped plain text; the lossless entity data
  lives in `messages.ndjson`. Not required by the task AC.
- `MONTH_MARKDOWN_SCHEMA_FAMILY = SchemaFamily(1)`: the family lineage is
  per-format (the id codec discriminates `DocFormat`), so it is distinct from the
  NDJSON family-1 document. The engine wires this into `tree::DocSchemas.month_markdown`.
