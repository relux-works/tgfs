//! Change application and cursors (TASK-260715-1opnb2; SYNC-004,
//! SYNC-021, SYNC-022): the cursor commits atomically with the state it
//! witnessed, replay is idempotent by Telegram identity, and scope
//! mismatches are explicit rejections.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use common::{account_record, chat_record, revision, scope};
use gramdrive_state::model::cursor::ChangeCursor;
use gramdrive_state::model::identity::{
    AccountId, AccountKey, AccountScope, ChatId, MessageId, NamespaceVersion, SchemaFamily,
};
use gramdrive_state::repo::{ChatSyncRecord, MessageChange, MessageEventKind, SyncWindow};
use gramdrive_state::{StateError, StateStore};

const CHAT: i64 = 100;
const STREAM: &str = "changes";

/// A store with the scaffold account and one chat, created through the
/// typed layer.
fn store() -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    let tx = store.write_txn().expect("write txn");
    tx.upsert_account(&account_record()).expect("account");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    tx.commit().expect("commit");
    store
}

fn cursor(payload: &str) -> ChangeCursor {
    ChangeCursor::new(scope(), payload.as_bytes().to_vec()).expect("cursor")
}

fn message_count(store: &mut StateStore) -> usize {
    store
        .read_txn()
        .expect("read txn")
        .messages_after(&common::chat_key(CHAT), MessageId(0), 10_000)
        .expect("messages")
        .len()
}

#[test]
fn cursor_commits_atomically_with_applied_changes() {
    let mut store = store();
    let chat = common::chat_key(CHAT);

    let tx = store.write_txn().expect("write txn");
    let changes: Vec<MessageChange> = (1..=3)
        .map(|m| MessageChange::Observed(revision(m, 1_000 * m)))
        .collect();
    let applied = tx.apply_message_changes(&chat, &changes).expect("apply");
    assert_eq!(applied.observed, 3);
    tx.put_cursor(STREAM, &cursor("pts:1"), 2_000)
        .expect("cursor");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read txn");
    let stored = read.cursor(scope(), STREAM).expect("cursor").expect("some");
    assert_eq!(stored.payload(), b"pts:1");
    drop(read);
    assert_eq!(message_count(&mut store), 3);
}

#[test]
fn a_failed_cursor_write_rolls_back_the_whole_batch() {
    let mut store = store();
    let chat = common::chat_key(CHAT);

    // Seed one committed batch with its cursor.
    let tx = store.write_txn().expect("write txn");
    tx.apply_message_changes(&chat, &[MessageChange::Observed(revision(1, 1_000))])
        .expect("apply");
    tx.put_cursor(STREAM, &cursor("pts:1"), 1_000)
        .expect("cursor");
    tx.commit().expect("commit");

    // Second batch applies, but its cursor carries a retired scope — the
    // put fails, the transaction is dropped, and *neither* the events nor
    // the cursor move (SYNC-022 atomicity, exercised through the failure
    // path).
    let tx = store.write_txn().expect("write txn");
    tx.apply_message_changes(&chat, &[MessageChange::Observed(revision(2, 2_000))])
        .expect("apply");
    let stale_scope = AccountScope {
        account: scope().account,
        namespace_version: NamespaceVersion(scope().namespace_version.0 + 1),
    };
    let stale = ChangeCursor::new(stale_scope, b"pts:2".to_vec()).expect("cursor");
    match tx.put_cursor(STREAM, &stale, 2_000) {
        Err(StateError::CursorOutOfScope { .. }) => {}
        other => panic!("expected CursorOutOfScope, got {other:?}"),
    }
    drop(tx); // rollback — the cancellation boundary

    assert_eq!(message_count(&mut store), 1, "batch must roll back whole");
    let read = store.read_txn().expect("read txn");
    let stored = read.cursor(scope(), STREAM).expect("cursor").expect("some");
    assert_eq!(stored.payload(), b"pts:1", "cursor must not advance");
}

