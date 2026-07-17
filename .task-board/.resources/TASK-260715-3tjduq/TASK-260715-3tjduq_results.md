# TASK-260715-3tjduq — Virtual tree builder: implementation notes

Status: implementation ready for review. `make check` 8/8 green; 46 tests in
`gramdrive-model` (10 unit + 8 golden + 13 identity property + 12 tree
fixture + 3 tree property). Nothing committed — working tree handed off.

## What was built

### 1. `gramdrive_model::tree` — the virtual tree builder (new module)

`crates/gramdrive-model/src/tree.rs`. `TreeProjection` projects normalized
source records (`AccountRecord`, `FolderRecord`, `ChatRecord`,
`AttachmentRecord`, `DocSchemas`) into the default layout of
`.spec/sync-and-filesystem-semantics.md`:

```text
Account/
  Main/
    Chat/
      chat.json
      messages.ndjson
      2026/
        07.md
        media/
          <attachment files>
  Archive/
  Telegram Folders/
    <one directory per custom folder>
```

API surface:

- `TreeProjection::new(account, folders, chats, schemas) -> Result<_, TreeInputError>`
  — validates the input contract (duplicate folder/chat/attachment
  identities, dangling folder memberships, months outside 1–12) and indexes
  everything into BTree structures keyed by stable identity.
- `root()` / `root_id()` — the account root node.
- `node(&ItemId) -> Option<TreeNode>` — resolves any identity to its tree
  position; returns `None` for record-not-position keys (canonical chats,
  messages, blobs), non-member appearances, foreign scopes.
- `children(&ItemId, after: Option<&ItemId>, limit: NonZeroUsize) ->
  Result<ChildPage, ChildrenError>` — lazy paged enumeration. The page
  boundary is the last returned child's `ItemId` (opaque, serializable in
  both provider forms). Boundaries are snapshot-scoped; a foreign boundary
  fails with `ForeignPageBoundary` instead of silently skipping/repeating
  (SYNC-003).
- `TreeNode { id, parent, kind, display_name, canonical, capabilities,
  size, content }` — `canonical` is the shared record reference (PRD-013),
  `content` the shared `BlobKey` for materialized attachments.
- `Capabilities` — read-only v1 (DEC-007/SYNC-060): both constructors
  (`read_only_directory`, `read_only_file`) pin every write-side field
  `false`; SYNC-063 owns any future change.

Key properties:

- **One record, many appearances.** Chats are stored once
  (`BTreeMap<i64, ChatState>`); views hold only chat-ID references. Every
  node below a view root is an appearance key (view × canonical), so Main
  and a folder show the same chat as two `ItemId`s over one record; blobs
  dedupe through `BlobKey` by content hash.
- **Laziness.** Construction is O(input records); appearance nodes are
  minted per requested page only — nothing is materialized per (view ×
  item) eagerly.
- **Determinism.** Sibling order is identity order per parent kind: fixed
  roots (Main, Archive, Telegram Folders), folder IDs, chat IDs, years,
  months, then (message ID, attachment ordinal). Input order can never
  influence output (all state BTree-keyed).
- POL-1 chat names: `<Display Name> — @<username>` (em dash), title alone
  without username. Names are raw; sanitization/collision suffixing is
  TASK-260715-1ffbkg, order metadata is TASK-260715-1jmsdp.

### 2. Identity vocabulary extensions (additive to frozen format v1)

The identity codec had explicitly reserved room: "the virtual tree builder
will add directory kinds" (codec.rs tag-table comment). Added:

| Addition | Tag | Purpose |
|---|---|---|
| `FolderCatalogKey { scope }` | kind `0x08` | the "Telegram Folders" grouping directory |
| `YearDirKey { chat, year }` | kind `0x09` | `Chat/2026/` |
| `MediaDirKey { chat, year }` | kind `0x0a` | `Chat/2026/media/` |
| `DocFormat::Json` | format `0x03` | `chat.json` (chat metadata generated doc) |

