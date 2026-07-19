//! The item change journal (TASK-260715-rhcnhc; PLAT-MAC-004): durable
//! change enumeration for provider sync anchors. What is proven here is the
//! contract a File Provider host leans on — sequences only ever grow, an
//! item occupies one coalesced row at its newest sequence, identical
//! re-pushes advance nothing, pages compose exactly, and the journal
//! identity distinguishes one database life from another.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use common::{account_record, attachment_key, canonical_chat_id, scope};
use gramdrive_state::StateStore;
use gramdrive_state::model::identity::{
    AccountId, AccountKey, AccountScope, CanonicalKey, ItemId, ItemKey, NamespaceVersion,
};
use gramdrive_state::model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::repo::{
    AccountRecord, FileFacts, ItemAvailability, ItemChangeRecord, ItemRecord,
};

const CHAT: i64 = 100;

fn account() -> AccountKey {
    scope().account
}

fn version(text: &str) -> MetadataVersion {
    MetadataVersion::new(text).expect("valid version")
}

fn content_version(text: &str) -> ContentVersion {
    ContentVersion::new(text).expect("valid version")
}

fn root_id() -> ItemId {
    ItemKey::Canonical(CanonicalKey::Account(account())).id()
}

fn chat_id() -> ItemId {
    canonical_chat_id(CHAT)
}