#[test]
fn replaying_a_batch_applies_nothing() {
    let mut store = store();
    let chat = common::chat_key(CHAT);
    let changes = vec![
        MessageChange::Observed(revision(1, 1_000)),
        MessageChange::Observed(revision(2, 2_000)),
        MessageChange::Deleted {
            message_id: MessageId(2),
            observed_at_ms: 3_000,
        },
    ];

    let tx = store.write_txn().expect("write txn");
    let first = tx.apply_message_changes(&chat, &changes).expect("apply");
    assert_eq!((first.observed, first.deleted, first.skipped), (2, 1, 0));
    tx.commit().expect("commit");

    let events_before = store
        .read_txn()
        .expect("read")
        .events_after(&chat, 0, 100)
        .expect("events");
    assert_eq!(events_before.len(), 3);

    // The crash-replay path: the same batch again, verbatim (SYNC-021).
    let tx = store.write_txn().expect("write txn");
    let replay = tx.apply_message_changes(&chat, &changes).expect("replay");
    assert_eq!(
        (
            replay.observed,
            replay.edited,
            replay.deleted,
            replay.skipped
        ),
        (0, 0, 0, 3),
        "a replayed batch must be recognized whole"
    );
    tx.commit().expect("commit");

    let events_after = store
        .read_txn()
        .expect("read")
        .events_after(&chat, 0, 100)
        .expect("events");
    assert_eq!(
        events_after, events_before,
        "the log must not grow on replay"
    );
}

#[test]
fn edits_append_new_revisions_and_stale_revisions_are_skipped() {
    let mut store = store();
    let chat = common::chat_key(CHAT);
    let original = revision(1, 1_000);

    let tx = store.write_txn().expect("write txn");
    tx.apply_message_changes(&chat, &[MessageChange::Observed(original.clone())])
        .expect("apply");

    // An edit: same identity, newer content.
    let mut edited = original.clone();
    edited.edited_at_ms = Some(5_000);
    edited.observed_at_ms = 5_005;
    edited.payload = b"payload-1-edited".to_vec();
    let applied = tx
        .apply_message_changes(&chat, &[MessageChange::Observed(edited.clone())])
        .expect("apply edit");
    assert_eq!((applied.edited, applied.skipped), (1, 0));

    // The pre-edit revision replayed afterwards — a history page fetched
    // before the edit — must not rewind the projection (SYNC-021).
    let applied = tx
        .apply_message_changes(&chat, &[MessageChange::Observed(original)])
        .expect("stale replay");
    assert_eq!((applied.edited, applied.skipped), (0, 1));
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let state = read
        .message(&gramdrive_state::model::identity::MessageKey {
            chat,
            message_id: MessageId(1),
        })
        .expect("message")
        .expect("some");
    assert_eq!(state.edited_at_ms, Some(5_000), "projection keeps the edit");
    let events = read.events_after(&chat, 0, 100).expect("events");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].kind, MessageEventKind::Edited);
    assert_eq!(
        events[1].payload.as_ref().map(|p| p.bytes.as_slice()),
        Some(b"payload-1-edited".as_slice())
    );
    assert_eq!(
        events[1].payload.as_ref().map(|p| p.schema),
        Some(SchemaFamily(1))
    );
}

#[test]
fn deletions_tombstone_and_never_imply_or_resurrect() {
    let mut store = store();
    let chat = common::chat_key(CHAT);
    let tx = store.write_txn().expect("write txn");

    // Deleting a message that was never observed records nothing (POL-3).
    let applied = tx
        .apply_message_changes(
            &chat,
            &[MessageChange::Deleted {
                message_id: MessageId(9),
                observed_at_ms: 1_000,
            }],
        )
        .expect("apply");
    assert_eq!((applied.deleted, applied.skipped), (0, 1));

    // Observe, then witness the deletion.
    tx.apply_message_changes(&chat, &[MessageChange::Observed(revision(1, 1_000))])
        .expect("observe");
    let applied = tx
        .apply_message_changes(
            &chat,
            &[MessageChange::Deleted {
                message_id: MessageId(1),
                observed_at_ms: 2_000,
            }],
        )
        .expect("delete");
    assert_eq!(applied.deleted, 1);

    // A replayed revision must not resurrect the tombstone (POL-3), and a
    // replayed deletion is a no-op.
    let applied = tx
        .apply_message_changes(
            &chat,
            &[
                MessageChange::Observed(revision(1, 1_000)),
                MessageChange::Deleted {
                    message_id: MessageId(1),
                    observed_at_ms: 2_000,
                },
            ],
        )
        .expect("replay");
    assert_eq!(
        (applied.observed, applied.deleted, applied.skipped),
        (0, 0, 2)
    );
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let state = read
        .message(&gramdrive_state::model::identity::MessageKey {
            chat,
            message_id: MessageId(1),
        })
        .expect("message")
        .expect("some");
    assert!(state.is_deleted);
    let events = read.events_after(&chat, 0, 100).expect("events");
    assert_eq!(events.len(), 2, "observed + deleted, nothing else");
    assert_eq!(events[1].kind, MessageEventKind::Deleted);
    assert!(events[1].payload.is_none(), "tombstones carry no content");
    assert_eq!(
        read.latest_event_seq(&chat).expect("seq"),
        events[1].event_seq
    );
}

