//! Dev-only seeder for the shared-state smoke (TASK-260715-gnsa2s;
//! `.scripts/smoke/run_shared_state_smoke.py`).
//!
//! Plays the *coordinator* process — the engine host that writes durable
//! state in-process through the Rust crates, which is the product's real
//! write path (the FFI exposes no writes; `src/shared_state.rs` § Writes).
//! The smoke's Swift processes then read the same container through the
//! packaged bindings and must observe exactly what this printed.
//!
//! Usage: `cargo run -p gramdrive-ffi --example shared_state_seed -- <data_root> seed|mutate|generated-initial`
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
    AccountId, AccountKey, AccountScope, AppearanceKey, AttachmentIndex, AttachmentKey,
    CanonicalKey, ChatId, ChatKey, ChatListKind, DocFormat, DocPartition, GeneratedDocKey, ItemId,
    ItemKey, MessageId, MessageKey, NamespaceVersion, SchemaFamily,
};
use gramdrive_model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::StateStore;
use gramdrive_state::repo::{
    AccountRecord, CacheEntryRecord, CacheKind, CacheVerification, FileFacts, ItemAvailability,
    ItemRecord, RetentionMode, SourceKind,
};
use std::fs;
use std::path::Path;

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

fn generated_id(chat_id: i64, partition: DocPartition, format: DocFormat) -> ItemId {
    let chat = ChatKey {
        scope: scope(),
        chat_id: ChatId(chat_id),
    };
    ItemKey::Appearance(AppearanceKey {
        view: ChatListKind::Main,
        item: CanonicalKey::GeneratedDoc(GeneratedDocKey {
            chat,
            partition,
            format,
            schema_family: SchemaFamily(1),
        }),
    })
    .id()
}

