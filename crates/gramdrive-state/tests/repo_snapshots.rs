//! Snapshot repositories (TASK-260715-1opnb2): accounts, chats and list
//! order (DEC-013), the items projection with paged enumeration (SYNC-003)
//! and version compare-and-set (DOM-003), attachments and blobs
//! (SYNC-045).

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use common::{account_record, chat_record, revision, scope};
use gramdrive_state::model::identity::{
    AccountId, AccountKey, AttachmentIndex, AttachmentKey, ChatId, ChatListKey, ChatListKind,
    ContentHash, DocFormat, DocPartition, FolderId, ItemId, MessageId, MessageKey,
    NamespaceVersion,
};
use gramdrive_state::model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::repo::{
    AttachmentAvailability, AttachmentFacts, ChatListEntry, FileFacts, ItemAvailability,
    ItemRecord, MessageChange,
};
use gramdrive_state::{StateError, StateStore};

const CHAT: i64 = 100;

fn store() -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    let tx = store.write_txn().expect("write txn");
    tx.upsert_account(&account_record()).expect("account");
    tx.commit().expect("commit");
    store
}

fn version(text: &str) -> MetadataVersion {
    MetadataVersion::new(text).expect("valid version")
}

fn content_version(text: &str) -> ContentVersion {
    ContentVersion::new(text).expect("valid version")
}

