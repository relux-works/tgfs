# TASK-260715-1c8fea — Chat metadata & list update mapping — design

## What the task is
Live TDLib chat-metadata/list updates → a normalized, provider-neutral change
stream the composing caller applies transactionally into `gramdrive-state`
(`chats`, `chat_list_entries`), with per-change provider invalidation
classification (POL-1). Sits on top of the initial snapshot
(TASK-260715-30amrq): the snapshot bootstraps the baseline; this machine keeps
it current from the live update stream.

## Altitude / layering (matches SnapshotMachine)
- Sans-IO reducer in `gramdrive-source-tdjson` (product code depends only on
  `gramdrive-model`). Emits provider-neutral vocabulary; the composing caller
  (engine / integration test) maps it to state via the typed repositories —
  exactly the snapshot precedent (`tests/chat_snapshot.rs`).
- No requests issued. It is a pure `on_update(&Value)` + `take_batch()`
  reducer. Gap resolution (`getChat`) is the caller's job; the caller feeds
  the resolved chat object back as `updateNewChat`.

## Output vocabulary (`src/updates.rs`)
- `ChatMetadata { chat_id, kind, title, username, is_protected, photo }` —
  full current provider-visible facts; caller upserts the chat row (version
  derived from all these fields). `photo` is an opaque token (avatar not
  persisted in v1, but it advances DOM-003 so a changed avatar re-renders).
- `MembershipChange::Set { list, chat_id, sort_order, pinned }` /
  `Removed { list, chat_id }` — one chat's membership in one list.
- `Invalidation`:
  - `FolderName { chat_id }` — a **known** chat's title/username changed →
    stable folder name changes → rename event (POL-1). A pure reorder never
    emits this.
  - `ListOrdering { list }` — a list's membership/order changed → order.json
    regen only (POL-1: reorder is content, never a rename).
  - `ChatMetadata { chat_id }` — first sight, or photo/protection changed →
    metadata version advances, no folder/list touched.
- `UpdateBatch { chats, memberships, invalidations, unresolved }`. Applied in
  one transaction: upsert `chats` first (FK: `chat_list_entries → chats`),
  then `memberships`. Deterministic ordering (SYNC-030 spirit).

## Update mapping
| TDLib update | Effect |
|---|---|
| `updateNewChat` | full facts + embedded positions; first sight → dirty (`ChatMetadata` inval); resolves a prior gap |
| `updateChatTitle` | known: title fact; changed → dirty + `FolderName`. unknown → gap |
| `updateChatPhoto` | known: photo token; changed → dirty + `ChatMetadata`. unknown → gap |
| `updateChatHasProtectedContent` | known: is_protected; changed → dirty + `ChatMetadata`. unknown → gap |
| `updateChatPosition` | known: order≠0 → `Set`+`ListOrdering`; order=0 → `Removed`+`ListOrdering` (if it existed). unknown → gap |
| `updateChatRemovedFromList` | known: `Removed`+`ListOrdering` (if present) |
| `updateChatAddedToList` | ignored — the paired `updateChatPosition` carries the order and is authoritative |
| `updateUser` / `updateSupergroup` | peer username map; propagate to chats referencing the peer id; changed username → dirty + `FolderName` |
| other | ignored |

Secret / unsupported chats: known-but-excluded (POL-4/DEC-016) — never emitted,
their positions ignored, never counted as gaps.

## Key properties (AC)
- **Converge / replay:** each field is last-write-wins overwrite; a value equal
  to current marks nothing dirty. After `take_batch()` the dirty sets clear, so
  re-feeding the same updates yields an empty batch. State writes are idempotent
  upsert/replace. Restart = fresh machine + TDLib's re-pushed burst → same
  state; the caller only bumps `metadata_version` when content actually changes,
  so a restart re-emit is a true no-op (SYNC-003 snapshot stability).
- **Reorder does not change canonical ID:** a position change never marks the
  chat's metadata dirty → `chats` excludes it → `upsert_chat` not called → row,
  identity, and version untouched. Invalidation is `ListOrdering` only.
- **Rename → folder rename event:** title/username change of a known chat →
  `FolderName`.
- **Gap/restart:** an update about a chat with no known facts → `unresolved`
  (no partial/broken row emitted; membership held back because of the
  `chat_list_entries → chats` FK). Caller `getChat`s and feeds it back, or
  re-baselines via the snapshot (SYNC-023). There is no server-side resumable
  offset for the live stream (like `loadChats`); durability is idempotent
  convergence + snapshot re-baseline, not a fake cursor token.

## State additions (`repo/chats.rs`)
`replace_chat_list` is whole-list; live deltas are per-entry, and a partial
in-memory model must never be allowed to wipe a list. So add:
- `WriteTxn::upsert_chat_list_entry(&ChatListKey, &ChatListEntry)`
- `WriteTxn::remove_chat_list_entry(&ChatListKey, ChatId) -> bool`
Both idempotent; ordering read (`chat_list`) and order.json regen stay
consistent because they sort by `pinned DESC, sort_order DESC` at read time.

## Shared wire helpers (`src/wire.rs`)
Factor the leaf parsers shared with the snapshot (`parse_order`, `parse_list`,
`active_username`) into `pub(crate) wire.rs` — the string-int64 order parse is
subtle and must not drift. Snapshot re-points to them; its two unit tests move
with them.

## Tests
- `updates.rs` unit tests: mapping, no-op coalescing, gap→resolve, secret
  exclusion, username propagation, deterministic ordering.
- `tests/chat_updates.rs` integration: drive live updates over the real runtime
  + mock, apply batches through the state repos; assert reorder→order-only +
  stable canonical ID/version, rename→folder-rename, duplicate/out-of-order
  convergence, gap detection, restart convergence.
- `gramdrive-state` `repo_changes.rs`/new coverage for the two entry methods.
