//! Custom Telegram folder views, end to end (TASK-260715-54nopz): the sans-IO
//! [`FolderCatalogMachine`] fed `updateChatFolders`, and the
//! [`UpdateMachine`] fed the folder memberships, with both applied through the
//! typed `gramdrive-state` repositories — the composing caller a future engine
//! story will be.
//!
//! The suites assert the acceptance criteria straight from the store: a chat in
//! several folders is one canonical `chats` row with one `chat_list_entries`
//! appearance per list (DOM-022), a folder deletion removes only those
//! appearances and leaves the canonical chats and every other list intact
//! (SYNC-026), and the catalog machine's create/rename/reorder changes carry
//! the POL-1 invalidation split.

// clippy.toml exempts test code keyed on `#[test]` functions; the fixture
// helpers below sit at module level in an integration-test binary. This file
// links into no product artifact (established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::hash::{Hash, Hasher};

use serde_json::{Value, json};

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, ChatId, ChatKey, ChatListKey, ChatListKind, FolderId,
    NamespaceVersion,
};
use gramdrive_model::version::MetadataVersion;
use gramdrive_source_tdjson::SnapshotChatKind;
use gramdrive_source_tdjson::folders::{
    FolderCatalogBatch, FolderCatalogMachine, FolderDefinition, FolderInvalidation,
};
use gramdrive_source_tdjson::updates::{
    ChatMetadata, MembershipChange, UpdateBatch, UpdateMachine,
};
use gramdrive_state::StateStore;
use gramdrive_state::repo::{
    AccountRecord, ChatListEntry, ChatRecord, ChatType, RetentionMode, SourceKind,
};

const ACCOUNT_ID: i64 = 7;
const NAMESPACE: u32 = 1;
const NOW_MS: i64 = 5_000;

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(ACCOUNT_ID),
        },
        namespace_version: NamespaceVersion(NAMESPACE),
    }
}

fn folder(id: i32) -> ChatListKind {
    ChatListKind::Folder(FolderId(id))
}

// ---------------------------------------------------------------------------
// Update builders
// ---------------------------------------------------------------------------

fn list_json(list: ChatListKind) -> Value {
    match list {
        ChatListKind::Main => json!({"@type": "chatListMain"}),
        ChatListKind::Archive => json!({"@type": "chatListArchive"}),
        ChatListKind::Folder(id) => json!({"@type": "chatListFolder", "chat_folder_id": id.0}),
    }
}

fn position(list: ChatListKind, order: i64, pinned: bool) -> Value {
    json!({
        "@type": "chatPosition",
        "list": list_json(list),
        "order": order.to_string(),
        "is_pinned": pinned,
    })
}

/// An `updateNewChat` for a private chat with the given positions — the shape
/// the initial load and a `getChat` resolution both push.
fn new_private_chat(id: i64, user_id: i64, title: &str, positions: Vec<Value>) -> Value {
    json!({
        "@type": "updateNewChat",
        "chat": {
            "id": id,
            "type": {"@type": "chatTypePrivate", "user_id": user_id},
            "title": title,
            "positions": positions,
        },
    })
}

/// An `updateChatFolders` from `(id, title)` pairs, in tab order.
fn folders_update(folders: &[(i32, &str)]) -> Value {
    let chat_folders: Vec<Value> = folders
        .iter()
        .map(|(id, title)| {
            json!({
                "@type": "chatFolderInfo",
                "id": id,
                "name": {
                    "@type": "chatFolderName",
                    "text": {"@type": "formattedText", "text": title, "entities": []},
                },
            })
        })
        .collect();
    json!({
        "@type": "updateChatFolders",
        "chat_folders": chat_folders,
        "main_chat_list_position": 0,
    })
}

// ---------------------------------------------------------------------------
// Persistence — the composing caller's job
// ---------------------------------------------------------------------------

fn store_with_account() -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open in-memory store");
    let tx = store.write_txn().expect("write txn");
    tx.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Test Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        retention_mode: RetentionMode::Mirror,
        archive_mode: false,
        secret_ref: None,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
    })
    .expect("account row");
    tx.commit().expect("commit account");
    store
}

fn map_kind(kind: SnapshotChatKind) -> ChatType {
    match kind {
        SnapshotChatKind::Private => ChatType::Private,
        SnapshotChatKind::Group => ChatType::Group,
        SnapshotChatKind::Supergroup => ChatType::Supergroup,
        SnapshotChatKind::Channel => ChatType::Channel,
    }
}