Purely additive: no pre-existing encoding changed; all original golden
fixtures pass untouched. New golden fixtures pin the new encodings (bytes
hand-verified against the format spec, text mechanically derived). The
property generators now sample the new variants, so round-trip/injectivity/
prefix-freedom proofs cover them. README format table updated in the same
change (doc+code same commit rule).

Also fixed opportunistically (logbook 2026-07-17 0655 finding): the wrong
"largest v1 key is 40 bytes" comment in codec.rs — corrected to blob
appearance at 49 bytes, `with_capacity(64)` aligned with the tested bound.

## Design decisions (for reviewer attention)

1. **`(view, item)` discipline** — which combinations resolve: canonical
   positions are the account root, Main/Archive/folder list roots, and the
   folder catalog; appearance positions are chats and their subtrees
   (year/media dirs, generated docs, attachments). Accounts, chat lists,
   the catalog, messages, and blobs never appear wrapped in a view. This is
   the discipline the identity module docs explicitly deferred to this task.
2. **`chat.json` is a `GeneratedDoc`** with partition `Chat`, new format
   `Json` — matches the domain model ("NDJSON/Markdown/chat metadata view",
   `.spec/domain-model.md` Generated document) without a new key kind.
3. **`media/` is per-year** (sibling of `MM.md` inside `YYYY/`), exactly as
   the spec layout example shows. It exists iff the year has attachments;
   month partitions and media are independent (a year can have either).
4. **Page boundaries are exact-match, snapshot-scoped.** SYNC-003 requires
   repeatability for a declared snapshot; an immutable projection is that
   snapshot. Cross-snapshot resume degrades to an explicit
   `ForeignPageBoundary` error, prompting re-enumeration — no silent gap.
5. **Records carry bare IDs, not keys** — the projection derives every key
   from its own `AccountScope`, so cross-account record mixups are
   unrepresentable rather than validated.
6. **Schema families are inputs** (`DocSchemas`): family assignment belongs
   to the rendering layer (DOM-023); the tree only stamps identities.

## Acceptance criteria → evidence

| AC | Evidence |
|---|---|
| Fixture tree matches specification | `tests/tree_fixture.rs::fixture_tree_matches_spec_layout` pins the spec example listing literally; `empty_account_has_fixed_roots`, `media_and_month_partitions_are_independent` pin the edge layouts |
| Canonical records/blobs not duplicated | `multiple_appearances_share_canonical_records_and_blobs`: two appearances, distinct ids, equal canonical keys, mirrored subtrees, one shared blob identity; structurally, views store only chat-ID references |
| Deterministic under shuffled input | `tests/tree_properties.rs::output_is_deterministic_under_shuffled_input`: seeded Fisher–Yates shuffles of folders, chats, memberships, months, attachments → identical full walk |
| Lazy children, page boundaries | `children()` API; `any_page_size_enumerates_exactly_once` (property), `page_boundaries_chain_without_gaps_or_repeats`, `page_size_one_yields_the_same_tree` (fixtures) |
| Capability metadata read-only (DEC-007) | `capabilities_are_read_only_and_links_resolve` walks every node: no write capability representable |
| POL-1 stable names | `chat_with_username_uses_pol1_name`; `rename_preserves_every_identity` (SYNC-026: rename changes names, zero identities) |

## Verification commands run

- `cargo test -p gramdrive-model` — 46/46 green
- `make check` (suite `all`: toolchain, format, lint, test, architecture,
  supply-chain, traceability, scripts) — 8/8 green, provenance at
  `.temp/acceptance/local-all`

## Files touched

- `crates/gramdrive-model/src/tree.rs` (new, module + unit surface)
- `crates/gramdrive-model/src/lib.rs` (`pub mod tree`)
- `crates/gramdrive-model/src/identity.rs`, `src/identity/codec.rs`
  (additive kinds/format, capacity-comment fix)
- `crates/gramdrive-model/tests/tree_fixture.rs`,
  `tests/tree_properties.rs` (new)
- `crates/gramdrive-model/tests/identity_golden.rs`,
  `tests/identity_properties.rs` (new-kind coverage)
- `crates/gramdrive-model/README.md` (format table, tree builder section)