#[test]
fn cursors_reject_foreign_and_retired_scopes_explicitly() {
    let mut store = store();

    // A cursor for an unconfigured account cannot be stored.
    let foreign_scope = AccountScope {
        account: AccountKey {
            account_id: AccountId(999),
        },
        namespace_version: NamespaceVersion(0),
    };
    let foreign = ChangeCursor::new(foreign_scope, b"x".to_vec()).expect("cursor");
    let tx = store.write_txn().expect("write txn");
    match tx.put_cursor(STREAM, &foreign, 1_000) {
        Err(StateError::RowNotFound { entity: "account" }) => {}
        other => panic!("expected RowNotFound(account), got {other:?}"),
    }

    // Store a valid cursor, then retire its epoch.
    tx.put_cursor(STREAM, &cursor("pts:7"), 1_000)
        .expect("cursor");
    tx.commit().expect("commit");

    let tx = store.write_txn().expect("write txn");
    let new_namespace = tx.bump_namespace(scope().account, 2_000).expect("bump");
    assert_eq!(
        new_namespace,
        NamespaceVersion(scope().namespace_version.0 + 1)
    );
    tx.commit().expect("commit");

    // Loading under the current scope now names the mismatch (SYNC-004).
    let current = AccountScope {
        account: scope().account,
        namespace_version: new_namespace,
    };
    let read = store.read_txn().expect("read");
    match read.cursor(current, STREAM) {
        Err(StateError::CursorOutOfScope { source }) => {
            assert_eq!(source.expected, current);
            assert_eq!(source.found, scope());
        }
        other => panic!("expected CursorOutOfScope, got {other:?}"),
    }
    drop(read);

    // A stale worker checkpointing into the retired epoch is refused too.
    let tx = store.write_txn().expect("write txn");
    match tx.put_cursor(STREAM, &cursor("pts:8"), 3_000) {
        Err(StateError::CursorOutOfScope { .. }) => {}
        other => panic!("expected CursorOutOfScope, got {other:?}"),
    }

    // Re-baseline: clear, then store a cursor minted under the new epoch.
    assert!(tx.clear_cursor(scope().account, STREAM).expect("clear"));
    assert!(
        !tx.clear_cursor(scope().account, STREAM)
            .expect("clear again")
    );
    let fresh = ChangeCursor::new(current, b"pts:0".to_vec()).expect("cursor");
    tx.put_cursor(STREAM, &fresh, 3_000).expect("put");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let stored = read.cursor(current, STREAM).expect("cursor").expect("some");
    assert_eq!(stored.payload(), b"pts:0");
}

#[test]
fn a_corrupt_stored_cursor_is_reported_not_skipped() {
    let mut store = store();
    let tx = store.write_txn().expect("write txn");
    tx.put_cursor(STREAM, &cursor("pts:1"), 1_000).expect("put");
    tx.commit().expect("commit");

    // Corrupt the stored text behind the typed layer's back.
    store
        .connection()
        .execute("UPDATE change_cursors SET cursor_text = 'not-a-cursor'", [])
        .expect("corrupt");

    let read = store.read_txn().expect("read");
    match read.cursor(scope(), STREAM) {
        Err(StateError::CursorCorrupt { .. }) => {}
        other => panic!("expected CursorCorrupt, got {other:?}"),
    }
}