fn dir_item(id: &ItemId, parent: Option<&ItemId>, safe_name: &str) -> ItemRecord {
    ItemRecord {
        id: id.clone(),
        parent: parent.cloned(),
        display_name: safe_name.to_owned(),
        safe_name: safe_name.to_owned(),
        metadata_version: version("m1"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: Some(1_000),
        modified_at_ms: Some(1_000),
        deleted_at_ms: None,
    }
}

fn doc_item(id: &ItemId, parent: &ItemId, safe_name: &str) -> ItemRecord {
    ItemRecord {
        id: id.clone(),
        parent: Some(parent.clone()),
        display_name: safe_name.to_owned(),
        safe_name: safe_name.to_owned(),
        metadata_version: version("m1"),
        content: Some(FileFacts {
            mime_type: Some("application/x-ndjson".to_owned()),
            logical_size: Some(64),
            content_version: Some(content_version("v1")),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: Some(1_000),
        modified_at_ms: Some(1_000),
        deleted_at_ms: None,
    }
}

#[test]
fn accounts_round_trip_and_upserts_never_rewind_the_epoch() {
    let mut store = store();
    let read = store.read_txn().expect("read");
    let stored = read
        .account(scope().account)
        .expect("account")
        .expect("some");
    assert_eq!(stored, account_record());
    assert_eq!(stored.scope(), scope());
    assert_eq!(read.accounts().expect("list"), vec![account_record()]);
    assert_eq!(
        read.current_scope(scope().account).expect("scope"),
        Some(scope())
    );
    assert_eq!(
        read.account(AccountKey {
            account_id: AccountId(999)
        })
        .expect("account"),
        None
    );
    drop(read);

    // An upsert replaying a stale record must not rewind the epoch or the
    // creation time (DOM-021).
    let tx = store.write_txn().expect("write");
    let bumped = tx.bump_namespace(scope().account, 5_000).expect("bump");
    let mut stale = account_record();
    stale.display_name = "Renamed".to_owned();
    stale.created_at_ms = 9_999;
    tx.upsert_account(&stale).expect("upsert");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let stored = read
        .account(scope().account)
        .expect("account")
        .expect("some");
    assert_eq!(stored.display_name, "Renamed");
    assert_eq!(stored.namespace_version, bumped, "epoch must not rewind");
    assert_eq!(stored.created_at_ms, 1_000, "creation time is immutable");
}

#[test]
fn account_validation_and_missing_rows_are_typed() {
    let mut store = store();
    let tx = store.write_txn().expect("write");
    let mut bad = account_record();
    bad.auth_state = String::new();
    match tx.upsert_account(&bad) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    let mut bad = account_record();
    bad.secret_ref = Some(String::new());
    match tx.upsert_account(&bad) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    match tx.bump_namespace(
        AccountKey {
            account_id: AccountId(999),
        },
        1_000,
    ) {
        Err(StateError::RowNotFound { entity: "account" }) => {}
        other => panic!("expected RowNotFound, got {other:?}"),
    }
}

#[test]
fn chats_round_trip_including_tombstone_markers() {
    let mut store = store();
    let tx = store.write_txn().expect("write");
    let mut record = chat_record(CHAT);
    record.username = Some("some_chat".to_owned());
    record.left_at_ms = Some(4_000);
    record.last_update_at_ms = Some(3_000);
    tx.upsert_chat(&record).expect("chat");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    assert_eq!(
        read.chat(&common::chat_key(CHAT)).expect("chat"),
        Some(record)
    );
    assert_eq!(read.chat(&common::chat_key(CHAT + 1)).expect("chat"), None);
}

#[test]
fn chat_lists_replace_atomically_and_read_in_presentation_order() {
    let mut store = store();
    let main = ChatListKey {
        scope: scope(),
        kind: ChatListKind::Main,
    };
    let folder = ChatListKey {
        scope: scope(),
        kind: ChatListKind::Folder(FolderId(5)),
    };

    let tx = store.write_txn().expect("write");
    for chat in [1, 2, 3] {
        tx.upsert_chat(&chat_record(chat)).expect("chat");
    }
    tx.replace_chat_list(
        &main,
        &[
            ChatListEntry {
                chat_id: ChatId(1),
                sort_order: 100,
                pinned: false,
            },
            ChatListEntry {
                chat_id: ChatId(2),
                sort_order: 900,
                pinned: false,
            },
            ChatListEntry {
                chat_id: ChatId(3),
                sort_order: 50,
                pinned: true,
            },
        ],
    )
    .expect("replace");
    tx.replace_chat_list(
        &folder,
        &[ChatListEntry {
            chat_id: ChatId(2),
            sort_order: 1,
            pinned: false,
        }],
    )
    .expect("replace folder");
    tx.commit().expect("commit");

    // Pinned first, then Telegram order descending (POL-1).
    let read = store.read_txn().expect("read");
    let order: Vec<i64> = read
        .chat_list(&main)
        .expect("list")
        .iter()
        .map(|entry| entry.chat_id.0)
        .collect();
    assert_eq!(order, vec![3, 2, 1]);
    assert_eq!(read.chat_list(&folder).expect("list").len(), 1);
    drop(read);

    // Replacement is total: a chat that left the list is gone (DEC-013).
    let tx = store.write_txn().expect("write");
    tx.replace_chat_list(
        &main,
        &[ChatListEntry {
            chat_id: ChatId(1),
            sort_order: 100,
            pinned: false,
        }],
    )
    .expect("replace");
    tx.commit().expect("commit");
    let read = store.read_txn().expect("read");
    assert_eq!(read.chat_list(&main).expect("list").len(), 1);
    assert_eq!(
        read.chat_list(&folder).expect("list").len(),
        1,
        "other lists are untouched"
    );
    drop(read);

    // Folder id 0 is the built-in-list sentinel, never a real folder.
    let tx = store.write_txn().expect("write");
    match tx.replace_chat_list(
        &ChatListKey {
            scope: scope(),
            kind: ChatListKind::Folder(FolderId(0)),
        },
        &[],
    ) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

/// The item tree every item test builds: root, main list, chat, doc.
fn build_tree(store: &mut StateStore) -> (ItemId, ItemId, ItemId, ItemId) {
    let root_id = common::account_root_id();
    let list_id = common::chat_list_id(ChatListKind::Main);
    let chat_id = common::appearance_id(ChatListKind::Main, common::chat_canonical_key(CHAT));
    let doc_id = gramdrive_state::model::identity::ItemKey::Appearance(
        gramdrive_state::model::identity::AppearanceKey {
            view: ChatListKind::Main,
            item: common::doc_key(CHAT, DocPartition::Year { year: 2026 }, DocFormat::Ndjson),
        },
    )
    .id();

    let tx = store.write_txn().expect("write");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    tx.upsert_item(&dir_item(&root_id, None, "Test Account"))
        .expect("root");
    tx.upsert_item(&dir_item(&list_id, Some(&root_id), "Main"))
        .expect("list");
    tx.upsert_item(&dir_item(&chat_id, Some(&list_id), "Chat 100"))
        .expect("chat item");
    tx.upsert_item(&doc_item(&doc_id, &chat_id, "2026.ndjson"))
        .expect("doc");
    tx.commit().expect("commit");
    (root_id, list_id, chat_id, doc_id)
}

#[test]
fn items_round_trip_with_identity_derived_columns() {
    let mut store = store();
    let (root_id, list_id, chat_id, doc_id) = build_tree(&mut store);

    let read = store.read_txn().expect("read");
    let root = read.item(&root_id).expect("item").expect("some");
    assert_eq!(root, dir_item(&root_id, None, "Test Account"));
    let doc = read.item(&doc_id).expect("item").expect("some");
    assert_eq!(doc, doc_item(&doc_id, &chat_id, "2026.ndjson"));

    // Path resolution one component at a time (DOM-005).
    let resolved = read
        .child_by_name(&root_id, "Main")
        .expect("child")
        .expect("some");
    assert_eq!(resolved.id, list_id);
    assert_eq!(
        read.child_by_name(&root_id, "Missing").expect("child"),
        None
    );

    // The appearance row is reachable from its canonical id (SYNC-026).
    let canonical =
        gramdrive_state::model::identity::ItemKey::Canonical(common::chat_canonical_key(CHAT)).id();
    let appearances = read.appearances_of(&canonical).expect("appearances");
    assert_eq!(appearances.len(), 1);
    assert_eq!(appearances[0].id, chat_id);
}

#[test]
fn children_pages_anchor_on_the_last_returned_id() {
    let mut store = store();
    let (_, list_id, chat_id, _) = build_tree(&mut store);

    // More children under the chat: docs for four more years.
    let tx = store.write_txn().expect("write");
    for year in 2022..=2025 {
        let id = gramdrive_state::model::identity::ItemKey::Appearance(
            gramdrive_state::model::identity::AppearanceKey {
                view: ChatListKind::Main,
                item: common::doc_key(CHAT, DocPartition::Year { year }, DocFormat::Ndjson),
            },
        )
        .id();
        tx.upsert_item(&doc_item(&id, &chat_id, &format!("{year}.ndjson")))
            .expect("doc");
    }
    tx.commit().expect("commit");

    // Walk pages of 2 to exhaustion; every child exactly once (SYNC-003).
    let read = store.read_txn().expect("read");
    let mut seen = Vec::new();
    let mut anchor: Option<ItemId> = None;
    loop {
        let page = read
            .children_page(&chat_id, anchor.as_ref(), 2)
            .expect("page");
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 2);
        anchor = Some(page[page.len() - 1].id.clone());
        seen.extend(page.into_iter().map(|item| item.id));
    }
    assert_eq!(seen.len(), 5);
    let mut sorted = seen.clone();
    sorted.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    assert_eq!(seen, sorted, "pages walk stable id order");
    // The list root has exactly one child and an empty page after it.
    assert_eq!(
        read.children_page(&list_id, None, 10).expect("page").len(),
        1
    );
}

#[test]
fn item_structure_violations_are_typed_not_check_failures() {
    let mut store = store();
    let root_id = common::account_root_id();
    let list_id = common::chat_list_id(ChatListKind::Main);

    let tx = store.write_txn().expect("write");
    // The account root, and only the account root, has no parent.
    match tx.upsert_item(&dir_item(&root_id, Some(&list_id), "Root")) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    match tx.upsert_item(&dir_item(&list_id, None, "Main")) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    // Directories carry no content facts.
    tx.upsert_item(&dir_item(&root_id, None, "Root"))
        .expect("root");
    let mut bad = dir_item(&list_id, Some(&root_id), "Main");
    bad.content = Some(FileFacts::default());
    match tx.upsert_item(&bad) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    // Messages are not provider nodes in v1.
    let message_id = gramdrive_state::model::identity::ItemKey::Canonical(
        gramdrive_state::model::identity::CanonicalKey::Message(MessageKey {
            chat: common::chat_key(CHAT),
            message_id: MessageId(1),
        }),
    )
    .id();
    match tx.upsert_item(&dir_item(&message_id, Some(&root_id), "msg")) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    // Empty safe names never reach SQL.
    let mut bad = dir_item(&list_id, Some(&root_id), "Main");
    bad.safe_name = String::new();
    match tx.upsert_item(&bad) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[test]
fn account_root_reads_its_epoch_from_the_account_row() {
    let mut store = StateStore::open_in_memory().expect("open");
    // No account yet: the root cannot be projected.
    let tx = store.write_txn().expect("write");
    match tx.upsert_item(&dir_item(&common::account_root_id(), None, "Root")) {
        Err(StateError::RowNotFound { entity: "account" }) => {}
        other => panic!("expected RowNotFound(account), got {other:?}"),
    }
    drop(tx);

    let tx = store.write_txn().expect("write");
    tx.upsert_account(&account_record()).expect("account");
    tx.upsert_item(&dir_item(&common::account_root_id(), None, "Root"))
        .expect("root");
    tx.commit().expect("commit");
    let namespace: i64 = store
        .connection()
        .query_row("SELECT namespace_version FROM items", [], |row| row.get(0))
        .expect("namespace");
    assert_eq!(
        NamespaceVersion(u32::try_from(namespace).expect("fits")),
        scope().namespace_version
    );
}

#[test]
fn item_content_updates_are_compare_and_set() {
    let mut store = store();
    let (root_id, _, _, doc_id) = build_tree(&mut store);

    let tx = store.write_txn().expect("write");
    // Stale expectation: the doc is at v1, not unversioned.
    match tx.update_item_content(
        &doc_id,
        None,
        &FileFacts {
            mime_type: Some("application/x-ndjson".to_owned()),
            logical_size: Some(128),
            content_version: Some(content_version("v2")),
        },
        &version("m2"),
        2_000,
    ) {
        Err(StateError::VersionConflict {
            entity: "item content",
            expected: None,
            found: Some(found),
        }) => assert_eq!(found, "v1"),
        other => panic!("expected VersionConflict, got {other:?}"),
    }
    // The right expectation applies.
    tx.update_item_content(
        &doc_id,
        Some(&content_version("v1")),
        &FileFacts {
            mime_type: Some("application/x-ndjson".to_owned()),
            logical_size: Some(128),
            content_version: Some(content_version("v2")),
        },
        &version("m2"),
        2_000,
    )
    .expect("update");
    // Directories have no content to set; unknown items are named.
    match tx.update_item_content(&root_id, None, &FileFacts::default(), &version("m2"), 2_000) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    let unknown = gramdrive_state::model::identity::ItemKey::Appearance(
        gramdrive_state::model::identity::AppearanceKey {
            view: ChatListKind::Archive,
            item: common::doc_key(CHAT, DocPartition::Chat, DocFormat::Markdown),
        },
    )
    .id();
    match tx.update_item_content(&unknown, None, &FileFacts::default(), &version("m2"), 2_000) {
        Err(StateError::RowNotFound { entity: "item" }) => {}
        other => panic!("expected RowNotFound, got {other:?}"),
    }
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let doc = read.item(&doc_id).expect("item").expect("some");
    let content = doc.content.expect("content");
    assert_eq!(
        content.content_version.map(|v| v.as_str().to_owned()),
        Some("v2".to_owned())
    );
    assert_eq!(content.logical_size, Some(128));
    assert_eq!(doc.metadata_version, version("m2"));
    assert_eq!(doc.modified_at_ms, Some(2_000));
}

#[test]
fn tombstones_free_the_sibling_name_and_are_idempotent() {
    let mut store = store();
    let (_, _, chat_id, doc_id) = build_tree(&mut store);

    let tx = store.write_txn().expect("write");
    tx.tombstone_item(&doc_id, 5_000, &version("m2"))
        .expect("tombstone");
    // Idempotent: the original observation time survives.
    tx.tombstone_item(&doc_id, 9_000, &version("m3"))
        .expect("again");
    // A live successor may reuse the name (SYNC-012).
    let successor = gramdrive_state::model::identity::ItemKey::Appearance(
        gramdrive_state::model::identity::AppearanceKey {
            view: ChatListKind::Main,
            item: common::doc_key(CHAT, DocPartition::Year { year: 2026 }, DocFormat::Markdown),
        },
    )
    .id();
    tx.upsert_item(&doc_item(&successor, &chat_id, "2026.ndjson"))
        .expect("successor");
    match tx.tombstone_item(
        &gramdrive_state::model::identity::ItemKey::Canonical(common::year_dir_key(CHAT, 1999))
            .id(),
        1_000,
        &version("m2"),
    ) {
        Err(StateError::RowNotFound { entity: "item" }) => {}
        other => panic!("expected RowNotFound, got {other:?}"),
    }
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let dead = read.item(&doc_id).expect("item").expect("some");
    assert_eq!(dead.deleted_at_ms, Some(5_000));
    let resolved = read
        .child_by_name(&chat_id, "2026.ndjson")
        .expect("child")
        .expect("some");
    assert_eq!(resolved.id, successor);
    // Tombstoned rows leave enumeration.
    let children = read.children_page(&chat_id, None, 10).expect("page");
    assert_eq!(children.len(), 1);
}

#[test]
fn attachment_refresh_never_detaches_verified_bytes() {
    let mut store = store();
    let chat = common::chat_key(CHAT);
    let key = AttachmentKey {
        message: MessageKey {
            chat,
            message_id: MessageId(1),
        },
        index: AttachmentIndex(0),
    };
    let facts = AttachmentFacts {
        key,
        original_name: Some("photo.jpg".to_owned()),
        mime_type: Some("image/jpeg".to_owned()),
        logical_size: Some(2_048),
        content_version: content_version("v1"),
        telegram_unique_id: Some("uniq".to_owned()),
        telegram_file_id: Some("file-1".to_owned()),
        file_reference: Some(b"ref-1".to_vec()),
        availability: AttachmentAvailability::Fetchable,
        can_be_saved: true,
    };
    let hash = ContentHash::Sha256([7u8; 32]);

    let tx = store.write_txn().expect("write");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    tx.apply_message_changes(&chat, &[MessageChange::Observed(revision(1, 1_000))])
        .expect("message");
    tx.upsert_attachment(&facts).expect("attachment");

    // Linking requires the blob row first — no dangling references.
    match tx.link_attachment_blob(&key, &hash, 2_000) {
        Err(StateError::RowNotFound { entity: "blob" }) => {}
        other => panic!("expected RowNotFound(blob), got {other:?}"),
    }
    tx.record_blob(scope().account, &hash, 2_048, 2_000)
        .expect("blob");
    // Idempotent: recording the same bytes again keeps first-seen.
    tx.record_blob(scope().account, &hash, 2_048, 9_000)
        .expect("blob again");
    tx.link_attachment_blob(&key, &hash, 2_000).expect("link");

    // A locator refresh (SYNC-045) rewrites metadata only.
    let mut refreshed = facts.clone();
    refreshed.telegram_file_id = Some("file-2".to_owned());
    refreshed.file_reference = Some(b"ref-2".to_vec());
    tx.upsert_attachment(&refreshed).expect("refresh");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let stored = read.attachment(&key).expect("attachment").expect("some");
    assert_eq!(stored.facts, refreshed);
    assert_eq!(stored.blob_hash, Some(hash), "refresh must keep the link");
    assert_eq!(stored.last_verified_at_ms, Some(2_000));
    let blob = read
        .blob(scope().account, &hash)
        .expect("blob")
        .expect("some");
    assert_eq!(blob.first_seen_at_ms, 2_000);
    assert_eq!(blob.size, 2_048);
    assert_eq!(
        read.attachments_of_message(&key.message)
            .expect("list")
            .len(),
        1
    );
    assert_eq!(
        read.attachments_referencing_blob(scope().account, &hash)
            .expect("refs"),
        vec![key]
    );
}