fn directory(id: ItemId, parent: Option<ItemId>, name: &str) -> ItemRecord {
    ItemRecord {
        aggregate_size: None,
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
        display_timezone: "UTC".to_owned(),
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
        aggregate_size: None,
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

struct GeneratedFixture {
    markdown: ItemId,
    ndjson: ItemId,
    chat_json: ItemId,
    unrelated: ItemId,
    deleted: ItemId,
    attachment: ItemId,
    chat_parent: ItemId,
    unrelated_parent: ItemId,
}

fn generated_file(
    id: ItemId,
    parent: ItemId,
    name: &str,
    mime_type: &str,
    bytes: &[u8],
    content_version: &str,
    deleted_at_ms: Option<i64>,
) -> ItemRecord {
    ItemRecord {
        aggregate_size: None,
        id,
        parent: Some(parent),
        display_name: name.to_owned(),
        safe_name: name.to_owned(),
        metadata_version: MetadataVersion::new(format!("metadata-{content_version}"))
            .expect("metadata version"),
        content: Some(FileFacts {
            mime_type: Some(mime_type.to_owned()),
            logical_size: Some(bytes.len() as u64),
            content_version: Some(ContentVersion::new(content_version).expect("content version")),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: Some(1_000),
        modified_at_ms: Some(2_000),
        deleted_at_ms,
    }
}

fn seed_generated_initial(store: &mut StateStore, cache_dir: &Path) -> GeneratedFixture {
    const MARKDOWN: &[u8] = b"# synthetic g2\n";
    const NDJSON: &[u8] = b"{\"synthetic\":2}\n";
    const CHAT_JSON: &[u8] = b"{\"generation\":2}\n";
    const UNRELATED: &[u8] = b"unrelated-current\n";
    const DELETED: &[u8] = b"deleted-current\n";

    let chat_parent = chat_dir_id();
    let unrelated_parent = ItemKey::Canonical(CanonicalKey::Chat(ChatKey {
        scope: scope(),
        chat_id: ChatId(200),
    }))
    .id();
    let markdown = generated_id(
        100,
        DocPartition::Month {
            year: 2026,
            month: 8,
        },
        DocFormat::Markdown,
    );
    let ndjson = generated_id(
        100,
        DocPartition::Month {
            year: 2026,
            month: 8,
        },
        DocFormat::Ndjson,
    );
    let chat_json = generated_id(100, DocPartition::Chat, DocFormat::Json);
    let unrelated = generated_id(200, DocPartition::Chat, DocFormat::Json);
    let deleted = generated_id(100, DocPartition::Year { year: 2025 }, DocFormat::Markdown);
    let attachment = file_id();

    let generated_root = cache_dir
        .join("generated")
        .join("account-7")
        .join("build154-fixture")
        .join("generation-2");
    fs::create_dir_all(&generated_root).expect("generated cache directory");

    let documents = [
        (
            &markdown,
            "Messages.md",
            "text/markdown",
            MARKDOWN,
            "markdown-v2",
        ),
        (
            &ndjson,
            "Messages.ndjson",
            "application/x-ndjson",
            NDJSON,
            "ndjson-v2",
        ),
        (
            &chat_json,
            ".chat.json",
            "application/json",
            CHAT_JSON,
            "chat-json-v2",
        ),
        (
            &unrelated,
            "other.json",
            "application/json",
            UNRELATED,
            "unrelated-v2",
        ),
    ];

    let txn = store.write_txn().expect("generated fixture write");
    txn.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Synthetic Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        display_timezone: "UTC".to_owned(),
        retention_mode: RetentionMode::Mirror,
        archive_mode: false,
        secret_ref: None,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
    })
    .expect("account");
    txn.upsert_item(&directory(root_id(), None, "Synthetic Account"))
        .expect("root");
    txn.upsert_item(&directory(
        chat_parent.clone(),
        Some(root_id()),
        "Synthetic Chat",
    ))
    .expect("chat parent");
    txn.upsert_item(&directory(
        unrelated_parent.clone(),
        Some(root_id()),
        "Unrelated Chat",
    ))
    .expect("unrelated parent");

    for (id, name, mime_type, bytes, version) in documents {
        let parent = if id == &unrelated {
            unrelated_parent.clone()
        } else {
            chat_parent.clone()
        };
        let path = generated_root.join(name);
        fs::write(&path, bytes).expect("generated cache bytes");
        txn.upsert_item(&generated_file(
            id.clone(),
            parent,
            name,
            mime_type,
            bytes,
            version,
            None,
        ))
        .expect("generated item");
        txn.upsert_cache_entry(&CacheEntryRecord {
            item: id.clone(),
            account: scope().account,
            content_version: ContentVersion::new(version).expect("cache version"),
            kind: CacheKind::GeneratedDoc,
            size: bytes.len() as u64,
            blob_hash: None,
            verification: CacheVerification::Verified,
            pin: None,
            last_access_at_ms: 2_000,
            materialized_at_ms: 2_000,
            materialization_ref: Some(path.to_string_lossy().into_owned()),
        })
        .expect("generated cache row");
    }

    let deleted_path = generated_root.join("Deleted.md");
    fs::write(&deleted_path, DELETED).expect("deleted cache bytes");
    txn.upsert_item(&generated_file(
        deleted.clone(),
        chat_parent.clone(),
        "Deleted.md",
        "text/markdown",
        DELETED,
        "deleted-v2",
        Some(3_000),
    ))
    .expect("deleted generated item");
    txn.upsert_cache_entry(&CacheEntryRecord {
        item: deleted.clone(),
        account: scope().account,
        content_version: ContentVersion::new("deleted-v2").expect("deleted version"),
        kind: CacheKind::GeneratedDoc,
        size: DELETED.len() as u64,
        blob_hash: None,
        verification: CacheVerification::Verified,
        pin: None,
        last_access_at_ms: 2_000,
        materialized_at_ms: 2_000,
        materialization_ref: Some(deleted_path.to_string_lossy().into_owned()),
    })
    .expect("deleted cache row");
    txn.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: attachment.clone(),
        parent: Some(chat_parent.clone()),
        display_name: "attachment.bin".to_owned(),
        safe_name: "attachment.bin".to_owned(),
        metadata_version: MetadataVersion::new("attachment-m1").expect("attachment metadata"),
        content: Some(FileFacts {
            mime_type: Some("application/octet-stream".to_owned()),
            logical_size: Some(4),
            content_version: Some(
                ContentVersion::new("attachment-v1").expect("attachment version"),
            ),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: Some(1_000),
        modified_at_ms: Some(2_000),
        deleted_at_ms: None,
    })
    .expect("attachment");
    txn.commit().expect("generated fixture commit");

    // Reproduce the installed upgrade boundary: current rows/cache exist,
    // but no item-change row was ever issued in this database life.
    store
        .connection()
        .execute("DELETE FROM item_changes", [])
        .expect("clear pre-journal rows");
    store
        .connection()
        .execute(
            "DELETE FROM sqlite_sequence WHERE name = 'item_changes'",
            [],
        )
        .expect("reset pre-journal sequence");

    GeneratedFixture {
        markdown,
        ndjson,
        chat_json,
        unrelated,
        deleted,
        attachment,
        chat_parent,
        unrelated_parent,
    }
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
    if phase == "generated-initial" {
        let fixture = seed_generated_initial(&mut store, Path::new(&layout.cache_dir));
        let journal = shared.change_journal_state().expect("journal state");
        println!("phase={phase}");
        println!("root={}", root_id().text());
        println!("chat_parent={}", fixture.chat_parent.text());
        println!("unrelated_parent={}", fixture.unrelated_parent.text());
        println!("markdown={}", fixture.markdown.text());
        println!("ndjson={}", fixture.ndjson.text());
        println!("chat_json={}", fixture.chat_json.text());
        println!("unrelated={}", fixture.unrelated.text());
        println!("deleted={}", fixture.deleted.text());
        println!("attachment={}", fixture.attachment.text());
        println!("journal_latest_sequence={}", journal.latest_sequence);
        return;
    }
    match phase.as_str() {
        "seed" => seed(&mut store),
        "mutate" => mutate(&mut store),
        other => panic!("unknown phase '{other}' (expected seed|mutate|generated-initial)"),
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
