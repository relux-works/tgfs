# TASK-260715-hmmiay — Review verdict: ACCEPTED

Reviewer: [reviewer] reviewer (claude). Read-only review of the monthly Markdown
renderer landed in `gramdrive-render`.

## Verdict

**ACCEPTED → done.** Implementation matches the AC, fits the project
architecture, and every quality gate is green. No changes requested.

## What I checked

Read every source and test in the change set:
`src/markdown/{mod,render,text}.rs`, `src/record.rs` (hoisted), `src/lib.rs`,
`src/ndjson/{mod,render}.rs`, `tests/{markdown_unit,markdown_golden,ndjson_unit}.rs`,
`tests/support/mod.rs`, both golden fixtures, the README section, and the
referenced spec (POL-3/4/6, SYNC-012/013/031/032/034, DOM-006/023, the SYNC-010
virtual tree).

### Acceptance criteria

- **Golden fixtures cover specified message types** — PASS. The SYNC-034 corpus
  exercises Unicode/entities, out-of-seq edits, deletion, reply/thread/topic,
  albums, reactions, service messages (incl. a list action), missing sender, and
  restricted/view-once/unavailable/unknown media. Frozen in `corpus_mirror.md`
  and `corpus_audit.md`.
- **Unchanged inputs are byte-identical** — PASS. `render_transcript` is a pure
  function of the records + frozen versions; revisions are sorted by `event_seq`
  so shuffled input renders identically (tested); the streaming form equals the
  string form (tested); a rerun-stability golden guards against a
  nondeterministic capture.
- **Links resolve to stable virtual items** — PASS. Attachments with a resolved
  `media_name` link to `media/<percent-encoded name>`. Verified against the
  SYNC-010 tree (`YYYY/{MM.md, media/}`): `media/` is a real sibling of the month
  file, so the relative link resolves. Unavailable/undownloaded content gets an
  explicit availability note and no link (SYNC-032, POL-4). Collision-resolved
  names are engine-supplied, not renderer-derived — correct, since suffixing
  needs the full sibling set the renderer does not hold.

### Injection safety (SYNC-031)

Traced every untrusted value to an escaper: message text (`escape_paragraph`),
earlier-revision text (`escape_flattened`), attachment name / reaction emoji /
service titles / raw `Other` kind tags (`escape_inline`), and `media_name`
(`percent_encode_component`). All numeric/renderer-controlled values (ids,
counts, timestamps, partition, offset label) are safe unescaped. The
`& < >`→entity + backslash-escape + C0→U+FFFD + hard-break-join rule keeps
untrusted multi-line text one inert paragraph; encoding `/` also blocks `../`
traversal. The `untrusted_text_cannot_break_structure` and
`indented_and_multiline_text_stays_one_paragraph` tests prove it structurally.
Entity-aware rich formatting is deferred with a sound rationale (UTF-16 splicing
+ URL-scheme sanitization = added injection surface; not in AC).

### Architecture fit

- Record contract hoisted to crate-level `record.rs`, re-exported by both
  renderers; existing `ndjson::*` paths preserved and NDJSON goldens
  byte-unchanged (confirmed by the passing NDJSON golden tests).
- `Attachment::media_name` added; NDJSON ignores it (links by opaque identity).
- Timezone-explicit via a fixed `UtcOffset` + hand-rolled Howard-Hinnant civil
  conversion — deterministic, dependency-free (POL-6). Zero new deps.
- Title-independent identity via `chat_id` (DOM-023); content-version token
  mirrors NDJSON and correctly excludes account-level render config (retention
  mode, timezone).
- Bounded output: blocks stream through one reused buffer; memory bounded by the
  largest single block, not the month.

## Gate results (re-run by reviewer)

- `cargo test -p gramdrive-render` — all suites green (16 lib + 17 markdown_unit
  + 3 markdown_golden + 14 ndjson_unit + 3 ndjson_golden).
- `cargo clippy -p gramdrive-render --all-targets` — clean.
- `make check-core` — 6/6 (toolchain, format, lint, test 11.5s, architecture,
  supply-chain).
- `make check-repo` — 2/2 (traceability, scripts).

## Notes

No defects found. Minor cosmetic observations (a source string ending in a
newline yields a trailing literal `\` on the last visual line; a bare URL in
untrusted text may GFM-autolink) are benign — neither breaks structure,
determinism, nor safety, and neither is in scope. Not blocking.