/// A content-derived metadata version (DOM-003): equal facts yield the same
/// token, so a re-observation that changed nothing keeps the version.
fn derive_version(chat: &ChatMetadata) -> MetadataVersion {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (
        chat.chat_id,
        &chat.title,
        &chat.username,
        chat.is_protected,
        &chat.photo,
    )
        .hash(&mut hasher);
    MetadataVersion::new(format!("v{:016x}", hasher.finish())).expect("valid version")
}

/// Apply one chat-update batch as a composing caller must: canonical rows
/// first (the `chat_list_entries → chats` foreign key), memberships after.
fn apply_chat_batch(store: &mut StateStore, batch: &UpdateBatch) {
    let tx = store.write_txn().expect("write txn");
    for chat in &batch.chats {
        let key = ChatKey {
            scope: scope(),
            chat_id: ChatId(chat.chat_id),
        };
        tx.upsert_chat(&ChatRecord {
            key,
            chat_type: map_kind(chat.kind),
            title: chat.title.clone(),
            username: chat.username.clone(),
            is_protected: chat.is_protected,
            archive_mode: false,
            metadata_version: derive_version(chat),
            left_at_ms: None,
            deleted_at_ms: None,
            last_update_at_ms: Some(NOW_MS),
        })
        .expect("upsert chat");
    }
    for change in &batch.memberships {
        match change {
            MembershipChange::Set {
                list,
                chat_id,
                sort_order,
                pinned,
            } => tx
                .upsert_chat_list_entry(
                    &ChatListKey {
                        scope: scope(),
                        kind: *list,
                    },
                    &ChatListEntry {
                        chat_id: ChatId(*chat_id),
                        sort_order: *sort_order,
                        pinned: *pinned,
                    },
                )
                .expect("upsert membership"),
            MembershipChange::Removed { list, chat_id } => {
                tx.remove_chat_list_entry(
                    &ChatListKey {
                        scope: scope(),
                        kind: *list,
                    },
                    ChatId(*chat_id),
                )
                .expect("remove membership");
            }
        }
    }
    tx.commit().expect("commit chat batch");
}

/// Apply a folder-catalog batch's removals as a composing caller must: a
/// deleted folder's appearances are cleared with an empty membership replace,
/// which drops only that folder's `chat_list_entries` (SYNC-026). Folder
/// definition upserts have no persistence target in this schema version, so the
/// caller here consumes them but the suites assert them from the batch.
fn apply_folder_removals(store: &mut StateStore, batch: &FolderCatalogBatch) {
    let tx = store.write_txn().expect("write txn");
    for &id in &batch.removed {
        tx.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Folder(id),
            },
            &[],
        )
        .expect("clear deleted folder membership");
    }
    tx.commit().expect("commit folder removals");
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Every list an appearance can live in, in these tests — Main, Archive, and
/// the two custom folders the suites exercise. `appearance_count` sums a chat's
/// memberships across all of them, so it counts *every* appearance, not a
/// hand-picked subset.
const ALL_LISTS: [ChatListKind; 4] = [
    ChatListKind::Main,
    ChatListKind::Archive,
    ChatListKind::Folder(FolderId(4)),
    ChatListKind::Folder(FolderId(7)),
];

fn list_members(store: &mut StateStore, list: ChatListKind) -> Vec<i64> {
    let tx = store.read_txn().expect("read txn");
    tx.chat_list(&ChatListKey {
        scope: scope(),
        kind: list,
    })
    .expect("list reads")
    .into_iter()
    .map(|entry| entry.chat_id.0)
    .collect()
}

fn chat_exists(store: &mut StateStore, chat_id: i64) -> bool {
    let tx = store.read_txn().expect("read txn");
    tx.chat(&ChatKey {
        scope: scope(),
        chat_id: ChatId(chat_id),
    })
    .expect("chat reads")
    .is_some()
}

