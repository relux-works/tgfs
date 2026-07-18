//! Dev-only seeder for the shared-state smoke (TASK-260715-gnsa2s;
//! `.scripts/smoke/run_shared_state_smoke.py`).
//!
//! Plays the *coordinator* process — the engine host that writes durable
//! state in-process through the Rust crates, which is the product's real
//! write path (the FFI exposes no writes; `src/shared_state.rs` § Writes).
//! The smoke's Swift processes then read the same container through the
//! packaged bindings and must observe exactly what this printed.
//!
//! Usage: `cargo run -p gramdrive-ffi --example shared_state_seed -- <data_root> seed|mutate`
//!
//! `seed` creates the layout and a small provider tree; `mutate` bumps the
//! seeded file's content version — the foreign commit the smoke's watcher
//! process must detect via `data_version`. Output is `key=value` lines the
//! smoke parses.

// A dev tool run by hand or by the smoke script: stdout is its interface,
// and failing loudly on a broken fixture is correct. It ships nowhere.
#![allow(clippy::print_stdout, clippy::expect_used, clippy::panic)]

use gramdrive_ffi::shared_state::{SharedStateStore, StateRole, shared_state_layout};
use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, AttachmentIndex, AttachmentKey, CanonicalKey, ChatId,
    ChatKey, ItemId, ItemKey, MessageId, MessageKey, NamespaceVersion,
};
use gramdrive_model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::StateStore;
use gramdrive_state::repo::{
    AccountRecord, FileFacts, ItemAvailability, ItemRecord, RetentionMode, SourceKind,
};

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(7),
        },
        namespace_version: NamespaceVersion(1),
    }
}

fn chat_key() -> ChatKey {
    ChatKey {
        scope: scope(),
        chat_id: ChatId(100),
    }
}

fn root_id() -> ItemId {
    ItemKey::Canonical(CanonicalKey::Account(scope().account)).id()
}

fn chat_dir_id() -> ItemId {
    ItemKey::Canonical(CanonicalKey::Chat(chat_key())).id()
}

fn file_id() -> ItemId {
    ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
        message: MessageKey {
            chat: chat_key(),
            message_id: MessageId(1),
        },
        index: AttachmentIndex(0),
    }))
    .id()
}

fn directory(id: ItemId, parent: Option<ItemId>, name: &str) -> ItemRecord {
    ItemRecord {
        id,
        parent,
        display_name: name.to_owned(),
        safe_name: name.to_owned(),
        metadata_version: MetadataVersion::new("m1").expect("version"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: Some(1_000),
        modified_at_ms: Some(1_000),
        deleted_at_ms: None,
    }
}

fn seed(store: &mut StateStore) {
    let txn = store.write_txn().expect("write txn");
    txn.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Smoke Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        retention_mode: RetentionMode::Mirror,
        archive_mode: false,
        secret_ref: None,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
    })
    .expect("account");
    txn.upsert_item(&directory(root_id(), None, "Smoke Account"))
        .expect("root");
    txn.upsert_item(&directory(chat_dir_id(), Some(root_id()), "Chat 100"))
        .expect("chat dir");
    txn.upsert_item(&ItemRecord {
        id: file_id(),
        parent: Some(chat_dir_id()),
        display_name: "photo.jpg".to_owned(),
        safe_name: "photo.jpg".to_owned(),
        metadata_version: MetadataVersion::new("m1").expect("version"),
        content: Some(FileFacts {
            mime_type: Some("image/jpeg".to_owned()),
            logical_size: Some(2_048),
            content_version: Some(ContentVersion::new("c1").expect("version")),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: Some(1_000),
        modified_at_ms: Some(1_500),
        deleted_at_ms: None,
    })
    .expect("file");
    txn.commit().expect("commit");
}

fn mutate(store: &mut StateStore) {
    let txn = store.write_txn().expect("write txn");
    txn.update_item_content(
        &file_id(),
        Some(&ContentVersion::new("c1").expect("version")),
        &FileFacts {
            mime_type: Some("image/jpeg".to_owned()),
            logical_size: Some(4_096),
            content_version: Some(ContentVersion::new("c2").expect("version")),
        },
        &MetadataVersion::new("m2").expect("version"),
        2_000,
    )
    .expect("bump content version");
    txn.commit().expect("commit");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let data_root = args
        .next()
        .expect("usage: shared_state_seed <data_root> seed|mutate");
    let phase = args
        .next()
        .expect("usage: shared_state_seed <data_root> seed|mutate");

    // Open through the product FFI path first: proves the coordinator open
    // creates the canonical layout (directories, WAL database, schema).
    let shared = SharedStateStore::open(data_root.clone(), StateRole::Coordinator)
        .expect("coordinator open");
    let layout = shared_state_layout(data_root).expect("layout");

    // Write the way the engine host does: in-process through the state
    // crate, over its own connection to the shared file.
    let mut store = StateStore::open(&layout.database_file).expect("writer open");
    match phase.as_str() {
        "seed" => seed(&mut store),
        "mutate" => mutate(&mut store),
        other => panic!("unknown phase '{other}' (expected seed|mutate)"),
    }

    // The coordinator handle re-reads what was just committed, so the smoke
    // compares Swift processes against Rust-observed truth, not intentions.
    let file = shared
        .item(file_id().text())
        .expect("read back")
        .expect("file exists");
    println!("phase={phase}");
    println!("root={}", root_id().text());
    println!("chat={}", chat_dir_id().text());
    println!("file={}", file_id().text());
    println!(
        "file_content_version={}",
        file.content_version.expect("content version")
    );
    println!("file_logical_size={}", file.logical_size.expect("size"));
}