fn file_id() -> ItemId {
    ItemKey::Canonical(attachment_key(CHAT, 1, 0)).id()
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

fn file_item(id: &ItemId, parent: &ItemId, safe_name: &str) -> ItemRecord {
    ItemRecord {
        id: id.clone(),
        parent: Some(parent.clone()),
        display_name: safe_name.to_owned(),
        safe_name: safe_name.to_owned(),
        metadata_version: version("m1"),
        content: Some(FileFacts {
            mime_type: Some("image/jpeg".to_owned()),
            logical_size: Some(2_048),
            content_version: Some(content_version("c1")),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: Some(1_000),
        modified_at_ms: Some(1_000),
        deleted_at_ms: None,
    }
}

fn store() -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    let tx = store.write_txn().expect("write txn");
    tx.upsert_account(&account_record()).expect("account");
    tx.commit().expect("commit");
    store
}

/// Seeds the three-node tree through the real write paths and returns the
/// store: root, one chat directory, one attachment file.
fn seeded_store() -> StateStore {
    let mut store = store();
    let tx = store.write_txn().expect("write txn");
    tx.upsert_item(&dir_item(&root_id(), None, "Test Account"))
        .expect("root");
    tx.upsert_item(&dir_item(&chat_id(), Some(&root_id()), "Chat 100"))
        .expect("chat");
    tx.upsert_item(&file_item(&file_id(), &chat_id(), "photo.jpg"))
        .expect("file");
    tx.commit().expect("commit");
    store
}

fn changes_since(store: &mut StateStore, after: i64, limit: u32) -> Vec<ItemChangeRecord> {
    let txn = store.read_txn().expect("read txn");
    txn.item_changes_since(account(), after, limit)
        .expect("changes")
}

fn latest(store: &mut StateStore) -> i64 {
    let txn = store.read_txn().expect("read txn");
    txn.change_journal_state()
        .expect("journal state")
        .latest_sequence
}

// ---------------------------------------------------------------------------
// Identity and the empty journal
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_journal_has_an_identity_and_no_changes() {
    let mut store = store();
    let txn = store.read_txn().expect("read txn");
    let state = txn.change_journal_state().expect("journal state");
    assert_eq!(
        state.instance_id.len(),
        32,
        "a 16-byte random identity in lowercase hex"
    );
    assert_eq!(state.latest_sequence, 0);
    assert_eq!(
        txn.item_changes_since(account(), 0, 100).expect("changes"),
        Vec::new()
    );
}

#[test]
fn the_journal_identity_is_durable_across_reopens() {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "gramdrive-item-changes-test-{}-{n}.sqlite3",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);

    let first = {
        let mut store = StateStore::open(&path).expect("create");
        let txn = store.read_txn().expect("read txn");
        txn.change_journal_state().expect("state").instance_id
    };
    let second = {
        let mut store = StateStore::open(&path).expect("reopen");
        let txn = store.read_txn().expect("read txn");
        txn.change_journal_state().expect("state").instance_id
    };
    assert_eq!(
        first, second,
        "one database life is one sequence space, however often it reopens"
    );

    for suffix in ["", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let _ = std::fs::remove_file(PathBuf::from(name));
    }
}

// ---------------------------------------------------------------------------
// What advances the journal, and what must not
// ---------------------------------------------------------------------------

#[test]
fn writes_journal_in_order_and_carry_current_state() {
    let mut store = seeded_store();

    let changes = changes_since(&mut store, 0, 100);
    assert_eq!(
        changes
            .iter()
            .map(|change| change.item.id.clone())
            .collect::<Vec<_>>(),
        vec![root_id(), chat_id(), file_id()],
        "sequence order is write order"
    );
    let sequences: Vec<i64> = changes.iter().map(|change| change.sequence).collect();
    assert!(
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        "sequences strictly increase: {sequences:?}"
    );
    assert_eq!(latest(&mut store), sequences[2]);
    assert_eq!(
        changes[2]
            .item
            .content
            .as_ref()
            .expect("facts")
            .logical_size,
        Some(2_048),
        "a change carries the item's full current metadata"
    );
}

#[test]
fn an_identical_reupsert_is_journal_quiet() {
    let mut store = seeded_store();
    let before = latest(&mut store);

    // The engine re-baselining after a restart: same rows, byte for byte.
    let tx = store.write_txn().expect("write txn");
    tx.upsert_item(&dir_item(&root_id(), None, "Test Account"))
        .expect("root again");
    tx.upsert_item(&dir_item(&chat_id(), Some(&root_id()), "Chat 100"))
        .expect("chat again");
    tx.upsert_item(&file_item(&file_id(), &chat_id(), "photo.jpg"))
        .expect("file again");
    tx.commit().expect("commit");

    assert_eq!(
        latest(&mut store),
        before,
        "an identical re-push must not replay the tree at the provider boundary"
    );
    assert_eq!(changes_since(&mut store, before, 100), Vec::new());
}

#[test]
fn a_rename_coalesces_to_the_items_newest_sequence() {
    let mut store = seeded_store();
    let before = latest(&mut store);

    let mut renamed = dir_item(&chat_id(), Some(&root_id()), "Chat Renamed");
    renamed.metadata_version = version("m2");
    let tx = store.write_txn().expect("write txn");
    tx.upsert_item(&renamed).expect("rename");
    tx.commit().expect("commit");

    // From the beginning of the journal the chat appears exactly once — at
    // its newest sequence, past everything issued before the rename.
    let changes = changes_since(&mut store, 0, 100);
    let chat_rows: Vec<&ItemChangeRecord> = changes
        .iter()
        .filter(|change| change.item.id == chat_id())
        .collect();
    assert_eq!(chat_rows.len(), 1, "coalesced: one row per item");
    assert!(chat_rows[0].sequence > before);
    assert_eq!(chat_rows[0].item.safe_name, "Chat Renamed");

    // An anchor taken before the rename sees exactly the rename.
    let after_anchor = changes_since(&mut store, before, 100);
    assert_eq!(after_anchor.len(), 1);
    assert_eq!(after_anchor[0].item.id, chat_id());
}

#[test]
fn content_updates_journal_once_and_a_noop_republish_stays_quiet() {
    let mut store = seeded_store();
    let before = latest(&mut store);

    let facts = FileFacts {
        mime_type: Some("image/jpeg".to_owned()),
        logical_size: Some(4_096),
        content_version: Some(content_version("c2")),
    };
    let tx = store.write_txn().expect("write txn");
    tx.update_item_content(
        &file_id(),
        Some(&content_version("c1")),
        &facts,
        &version("m2"),
        2_000,
    )
    .expect("publish");
    tx.commit().expect("commit");

    let changes = changes_since(&mut store, before, 100);
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0]
            .item
            .content
            .as_ref()
            .expect("facts")
            .content_version,
        Some(content_version("c2"))
    );

    // Republishing the identical facts under the identical versions is
    // provider-invisible and must stay journal-quiet.
    let after_publish = latest(&mut store);
    let tx = store.write_txn().expect("write txn");
    tx.update_item_content(
        &file_id(),
        Some(&content_version("c2")),
        &facts,
        &version("m2"),
        2_000,
    )
    .expect("republish");
    tx.commit().expect("commit");
    assert_eq!(latest(&mut store), after_publish);
}

