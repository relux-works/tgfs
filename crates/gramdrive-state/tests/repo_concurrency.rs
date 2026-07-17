//! Concurrent readers and writers over one WAL file
//! (TASK-260715-1opnb2; `.spec/architecture.md`).
//!
//! On Apple platforms the app and the File Provider extension are separate
//! *processes* over one database file. SQLite's locking is file-based, so
//! two connections in one test process exercise exactly the primitives two
//! processes would: WAL snapshot reads that never block the writer, an
//! IMMEDIATE write lock serializing writers through the busy handler, and
//! the SYNC-022 invariant that a reader can never observe a cursor ahead
//! of the state it seals.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Barrier;

use common::{TempDb, account_record, chat_record, revision, scope};
use gramdrive_state::StateStore;
use gramdrive_state::model::cursor::ChangeCursor;
use gramdrive_state::model::identity::MessageId;
use gramdrive_state::model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::repo::{ItemAvailability, ItemRecord, MessageChange, TransferId};

const CHAT: i64 = 100;
const STREAM: &str = "changes";

/// Opens the shared file and seeds the scaffold account and chat.
fn seeded(db: &TempDb) -> StateStore {
    let mut store = StateStore::open(&db.path).expect("open");
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&account_record()).expect("account");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    tx.commit().expect("commit");
    store
}

#[test]
fn a_read_snapshot_is_stable_while_the_other_connection_commits() {
    let db = TempDb::new();
    let mut writer = seeded(&db);
    let mut reader = StateStore::open(&db.path).expect("open reader");
    let chat = common::chat_key(CHAT);

    // The reader pins its snapshot before the writer commits.
    let read = reader.read_txn().expect("read txn");
    assert_eq!(
        read.messages_after(&chat, MessageId(0), 100)
            .expect("messages")
            .len(),
        0
    );

    // A whole write transaction begins, writes, and commits on the other
    // connection while the read snapshot stays open — WAL never blocks the
    // writer on a reader.
    let tx = writer.write_txn().expect("write txn");
    tx.apply_message_changes(&chat, &[MessageChange::Observed(revision(1, 1_000))])
        .expect("apply");
    tx.commit().expect("commit");

    // Same snapshot: still empty. That is snapshot isolation, not staleness.
    assert_eq!(
        read.messages_after(&chat, MessageId(0), 100)
            .expect("messages")
            .len(),
        0
    );
    drop(read);

    // A fresh snapshot sees the commit.
    let read = reader.read_txn().expect("read txn");
    assert_eq!(
        read.messages_after(&chat, MessageId(0), 100)
            .expect("messages")
            .len(),
        1
    );
}

