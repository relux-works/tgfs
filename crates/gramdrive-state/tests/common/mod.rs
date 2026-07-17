//! Shared helpers for the schema test suites: identity minting through the
//! real model vocabulary (the schema stores canonical `ItemId` bytes,
//! DEC-008) and minimal row constructors for the tables most tests need as
//! scaffolding.
//!
//! Compiled once per test binary, and each binary uses its own subset —
//! dead-code warnings here would only report that fact.
#![allow(dead_code)]

use gramdrive_state::StateStore;
use gramdrive_state::model::identity::{
    AccountId, AccountKey, AccountScope, AttachmentIndex, AttachmentKey, CanonicalKey, ChatId,
    ChatKey, ChatListKey, ChatListKind, DocFormat, DocPartition, GeneratedDocKey, ItemId, ItemKey,
    MediaDirKey, MessageId, MessageKey, NamespaceVersion, OrderDocKey, SchemaFamily, YearDirKey,
};
use rusqlite::{Connection, params};

/// The account id every test scaffold uses.
pub(crate) const ACCOUNT_ID: i64 = 7;
/// The namespace epoch every test scaffold uses.
pub(crate) const NAMESPACE: u32 = 1;

pub(crate) fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(ACCOUNT_ID),
        },
        namespace_version: NamespaceVersion(NAMESPACE),
    }
}

pub(crate) fn chat_key(chat: i64) -> ChatKey {
    ChatKey {
        scope: scope(),
        chat_id: ChatId(chat),
    }
}

pub(crate) fn account_root_id() -> ItemId {
    ItemKey::Canonical(CanonicalKey::Account(scope().account)).id()
}

pub(crate) fn chat_list_id(kind: ChatListKind) -> ItemId {
    ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
        scope: scope(),
        kind,
    }))
    .id()
}

pub(crate) fn canonical_chat_id(chat: i64) -> ItemId {
    ItemKey::Canonical(CanonicalKey::Chat(chat_key(chat))).id()
}

pub(crate) fn appearance_id(view: ChatListKind, canonical: CanonicalKey) -> ItemId {
    ItemKey::Appearance(gramdrive_state::model::identity::AppearanceKey {
        view,
        item: canonical,
    })
    .id()
}

pub(crate) fn chat_canonical_key(chat: i64) -> CanonicalKey {
    CanonicalKey::Chat(chat_key(chat))
}

pub(crate) fn year_dir_key(chat: i64, year: u16) -> CanonicalKey {
    CanonicalKey::YearDir(YearDirKey {
        chat: chat_key(chat),
        year,
    })
}

pub(crate) fn media_dir_key(chat: i64, year: u16) -> CanonicalKey {
    CanonicalKey::MediaDir(MediaDirKey {
        chat: chat_key(chat),
        year,
    })
}

pub(crate) fn attachment_key(chat: i64, message: i64, index: u32) -> CanonicalKey {
    CanonicalKey::Attachment(AttachmentKey {
        message: MessageKey {
            chat: chat_key(chat),
            message_id: MessageId(message),
        },
        index: AttachmentIndex(index),
    })
}

pub(crate) fn doc_key(chat: i64, partition: DocPartition, format: DocFormat) -> CanonicalKey {
    CanonicalKey::GeneratedDoc(GeneratedDocKey {
        chat: chat_key(chat),
        partition,
        format,
        schema_family: SchemaFamily(1),
    })
}

pub(crate) fn order_doc_key(kind: ChatListKind) -> CanonicalKey {
    CanonicalKey::OrderDoc(OrderDocKey {
        list: ChatListKey {
            scope: scope(),
            kind,
        },
        schema_family: SchemaFamily(1),
    })
}

/// An in-memory store with the scaffold rows most suites need: the account
/// and its root item.
pub(crate) fn store_with_account() -> StateStore {
    let store = StateStore::open_in_memory().expect("open in-memory store");
    insert_account(store.connection());
    insert_root_item(store.connection());
    store
}

pub(crate) fn insert_account(conn: &Connection) {
    conn.execute(
        "INSERT INTO accounts (account_id, source_kind, display_name, auth_state,
                               namespace_version, created_at_ms, updated_at_ms)
         VALUES (?1, 'local_tdlib', 'Test Account', 'authorized', ?2, 1000, 1000)",
        params![ACCOUNT_ID, NAMESPACE],
    )
    .expect("insert account");
}

pub(crate) fn insert_root_item(conn: &Connection) {
    conn.execute(
        "INSERT INTO items (item_id, account_id, namespace_version, kind, parent_item_id,
                            display_name, safe_name, is_directory, metadata_version)
         VALUES (?1, ?2, ?3, 'account', NULL, 'Test Account', 'Test Account', 1, 'm1')",
        params![account_root_id().as_bytes(), ACCOUNT_ID, NAMESPACE],
    )
    .expect("insert account root item");
}

pub(crate) fn insert_chat(conn: &Connection, chat: i64) {
    conn.execute(
        "INSERT INTO chats (account_id, namespace_version, chat_id, chat_type, title,
                            metadata_version)
         VALUES (?1, ?2, ?3, 'private', 'Chat', 'm1')",
        params![ACCOUNT_ID, NAMESPACE, chat],
    )
    .expect("insert chat");
}

/// Appends an 'observed' event and returns its sequence number.
pub(crate) fn insert_observed_event(conn: &Connection, chat: i64, message: i64) -> i64 {
    conn.execute(
        "INSERT INTO message_events (account_id, namespace_version, chat_id, message_id,
                                     event_kind, observed_at_ms, payload_schema, payload)
         VALUES (?1, ?2, ?3, ?4, 'observed', 1000, 1, ?5)",
        params![ACCOUNT_ID, NAMESPACE, chat, message, b"payload".as_slice()],
    )
    .expect("insert event");
    conn.last_insert_rowid()
}

pub(crate) fn insert_message(conn: &Connection, chat: i64, message: i64, latest_event_seq: i64) {
    conn.execute(
        "INSERT INTO messages (account_id, namespace_version, chat_id, message_id,
                               sent_at_ms, latest_event_seq)
         VALUES (?1, ?2, ?3, ?4, 1000, ?5)",
        params![ACCOUNT_ID, NAMESPACE, chat, message, latest_event_seq],
    )
    .expect("insert message");
}

/// Asserts a statement fails and that SQLite's complaint mentions `needle` —
/// the constraint the schema is expected to enforce.
pub(crate) fn expect_rejected(result: Result<usize, rusqlite::Error>, needle: &str) {
    match result {
        Ok(_) => panic!("statement succeeded; expected a violation mentioning {needle:?}"),
        Err(error) => {
            let text = error.to_string();
            assert!(
                text.contains(needle),
                "expected a violation mentioning {needle:?}, got: {text}"
            );
        }
    }
}