#[test]
fn empty_stream_names_are_refused() {
    let mut store = store();
    let tx = store.write_txn().expect("write txn");
    match tx.put_cursor("", &cursor("x"), 1_000) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    drop(tx);
    let read = store.read_txn().expect("read");
    match read.cursor(scope(), "") {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

#[test]
fn sync_windows_move_with_state_and_feed_the_backlog() {
    let mut store = store();
    let chat = common::chat_key(CHAT);
    let other = common::chat_key(CHAT + 1);

    let tx = store.write_txn().expect("write txn");
    tx.upsert_chat(&chat_record(CHAT + 1)).expect("chat");

    // An inverted window is a caller bug, refused before SQL.
    match tx.record_chat_sync(
        &chat,
        &ChatSyncRecord {
            window: Some(SyncWindow {
                oldest: MessageId(50),
                newest: MessageId(10),
            }),
            history_complete: false,
            last_sync_at_ms: Some(1_000),
        },
    ) {
        Err(StateError::InvalidArgument { .. }) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }

    // A history page and its window, in one transaction (SYNC-022).
    tx.apply_message_changes(
        &chat,
        &[
            MessageChange::Observed(revision(10, 1_000)),
            MessageChange::Observed(revision(50, 5_000)),
        ],
    )
    .expect("apply");
    let record = ChatSyncRecord {
        window: Some(SyncWindow {
            oldest: MessageId(10),
            newest: MessageId(50),
        }),
        history_complete: false,
        last_sync_at_ms: Some(1_000),
    };
    tx.record_chat_sync(&chat, &record).expect("sync state");
    tx.record_chat_sync(
        &other,
        &ChatSyncRecord {
            window: None,
            history_complete: false,
            last_sync_at_ms: None,
        },
    )
    .expect("sync state");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    assert_eq!(read.chat_sync_state(&chat).expect("state"), Some(record));
    // Never-synced chats lead the backlog (NULL sorts first).
    let backlog = read.backfill_backlog(&scope(), 10).expect("backlog");
    assert_eq!(backlog, vec![ChatId(CHAT + 1), ChatId(CHAT)]);
    drop(read);

    // Completing history removes the chat from the backlog.
    let tx = store.write_txn().expect("write txn");
    tx.record_chat_sync(
        &chat,
        &ChatSyncRecord {
            window: Some(SyncWindow {
                oldest: MessageId(1),
                newest: MessageId(50),
            }),
            history_complete: true,
            last_sync_at_ms: Some(2_000),
        },
    )
    .expect("sync state");
    tx.commit().expect("commit");
    let read = store.read_txn().expect("read");
    let backlog = read.backfill_backlog(&scope(), 10).expect("backlog");
    assert_eq!(backlog, vec![ChatId(CHAT + 1)]);
}

#[test]
fn reads_page_messages_and_events_in_order() {
    let mut store = store();
    let chat = common::chat_key(CHAT);
    let tx = store.write_txn().expect("write txn");
    let changes: Vec<MessageChange> = (1..=5)
        .map(|m| MessageChange::Observed(revision(m, 1_000 * m)))
        .collect();
    tx.apply_message_changes(&chat, &changes).expect("apply");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    // Id-ordered paging (SYNC-021).
    let page = read.messages_after(&chat, MessageId(0), 2).expect("page");
    assert_eq!(
        page.iter().map(|m| m.message_id).collect::<Vec<_>>(),
        vec![MessageId(1), MessageId(2)]
    );
    let page = read
        .messages_after(&chat, page[1].message_id, 10)
        .expect("page");
    assert_eq!(
        page.iter().map(|m| m.message_id).collect::<Vec<_>>(),
        vec![MessageId(3), MessageId(4), MessageId(5)]
    );
    // Time-window reads (SYNC-031): [2000, 4000) — messages 2 and 3.
    let window = read
        .messages_in_window(&chat, 2_000, 4_000)
        .expect("window");
    assert_eq!(window.len(), 2);
    // Event tail from a watermark (SYNC-022).
    let all = read.events_after(&chat, 0, 100).expect("events");
    let tail = read
        .events_after(&chat, all[2].event_seq, 100)
        .expect("tail");
    assert_eq!(tail.len(), 2);
    assert_eq!(read.latest_event_seq(&chat).expect("seq"), all[4].event_seq);
    // A chat with no events sits at watermark zero.
    assert_eq!(
        read.latest_event_seq(&common::chat_key(CHAT + 7))
            .expect("seq"),
        0
    );
}