#[test]
fn a_tombstone_journals_its_transition_exactly_once() {
    let mut store = seeded_store();
    let before = latest(&mut store);

    let tx = store.write_txn().expect("write txn");
    tx.tombstone_item(&file_id(), 3_000, &version("m2"))
        .expect("tombstone");
    tx.commit().expect("commit");

    let changes = changes_since(&mut store, before, 100);
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0].item.deleted_at_ms,
        Some(3_000),
        "a deletion is a change"
    );
    let after_tombstone = latest(&mut store);

    // POL-3 idempotence: re-observing the deletion changes nothing.
    let tx = store.write_txn().expect("write txn");
    tx.tombstone_item(&file_id(), 9_000, &version("m3"))
        .expect("tombstone again");
    tx.commit().expect("commit");
    assert_eq!(latest(&mut store), after_tombstone);
}

// ---------------------------------------------------------------------------
// Paging and account scoping
// ---------------------------------------------------------------------------

#[test]
fn pages_compose_exactly_and_scope_to_their_account() {
    let mut store = seeded_store();

    // A second account with its own root, interleaved into the journal.
    let other = AccountKey {
        account_id: AccountId(3),
    };
    let other_scope = AccountScope {
        account: other,
        namespace_version: NamespaceVersion(1),
    };
    let other_root = ItemKey::Canonical(CanonicalKey::Account(other)).id();
    let tx = store.write_txn().expect("write txn");
    tx.upsert_account(&AccountRecord {
        account: other,
        display_name: "Second Account".to_owned(),
        namespace_version: other_scope.namespace_version,
        ..account_record()
    })
    .expect("second account");
    tx.upsert_item(&dir_item(&other_root, None, "Second Account"))
        .expect("second root");
    tx.commit().expect("commit");

    // Page the first account's journal one change at a time; the pages must
    // compose into exactly its three items, in one strictly increasing
    // sequence walk, never meeting the other account's row.
    let mut anchor = 0;
    let mut walked = Vec::new();
    loop {
        let page = changes_since(&mut store, anchor, 1);
        match page.as_slice() {
            [] => break,
            [only] => {
                assert!(only.sequence > anchor);
                anchor = only.sequence;
                walked.push(only.item.id.clone());
            }
            more => panic!("limit 1 returned {} rows", more.len()),
        }
    }
    assert_eq!(walked, vec![root_id(), chat_id(), file_id()]);

    let other_changes = {
        let txn = store.read_txn().expect("read txn");
        txn.item_changes_since(other, 0, 100).expect("changes")
    };
    assert_eq!(other_changes.len(), 1);
    assert_eq!(other_changes[0].item.id, other_root);
}

#[test]
fn latest_sequence_never_rewinds_even_when_rows_cascade_away() {
    let mut store = seeded_store();
    let before = latest(&mut store);
    assert!(before > 0);

    // Account removal sweeps its items; the cascade takes the journal rows
    // with them. Issued sequences stay issued — an anchor from another
    // account must never see the high-water mark move backwards.
    store
        .connection()
        .execute("DELETE FROM accounts WHERE account_id = 7", [])
        .expect("cascade");
    let remaining: i64 = store
        .connection()
        .query_row("SELECT count(*) FROM item_changes", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        remaining, 0,
        "journal rows live exactly as long as their items"
    );
    assert_eq!(latest(&mut store), before);
}
