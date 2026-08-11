//! Live chat-metadata/list updates, end to end (TASK-260715-1c8fea): the
//! sans-IO [`UpdateMachine`] fed TDLib's push updates over the real runtime and
//! mock, with every drained [`UpdateBatch`] applied through the typed
//! `gramdrive-state` repositories in one transaction — the composing caller a
//! future engine story will be.
//!
//! The caller here embodies the two decisions the machine deliberately leaves
//! out: it derives each chat's `metadata_version` from the emitted facts
//! (DOM-003 allows a content-derived version) and upserts only when that
//! version actually moves, so a duplicate or a restart re-push writes nothing;
//! and it applies memberships incrementally (`upsert_chat_list_entry` /
//! `remove_chat_list_entry`) so a reorder never rewrites a chat's canonical
//! row. The suites then assert the acceptance criteria straight from the store:
//! reorder does not change canonical identity, a rename raises a folder-rename
//! invalidation, and duplicate/out-of-order/gap/restart all converge.

// clippy.toml exempts test code keyed on `#[test]` functions; the fixture
// helpers below sit at module level in an integration-test binary. This file
// links into no product artifact (established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::hash::{Hash, Hasher};

use serde_json::{Value, json};

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, ChatId, ChatKey, ChatListKey, ChatListKind, FolderId,
    NamespaceVersion,
};
use gramdrive_model::version::MetadataVersion;
use gramdrive_source_tdjson::mock::MockHandle;
use gramdrive_source_tdjson::updates::{
    ChatMetadata, Invalidation, MembershipChange, UpdateBatch, UpdateMachine,
};
use gramdrive_source_tdjson::{SnapshotChatKind, TdClient, TdRuntime, UpdateStream};
use gramdrive_state::StateStore;
use gramdrive_state::repo::{
    AccountRecord, ChatListEntry, ChatRecord, ChatType, RetentionMode, SourceKind,
};

use common::{GUARD, start_runtime, test_config};

const ACCOUNT_ID: i64 = 7;
const NAMESPACE: u32 = 1;
const NOW_MS: i64 = 5_000;
const FOLDER: i32 = 4;

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(ACCOUNT_ID),
        },
        namespace_version: NamespaceVersion(NAMESPACE),
    }
}

fn folder_list() -> ChatListKind {
    ChatListKind::Folder(FolderId(FOLDER))
}

// ---------------------------------------------------------------------------
// Update builders
// ---------------------------------------------------------------------------

