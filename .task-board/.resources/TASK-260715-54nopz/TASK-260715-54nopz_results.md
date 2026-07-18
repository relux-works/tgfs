# TASK-260715-54nopz — Custom Telegram folder views — results

Status: ready for review (`to-review`).

## What was built

A deterministic sans-IO `FolderCatalogMachine`
(`crates/gramdrive-source-tdjson/src/folders.rs`, new) that discovers the custom
Telegram folders (chat filters) which populate the "Telegram Folders/" catalog
and folds TDLib's `updateChatFolders` into a normalized, provider-neutral
folder create / rename / delete / reorder change stream — the same altitude as
the sibling `SnapshotMachine` and `UpdateMachine`: typed output the composing
caller applies, no requests, no client.

It is a pure full-state reducer. `updateChatFolders` always carries the
*complete* ordered folder list, so `on_update` replaces the observed catalog
wholesale and `take_batch` drains the net change against the last-drained
baseline as a `FolderCatalogBatch { upserts, removed, invalidations }`.
`folders()` yields the ordered folder-id set a caller feeds into a
`SnapshotPlan` — the discovery the snapshot machine deliberately deferred here.

## The two-facts split (why membership needed no new code)

A folder is two separate facts:

- **Membership** — which chats it contains and in what order — is ordinary
  `chatListFolder` positions the snapshot and update machines already fold into
  `chat_list_entries` appearances (one canonical chat, one membership row per
  list, DOM-022). Nothing to add.
- **Definition** — that the folder exists, its title (its directory name), its
  tab position — is what neither machine discovers. `FolderCatalogMachine` owns
  exactly this and never touches a chat row.

So a chat in several folders stays one canonical `chats` record with one
appearance per list, and deleting a folder clears only that folder's
memberships — the caller applies each `FolderCatalogBatch::removed` with an
empty `replace_chat_list`, which already existed and leaves the canonical chats
and every other list untouched (SYNC-026). No state-schema or state-repo change.

## Acceptance criteria → evidence

- **Membership changes add/remove only appearances, preserve canonical data,
  emit complete changes.** The end-to-end suite drives both machines into a real
  `StateStore` and reads back from it:
  - `a_chat_in_two_folders_is_one_canonical_record_with_three_appearances` —
    Alice in Main + Work + Family is ONE `chats` row with three appearances; Bob
    a second row with one.
  - `deleting_a_folder_removes_appearances_only` — deleting Family clears only
    its `chat_list_entries`; Main and Work keep their members, both canonical
    chats survive (including Bob, whose last folder appearance was Family).
  - `renaming_a_folder_preserves_memberships_and_canonical_data` — a rename
    emits `FolderInvalidation::Renamed` and disturbs no membership.
  - `reordering_the_catalog_is_ordering_only` — a tab reorder emits a single
    `CatalogOrdering` and moves no appearance.
  - `creating_a_folder_adds_appearances_incrementally` — a new folder + a fresh
    folder position add a second appearance of the same canonical chat.

- **Folder create/rename/delete/reorder from source, applied incrementally.**
  Unit tests in `src/folders.rs` pin the normalized deltas and the POL-1
  invalidation split: `first_sight_creates_every_folder_in_catalog_order`,
  `rename_emits_folder_name_and_never_catalog_ordering`,
  `reorder_emits_catalog_ordering_only_and_never_a_rename`,
  `deletion_removes_the_folder_and_shifts_the_survivors`,
  `deleting_the_last_folder_leaves_the_earlier_ones_untouched`,
  `creating_a_folder_shifts_and_re_upserts_the_ones_after_it`,
  `a_duplicate_catalog_coalesces_to_a_noop`,
  `intermediate_observations_between_drains_coalesce`,
  `a_restart_re_push_converges_without_churn`.

- **Version-tolerant, fail-safe parse.**
  `title_is_read_across_tdlib_name_shapes` (modern `name.text.text`,
  intermediate `name.text`, bare `name`, legacy `title`, absent → empty) and
  `non_folder_updates_and_malformed_entries_are_ignored` (an entry without an id
  is skipped, still consuming its tab index).

## Provider invalidation split (POL-1 / SYNC-011)

Mirrors the chat machine's reorder/rename discipline:

- Rename (title changed) → `FolderInvalidation::Renamed` — the catalog directory
  is renamed; no order regenerates.
- Reorder (tab position changed only) → a single
  `FolderInvalidation::CatalogOrdering` — content, never a rename.
- Create → `Created`; delete → `Removed`; any set/order change → one
  `CatalogOrdering`. Deleting a folder in the middle legitimately shifts
  survivors' positions, so they are re-upserted for order (never renamed).

## Scope decision — folder-definition SQL persistence deferred (not a forced fit)

There is no table for folder names/order today, and adding one requires the
FIRST real schema migration (bump `SCHEMA_VERSION`, add a `MIGRATIONS` entry).
The migration machinery's own tests are written around "MIGRATIONS empty,
version == BASELINE" — `the_v1_fixture_opens_at_the_current_version` asserts
`schema_history == vec![SCHEMA_VERSION]` (a single application). Forcing that
migration here to store folder titles would break sibling state tests and step
on the metadata-state-store story (STORY-260715-16ik2x). The catalog machine
therefore emits typed folder definitions the composing caller / tree builder
consumes; name and order persistence lands with the tree builder's own storage.
This keeps ownership clean and the change purely additive to the source layer.

## TDLib wire shape (pinned commit 022d6020)

`updateChatFolders chat_folders:vector<chatFolderInfo> …`;
`chatFolderInfo id:int32 name:chatFolderName …`;
`chatFolderName text:formattedText …` — so the title is `name.text.text`.
`main_chat_list_position` / `are_tags_enabled` are ignored: Main and Archive are
separate top-level directories, not catalog entries.

## Quality gates

`make check` → 8/8 green (toolchain, format, lint `-D warnings`, test 26s,
architecture, supply-chain, traceability, scripts). Provenance
`.temp/acceptance/local-all`. No new dependencies; no cross-crate dependency
direction changed. Not run: `make tdjson-smoke` (real-linkage smoke) — this is
mock-only reducer logic, unaffected by FFI linkage, and no app-level runtime
surface exists yet to drive beyond the integration suite.

## Files

- `crates/gramdrive-source-tdjson/src/folders.rs` — new (machine + 12 unit tests)
- `crates/gramdrive-source-tdjson/tests/folder_catalog.rs` — new (5 integration suites)
- `crates/gramdrive-source-tdjson/src/lib.rs` — module + re-exports + docs
- `crates/gramdrive-source-tdjson/README.md` — folder-catalog section, table row, deps note

## Composing-caller contract (follow-up wiring)

Feed each `updateChatFolders` to `on_update`; per checkpoint apply the
`FolderCatalogBatch`: upsert changed definitions where the tree persists them,
and for each `removed` folder call `replace_chat_list(folder, &[])` to drop its
appearances. Use `folders()` to build the `SnapshotPlan` so each folder's
membership is enumerated.