/// How many `chat_list_entries` reference one chat across every list — the
/// number of appearances that one canonical chat has. The `chats` primary key
/// makes the canonical row unique per chat id, so a chat that `chat_exists` and
/// has N appearances is exactly "one canonical record with N appearances".
fn appearance_count(store: &mut StateStore, chat_id: i64) -> usize {
    ALL_LISTS
        .iter()
        .filter(|&&list| list_members(store, list).contains(&chat_id))
        .count()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A chat that lives in Main and two custom folders is exactly one canonical
/// `chats` row with one appearance per list — the DOM-022 shared-canonical
/// invariant, read straight from the store.
#[test]
fn a_chat_in_two_folders_is_one_canonical_record_with_three_appearances() {
    let mut store = store_with_account();
    let mut catalog = FolderCatalogMachine::new();
    let mut updates = UpdateMachine::new();

    // The catalog announces two folders; their memberships arrive as ordinary
    // folder positions on the chats.
    catalog.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
    let catalog_batch = catalog.take_batch();
    assert_eq!(
        catalog.folders(),
        vec![FolderId(4), FolderId(7)],
        "the folder set feeds the snapshot plan in tab order"
    );
    assert_eq!(catalog_batch.removed, Vec::<FolderId>::new());

    // One chat in Main + both folders; a second chat only in Work.
    updates.on_update(&new_private_chat(
        101,
        1101,
        "Alice",
        vec![
            position(ChatListKind::Main, 9_000, false),
            position(folder(4), 3_000, false),
            position(folder(7), 2_000, true),
        ],
    ));
    updates.on_update(&new_private_chat(
        102,
        1102,
        "Bob",
        vec![position(folder(4), 1_000, false)],
    ));
    apply_chat_batch(&mut store, &updates.take_batch());

    // One canonical record each, and Alice appears once per list.
    assert!(
        chat_exists(&mut store, 101) && chat_exists(&mut store, 102),
        "two canonical chats, not one row per appearance"
    );
    assert_eq!(
        appearance_count(&mut store, 101),
        3,
        "Alice appears in Main, Work, and Family — three appearances, one chat"
    );
    assert_eq!(appearance_count(&mut store, 102), 1);
    assert_eq!(list_members(&mut store, ChatListKind::Main), vec![101]);
    assert_eq!(list_members(&mut store, folder(4)), vec![101, 102]);
    assert_eq!(list_members(&mut store, folder(7)), vec![101]);
}

/// Deleting a folder removes only its appearances: its `chat_list_entries`
/// vanish, but the canonical chats and every other list are untouched
/// (SYNC-026, the acceptance criterion).
#[test]
fn deleting_a_folder_removes_appearances_only() {
    let mut store = store_with_account();
    let mut catalog = FolderCatalogMachine::new();
    let mut updates = UpdateMachine::new();

    catalog.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
    let _ = catalog.take_batch();

    updates.on_update(&new_private_chat(
        101,
        1101,
        "Alice",
        vec![
            position(ChatListKind::Main, 9_000, false),
            position(folder(4), 3_000, false),
            position(folder(7), 2_000, false),
        ],
    ));
    updates.on_update(&new_private_chat(
        102,
        1102,
        "Bob",
        vec![position(folder(7), 1_000, false)],
    ));
    apply_chat_batch(&mut store, &updates.take_batch());
    assert_eq!(list_members(&mut store, folder(7)), vec![101, 102]);

    // The user deletes Family. TDLib re-pushes the catalog without it.
    catalog.on_update(&folders_update(&[(4, "Work")]));
    let batch = catalog.take_batch();
    assert_eq!(batch.removed, vec![FolderId(7)]);
    assert!(
        batch
            .invalidations
            .contains(&FolderInvalidation::Removed { id: FolderId(7) }),
        "the deletion is reported"
    );
    apply_folder_removals(&mut store, &batch);

    // Family's appearances are gone; nothing else is.
    assert_eq!(
        list_members(&mut store, folder(7)),
        Vec::<i64>::new(),
        "the deleted folder has no members"
    );
    assert_eq!(
        appearance_count(&mut store, 101),
        2,
        "Alice keeps Main and Work; only Family dropped"
    );
    assert_eq!(list_members(&mut store, ChatListKind::Main), vec![101]);
    assert_eq!(list_members(&mut store, folder(4)), vec![101]);
    // Bob was only in Family: his appearance is gone but his canonical row
    // stays (retention/tombstone is the engine's decision, not the folder's).
    assert_eq!(appearance_count(&mut store, 102), 0);
    assert!(
        chat_exists(&mut store, 101) && chat_exists(&mut store, 102),
        "deletion removes appearances, never canonical chats — both rows survive"
    );
}

/// A folder rename moves its directory name and touches no membership — the
/// POL-1 rename side of the split, with the canonical chats left alone.
#[test]
fn renaming_a_folder_preserves_memberships_and_canonical_data() {
    let mut store = store_with_account();
    let mut catalog = FolderCatalogMachine::new();
    let mut updates = UpdateMachine::new();

    catalog.on_update(&folders_update(&[(4, "Work")]));
    let _ = catalog.take_batch();
    updates.on_update(&new_private_chat(
        101,
        1101,
        "Alice",
        vec![position(folder(4), 3_000, false)],
    ));
    apply_chat_batch(&mut store, &updates.take_batch());
    let before = list_members(&mut store, folder(4));

    catalog.on_update(&folders_update(&[(4, "Job")]));
    let batch = catalog.take_batch();
    assert_eq!(
        batch.upserts,
        vec![FolderDefinition {
            id: FolderId(4),
            title: "Job".to_owned(),
            position: 0,
        }]
    );
    assert_eq!(
        batch.invalidations,
        vec![FolderInvalidation::Renamed { id: FolderId(4) }],
        "a rename regenerates no order and touches no appearance"
    );
    // A rename carries no membership or removal work.
    assert!(batch.removed.is_empty());
    apply_folder_removals(&mut store, &batch);
    assert_eq!(
        list_members(&mut store, folder(4)),
        before,
        "the folder's members are exactly as they were"
    );
}

/// Reordering the catalog regenerates the ordering document and nothing else:
/// no folder is renamed, and no appearance moves.
#[test]
fn reordering_the_catalog_is_ordering_only() {
    let mut store = store_with_account();
    let mut catalog = FolderCatalogMachine::new();
    let mut updates = UpdateMachine::new();

    catalog.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
    let _ = catalog.take_batch();
    updates.on_update(&new_private_chat(
        101,
        1101,
        "Alice",
        vec![position(folder(4), 3_000, false)],
    ));
    apply_chat_batch(&mut store, &updates.take_batch());
    let before = list_members(&mut store, folder(4));

    catalog.on_update(&folders_update(&[(7, "Family"), (4, "Work")]));
    let batch = catalog.take_batch();
    assert_eq!(
        batch.invalidations,
        vec![FolderInvalidation::CatalogOrdering],
        "a reorder is content, never a rename (POL-1)"
    );
    assert!(batch.removed.is_empty());
    apply_folder_removals(&mut store, &batch);
    assert_eq!(
        list_members(&mut store, folder(4)),
        before,
        "a catalog reorder never disturbs a folder's membership"
    );
    assert_eq!(catalog.folders(), vec![FolderId(7), FolderId(4)]);
}

/// Creating a folder and its members incrementally: the definition arrives on
/// `updateChatFolders`, the memberships as folder positions, and together they
/// add appearances without a second canonical chat.
#[test]
fn creating_a_folder_adds_appearances_incrementally() {
    let mut store = store_with_account();
    let mut catalog = FolderCatalogMachine::new();
    let mut updates = UpdateMachine::new();

    // Baseline: Alice is a Main chat with no folders yet.
    updates.on_update(&new_private_chat(
        101,
        1101,
        "Alice",
        vec![position(ChatListKind::Main, 9_000, false)],
    ));
    apply_chat_batch(&mut store, &updates.take_batch());
    assert_eq!(appearance_count(&mut store, 101), 1);

    // A new folder appears, and Alice is added to it (a fresh folder position).
    catalog.on_update(&folders_update(&[(4, "Work")]));
    let catalog_batch = catalog.take_batch();
    assert_eq!(
        catalog_batch.invalidations,
        vec![
            FolderInvalidation::Created { id: FolderId(4) },
            FolderInvalidation::CatalogOrdering,
        ]
    );
    updates.on_update(&json!({
        "@type": "updateChatPosition",
        "chat_id": 101,
        "position": position(folder(4), 3_000, false),
    }));
    apply_chat_batch(&mut store, &updates.take_batch());

    // The canonical chat is unchanged; it just gained an appearance.
    assert!(chat_exists(&mut store, 101), "still one canonical chat");
    assert_eq!(
        appearance_count(&mut store, 101),
        2,
        "the folder membership is a second appearance of the same chat"
    );
    assert_eq!(list_members(&mut store, folder(4)), vec![101]);
}