#[test]
fn two_connections_never_claim_the_same_transfer() {
    let db = TempDb::new();
    let mut store = seeded(&db);

    // Two queued transfers on two items.
    let tx = store.write_txn().expect("write");
    let root = common::account_root_id();
    tx.upsert_item(&ItemRecord {
        id: root.clone(),
        parent: None,
        display_name: "Root".to_owned(),
        safe_name: "Root".to_owned(),
        metadata_version: MetadataVersion::new("m1").expect("version"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("root");
    for (name, key) in [
        ("a.ndjson", common::year_dir_key(CHAT, 2025)),
        ("b.ndjson", common::year_dir_key(CHAT, 2026)),
    ] {
        // Year dirs stand in for any two distinct items; files would need
        // more scaffolding and prove nothing extra here.
        let id = gramdrive_state::model::identity::ItemKey::Canonical(key).id();
        tx.upsert_item(&ItemRecord {
            id: id.clone(),
            parent: Some(root.clone()),
            display_name: name.to_owned(),
            safe_name: name.to_owned(),
            metadata_version: MetadataVersion::new("m1").expect("version"),
            content: None,
            availability: ItemAvailability::Fetchable,
            created_at_ms: None,
            modified_at_ms: None,
            deleted_at_ms: None,
        })
        .expect("item");
        tx.enqueue_transfer(
            &id,
            &ContentVersion::new("v1").expect("version"),
            &[],
            0,
            1_000,
        )
        .expect("enqueue");
    }
    tx.commit().expect("commit");
    drop(store);

    // Two "processes" race to claim work. IMMEDIATE serializes them; the
    // busy handler makes the loser wait instead of failing.
    let barrier = Barrier::new(2);
    let claims: Vec<Option<TransferId>> = std::thread::scope(|threads| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let barrier = &barrier;
                let path = &db.path;
                threads.spawn(move || {
                    let mut store = StateStore::open(path).expect("open");
                    barrier.wait();
                    let tx = store.write_txn().expect("write txn");
                    let claimed = tx
                        .claim_next_transfer(2_000)
                        .expect("claim")
                        .map(|record| record.id);
                    tx.commit().expect("commit");
                    claimed
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("thread"))
            .collect()
    });

    let mut ids: Vec<TransferId> = claims.into_iter().flatten().collect();
    assert_eq!(ids.len(), 2, "both claims must land");
    ids.sort_by_key(|id| id.0);
    ids.dedup();
    assert_eq!(ids.len(), 2, "one transfer must never be claimed twice");
}

#[test]
fn a_reader_never_observes_a_cursor_ahead_of_its_state() {
    const BATCHES: u64 = 20;
    const PER_BATCH: u64 = 5;

    let db = TempDb::new();
    let mut reader = seeded(&db);
    let chat = common::chat_key(CHAT);

    let writer = {
        let path = db.path.clone();
        std::thread::spawn(move || {
            let mut store = StateStore::open(path).expect("open writer");
            let chat = common::chat_key(CHAT);
            for batch in 1..=BATCHES {
                // One batch and the cursor that seals it, in one
                // transaction (SYNC-022).
                let tx = store.write_txn().expect("write txn");
                let changes: Vec<MessageChange> = (1..=PER_BATCH)
                    .map(|m| {
                        let id = i64::try_from((batch - 1) * PER_BATCH + m).expect("fits");
                        MessageChange::Observed(revision(id, id * 1_000))
                    })
                    .collect();
                tx.apply_message_changes(&chat, &changes).expect("apply");
                let cursor =
                    ChangeCursor::new(scope(), batch.to_be_bytes().to_vec()).expect("cursor");
                tx.put_cursor(STREAM, &cursor, 1_000).expect("cursor");
                tx.commit().expect("commit");
            }
        })
    };

    // Concurrently, read (cursor, state) under one snapshot each time and
    // hold the writer to the invariant: a cursor at batch k proves at
    // least k * PER_BATCH messages — a cursor ahead of its state would be
    // exactly the torn commit SYNC-022 forbids.
    let mut observed_final = false;
    for _ in 0..1_000_000 {
        let read = reader.read_txn().expect("read txn");
        let cursor = read.cursor(scope(), STREAM).expect("cursor");
        let messages = read
            .messages_after(&chat, MessageId(0), 10_000)
            .expect("messages")
            .len() as u64;
        drop(read);
        if let Some(cursor) = cursor {
            let payload: [u8; 8] = cursor.payload().try_into().expect("payload");
            let batch = u64::from_be_bytes(payload);
            assert!(
                messages >= batch * PER_BATCH,
                "cursor at batch {batch} but only {messages} messages visible"
            );
            if batch == BATCHES {
                observed_final = true;
                break;
            }
        }
    }
    writer.join().expect("writer");
    assert!(
        observed_final,
        "the reader must eventually see the last batch"
    );

    // And after the dust settles, the state matches the final cursor.
    let read = reader.read_txn().expect("read txn");
    assert_eq!(
        read.messages_after(&chat, MessageId(0), 10_000)
            .expect("messages")
            .len() as u64,
        BATCHES * PER_BATCH
    );
}