fn list_json(list: ChatListKind) -> Value {
    match list {
        ChatListKind::Main => json!({"@type": "chatListMain"}),
        ChatListKind::Archive => json!({"@type": "chatListArchive"}),
        ChatListKind::Stories => panic!("Stories is derived from storyListMain"),
        ChatListKind::Folder(folder) => {
            json!({"@type": "chatListFolder", "chat_folder_id": folder.0})
        }
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

fn set_title(chat_id: i64, title: &str) -> Value {
    json!({"@type": "updateChatTitle", "chat_id": chat_id, "title": title})
}

fn set_position(chat_id: i64, list: ChatListKind, order: i64, pinned: bool) -> Value {
    json!({
        "@type": "updateChatPosition",
        "chat_id": chat_id,
        "position": position(list, order, pinned),
    })
}

fn removed_from_list(chat_id: i64, list: ChatListKind) -> Value {
    json!({
        "@type": "updateChatRemovedFromList",
        "chat_id": chat_id,
        "chat_list": list_json(list),
    })
}

// ---------------------------------------------------------------------------
// Driving and persistence
// ---------------------------------------------------------------------------

/// A runtime over a fresh mock with one registered client — the driving ends.
fn start() -> (TdRuntime, MockHandle, TdClient, UpdateStream) {
    let (runtime, handle) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().expect("client registers");
    (runtime, handle, client, updates)
}

/// Push a burst of updates onto the client's stream and fold exactly that many
/// into the machine, in arrival order — the mock is FIFO, so the count is a
/// deterministic barrier that needs no request/response round trip.
fn feed(
    machine: &mut UpdateMachine,
    handle: &MockHandle,
    updates: &UpdateStream,
    client: &TdClient,
    events: &[Value],
) {
    for event in events {
        let mut event = event.clone();
        event["@client_id"] = json!(client.client_id());
        handle.push_event(&event.to_string());
    }
    for _ in 0..events.len() {
        let update = updates
            .recv_timeout(GUARD)
            .unwrap_or_else(|error| panic!("update must arrive within the guard: {error:?}"));
        machine.on_update(&update);
    }
}

/// A store with the account registered — the `chats → accounts` foreign key.
fn store_with_account() -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open in-memory store");
    let tx = store.write_txn().expect("write txn");
    tx.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Test Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        display_timezone: "UTC".to_owned(),
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

/// A content-derived metadata version (DOM-003): equal facts — including the
/// avatar token that has no column — yield the same token, so a re-observation
/// that changed nothing keeps the version and skips the write.
fn derive_version(chat: &ChatMetadata) -> MetadataVersion {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    map_kind(chat.kind).as_str_for_hash().hash(&mut hasher);
    chat.title.hash(&mut hasher);
    chat.username.hash(&mut hasher);
    chat.is_protected.hash(&mut hasher);
    chat.photo.hash(&mut hasher);
    MetadataVersion::new(format!("v{:016x}", hasher.finish())).expect("valid version")
}

trait KindHash {
    fn as_str_for_hash(&self) -> &'static str;
}
impl KindHash for ChatType {
    fn as_str_for_hash(&self) -> &'static str {
        match self {
            ChatType::Private => "private",
            ChatType::Group => "group",
            ChatType::Supergroup => "supergroup",
            ChatType::Channel => "channel",
        }
    }
}

/// Apply one drained batch exactly as a composing caller must: canonical rows
/// first (the `chat_list_entries → chats` foreign key), memberships after, all
/// in one transaction. Returns how many chat rows were actually written — zero
/// proves a no-op checkpoint.
fn apply_batch(store: &mut StateStore, batch: &UpdateBatch) -> usize {
    let tx = store.write_txn().expect("write txn");
    let mut written = 0;
    for chat in &batch.chats {
        let key = ChatKey {
            scope: scope(),
            chat_id: ChatId(chat.chat_id),
        };
        let current = tx.read().chat(&key).expect("read chat");
        let version = derive_version(chat);
        if current
            .as_ref()
            .is_some_and(|row| row.metadata_version == version)
        {
            continue; // Nothing moved — keep the row and its version.
        }
        let (archive_mode, left_at_ms, deleted_at_ms) = current
            .as_ref()
            .map(|row| (row.archive_mode, row.left_at_ms, row.deleted_at_ms))
            .unwrap_or((false, None, None));
        tx.upsert_chat(&ChatRecord {
            key,
            chat_type: map_kind(chat.kind),
            title: chat.title.clone(),
            username: chat.username.clone(),
            is_protected: chat.is_protected,
            archive_mode,
            metadata_version: version,
            left_at_ms,
            deleted_at_ms,
            last_update_at_ms: Some(NOW_MS),
        })
        .expect("upsert chat");
        written += 1;
    }
    for change in &batch.memberships {
        match change {
            MembershipChange::Set {
                list,
                chat_id,
                sort_order,
                pinned,
            } => {
                tx.upsert_chat_list_entry(
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
                .expect("upsert membership");
            }
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
    tx.commit().expect("commit batch");
    written
}

fn read_list(store: &mut StateStore, kind: ChatListKind) -> Vec<(i64, i64, bool)> {
    let tx = store.read_txn().expect("read txn");
    tx.chat_list(&ChatListKey {
        scope: scope(),
        kind,
    })
    .expect("list reads")
    .iter()
    .map(|entry| (entry.chat_id.0, entry.sort_order, entry.pinned))
    .collect()
}

fn read_chat(store: &mut StateStore, chat_id: i64) -> ChatRecord {
    let tx = store.read_txn().expect("read txn");
    tx.chat(&ChatKey {
        scope: scope(),
        chat_id: ChatId(chat_id),
    })
    .expect("chat reads")
    .expect("chat exists")
}

/// The baseline burst: three private chats across Main and a folder, with
/// their usernames, exactly as the initial load pushes them.
fn baseline() -> Vec<Value> {
    vec![
        json!({"@type": "updateUser", "user": {"id": 1101, "usernames":
            {"editable_username": "alice", "active_usernames": ["alice"]}}}),
        new_private_chat(
            101,
            1101,
            "Alice",
            vec![
                position(ChatListKind::Main, 9_000, true),
                position(folder_list(), 3_000, false),
            ],
        ),
        new_private_chat(
            102,
            1102,
            "Bob",
            vec![position(ChatListKind::Main, 8_000, false)],
        ),
        new_private_chat(
            103,
            1103,
            "Carol",
            vec![position(ChatListKind::Main, 7_000, false)],
        ),
    ]
}

// ---------------------------------------------------------------------------
// Suites
// ---------------------------------------------------------------------------

#[test]
fn baseline_and_live_deltas_apply_into_state() {
    let (_runtime, handle, client, updates) = start();
    let mut store = store_with_account();
    let mut machine = UpdateMachine::new();

    feed(&mut machine, &handle, &updates, &client, &baseline());
    let base = machine.take_batch();
    assert_eq!(base.chats.len(), 3);
    apply_batch(&mut store, &base);

    // Main in exact presentation order, and the folder appearance.
    assert_eq!(
        read_list(&mut store, ChatListKind::Main),
        vec![(101, 9_000, true), (102, 8_000, false), (103, 7_000, false),]
    );
    assert_eq!(
        read_list(&mut store, folder_list()),
        vec![(101, 3_000, false)]
    );
    assert_eq!(
        read_chat(&mut store, 101).username.as_deref(),
        Some("alice")
    );

    // A live burst: rename Bob, reorder Carol above Bob, Alice leaves the
    // folder, and a fresh chat arrives pinned.
    feed(
        &mut machine,
        &handle,
        &updates,
        &client,
        &[
            set_title(102, "Bobby"),
            set_position(103, ChatListKind::Main, 8_500, false),
            removed_from_list(101, folder_list()),
            new_private_chat(
                104,
                1104,
                "Dave",
                vec![position(ChatListKind::Main, 20_000, true)],
            ),
        ],
    );
    let batch = machine.take_batch();
    apply_batch(&mut store, &batch);

    assert_eq!(read_chat(&mut store, 102).title, "Bobby");
    // Alice left only the folder, so she stays pinned in Main.
    assert_eq!(
        read_list(&mut store, ChatListKind::Main),
        vec![
            (104, 20_000, true),
            (101, 9_000, true),
            (103, 8_500, false),
            (102, 8_000, false),
        ]
    );
    assert!(
        read_list(&mut store, folder_list()).is_empty(),
        "Alice left the folder"
    );
    // Canonical rows survive leaving a list (SYNC-026).
    assert!(
        read_chat(&mut store, 101).left_at_ms.is_none(),
        "leaving a list is not a POL-3 tombstone at this layer"
    );

    // The invalidations name the rename and the touched lists, and only those.
    assert!(
        batch
            .invalidations
            .contains(&Invalidation::FolderName { chat_id: 102 })
    );
    assert!(batch.invalidations.contains(&Invalidation::ListOrdering {
        list: ChatListKind::Main
    }));
    assert!(batch.invalidations.contains(&Invalidation::ListOrdering {
        list: folder_list()
    }));
    assert!(
        !batch.invalidations.iter().any(|inval| matches!(
            inval,
            Invalidation::FolderName { chat_id } if *chat_id == 103
        )),
        "a reorder never raises a folder rename"
    );
}

#[test]
fn reorder_keeps_canonical_row_and_version_and_regenerates_order_only() {
    let (_runtime, handle, client, updates) = start();
    let mut store = store_with_account();
    let mut machine = UpdateMachine::new();

    feed(&mut machine, &handle, &updates, &client, &baseline());
    apply_batch(&mut store, &machine.take_batch());
    let before = read_chat(&mut store, 103);

    // Carol reorders and pins — a pure position change.
    feed(
        &mut machine,
        &handle,
        &updates,
        &client,
        &[set_position(103, ChatListKind::Main, 50_000, true)],
    );
    let batch = machine.take_batch();
    assert!(
        batch.chats.is_empty(),
        "reorder emits no canonical metadata"
    );
    assert_eq!(
        batch.invalidations,
        vec![Invalidation::ListOrdering {
            list: ChatListKind::Main
        }]
    );
    let written = apply_batch(&mut store, &batch);
    assert_eq!(written, 0, "no canonical row is rewritten by a reorder");

    // The canonical row — identity and version — is byte-for-byte unchanged.
    let after = read_chat(&mut store, 103);
    assert_eq!(after, before, "reorder does not change canonical identity");
    // The order moved, though.
    assert_eq!(
        read_list(&mut store, ChatListKind::Main)[0],
        (103, 50_000, true)
    );
}

#[test]
fn duplicate_and_out_of_order_updates_converge() {
    let (_runtime, handle, client, updates) = start();
    let mut store = store_with_account();
    let mut machine = UpdateMachine::new();

    feed(&mut machine, &handle, &updates, &client, &baseline());
    apply_batch(&mut store, &machine.take_batch());

    // A title that flips and settles, each step duplicated, and a position
    // repeated verbatim — the machine coalesces to the final observed state.
    feed(
        &mut machine,
        &handle,
        &updates,
        &client,
        &[
            set_title(102, "Robert"),
            set_title(102, "Robert"),
            set_title(102, "Bobby"),
            set_position(103, ChatListKind::Main, 7_000, false),
            set_title(102, "Bobby"),
        ],
    );
    let batch = machine.take_batch();
    apply_batch(&mut store, &batch);
    assert_eq!(read_chat(&mut store, 102).title, "Bobby");
    // The repeated position matched the baseline, so it never became a change.
    assert!(
        !batch
            .memberships
            .iter()
            .any(|change| matches!(change, MembershipChange::Set { chat_id: 103, .. })),
        "a position equal to the known one is a no-op"
    );

    // Re-observing the current state (Bobby's title and Carol's position, both
    // as they now stand) writes nothing: convergence is a fixed point.
    feed(
        &mut machine,
        &handle,
        &updates,
        &client,
        &[
            set_title(102, "Bobby"),
            set_position(103, ChatListKind::Main, 7_000, false),
        ],
    );
    let replay = machine.take_batch();
    assert!(
        replay.is_empty(),
        "re-observing the current state is a no-op: {replay:?}"
    );
    assert_eq!(apply_batch(&mut store, &replay), 0);
    assert_eq!(read_chat(&mut store, 102).title, "Bobby");
}

#[test]
fn a_restart_re_pushes_current_state_and_converges_without_churn() {
    // Session one: baseline, then Bob is renamed.
    let mut store = store_with_account();
    {
        let (_runtime, handle, client, updates) = start();
        let mut machine = UpdateMachine::new();
        feed(&mut machine, &handle, &updates, &client, &baseline());
        apply_batch(&mut store, &machine.take_batch());
        feed(
            &mut machine,
            &handle,
            &updates,
            &client,
            &[set_title(102, "Bobby")],
        );
        apply_batch(&mut store, &machine.take_batch());
    }
    let main_before = read_list(&mut store, ChatListKind::Main);
    let versions_before: Vec<MetadataVersion> = [101, 102, 103]
        .iter()
        .map(|id| read_chat(&mut store, *id).metadata_version)
        .collect();

    // Session two: a fresh machine fed TDLib's re-pushed burst, which carries
    // Bob's *current* title. Nothing should be rewritten.
    let (_runtime, handle, client, updates) = start();
    let mut machine = UpdateMachine::new();
    let mut restart = baseline();
    restart[2] = new_private_chat(
        102,
        1102,
        "Bobby",
        vec![position(ChatListKind::Main, 8_000, false)],
    );
    feed(&mut machine, &handle, &updates, &client, &restart);
    let batch = machine.take_batch();
    let written = apply_batch(&mut store, &batch);

    assert_eq!(
        written, 0,
        "a restart that observes the same state rewrites nothing"
    );
    assert_eq!(
        read_list(&mut store, ChatListKind::Main),
        main_before,
        "order is stable"
    );
    let versions_after: Vec<MetadataVersion> = [101, 102, 103]
        .iter()
        .map(|id| read_chat(&mut store, *id).metadata_version)
        .collect();
    assert_eq!(
        versions_after, versions_before,
        "metadata versions do not churn"
    );
}

#[test]
fn an_update_before_its_chat_is_a_gap_then_resolves() {
    let (_runtime, handle, client, updates) = start();
    let mut store = store_with_account();
    let mut machine = UpdateMachine::new();

    // A title and a position for a chat no `updateNewChat` announced yet.
    feed(
        &mut machine,
        &handle,
        &updates,
        &client,
        &[
            set_title(500, "Ghost"),
            set_position(500, ChatListKind::Main, 100, false),
        ],
    );
    let batch = machine.take_batch();
    assert_eq!(batch.unresolved, vec![500]);
    assert!(batch.chats.is_empty(), "no forged canonical row");
    assert!(
        batch.memberships.is_empty(),
        "no membership without a chat row"
    );
    assert_eq!(apply_batch(&mut store, &batch), 0);

    // The caller getChats it and feeds the full object back: current title and
    // positions arrive together (SYNC-023 resolution).
    feed(
        &mut machine,
        &handle,
        &updates,
        &client,
        &[new_private_chat(
            500,
            1500,
            "Grace",
            vec![position(ChatListKind::Main, 100, false)],
        )],
    );
    let batch = machine.take_batch();
    assert!(batch.unresolved.is_empty());
    apply_batch(&mut store, &batch);
    assert_eq!(read_chat(&mut store, 500).title, "Grace");
    assert_eq!(
        read_list(&mut store, ChatListKind::Main),
        vec![(500, 100, false)]
    );
}
