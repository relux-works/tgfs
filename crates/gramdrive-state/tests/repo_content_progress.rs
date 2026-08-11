//! Durable privacy-safe chat content progress (TASK-260721-yrcjlo).

#![allow(clippy::expect_used, clippy::panic)]

mod common;

use common::{chat_key, insert_chat, store_with_account};
use gramdrive_state::repo::{ChatContentPhase, ChatContentProgressRecord};

fn progress(
    phase: ChatContentPhase,
    category: Option<&str>,
    retryable: bool,
) -> ChatContentProgressRecord {
    ChatContentProgressRecord {
        phase,
        failure_category: category.map(str::to_owned),
        retryable,
        retry_at_ms: retryable.then_some(9_000),
        attempt_count: u32::from(retryable),
        updated_at_ms: 1_000,
    }
}

#[test]
fn progress_round_trips_and_upserts_without_sensitive_detail() {
    let mut store = store_with_account();
    insert_chat(store.connection(), 100);
    let key = chat_key(100);

    let tx = store.write_txn().expect("write");
    tx.put_chat_content_progress(
        &key,
        &progress(
            ChatContentPhase::Unavailable,
            Some("history-unavailable"),
            true,
        ),
    )
    .expect("put unavailable");
    tx.commit().expect("commit");

    let tx = store.write_txn().expect("write");
    let ready = progress(ChatContentPhase::Ready, None, false);
    tx.put_chat_content_progress(&key, &ready)
        .expect("replace with ready");
    tx.commit().expect("commit");

    let tx = store.read_txn().expect("read");
    assert_eq!(
        tx.chat_content_progress(&key).expect("progress"),
        Some(ready)
    );
}

#[test]
fn first_list_membership_seeds_history_and_canonical_metadata_alone_does_not() {
    let mut store = store_with_account();
    insert_chat(store.connection(), 100);
    let key = chat_key(100);

    let read = store.read_txn().expect("read canonical-only chat");
    assert_eq!(read.chat_sync_state(&key).expect("sync"), None);
    assert_eq!(read.chat_content_progress(&key).expect("progress"), None);
    drop(read);

    store
        .connection()
        .execute(
            "INSERT INTO chat_list_entries (
                 account_id, namespace_version, list_kind, folder_id,
                 chat_id, sort_order, pinned
             ) VALUES (7, 1, 'main', 0, 100, 1, 0)",
            [],
        )
        .expect("insert first list membership");

    let read = store.read_txn().expect("read");
    let sync = read
        .chat_sync_state(&key)
        .expect("sync")
        .expect("seeded sync");
    assert_eq!(sync.window, None);
    assert!(!sync.history_complete);
    let mut expected = progress(ChatContentPhase::Pending, None, false);
    expected.updated_at_ms = 0;
    assert_eq!(
        read.chat_content_progress(&key).expect("progress"),
        Some(expected)
    );
}

#[test]
fn cursor_survives_last_membership_removal_and_reappearance() {
    let mut store = store_with_account();
    insert_chat(store.connection(), 100);
    store
        .connection()
        .execute(
            "INSERT INTO chat_list_entries VALUES (7, 1, 'main', 0, 100, 1, 0)",
            [],
        )
        .expect("insert membership");
    store
        .connection()
        .execute(
            "UPDATE chat_sync_state
             SET oldest_loaded_message_id=10, newest_loaded_message_id=100,
                 last_sync_at_ms=500
             WHERE account_id=7 AND namespace_version=1 AND chat_id=100",
            [],
        )
        .expect("advance cursor");
    store
        .connection()
        .execute(
            "DELETE FROM chat_list_entries
             WHERE account_id=7 AND namespace_version=1 AND chat_id=100",
            [],
        )
        .expect("remove membership");

    let read = store.read_txn().expect("read hidden cursor");
    let hidden = read
        .chat_sync_state(&chat_key(100))
        .expect("hidden cursor")
        .expect("cursor retained");
    assert_eq!(hidden.window.expect("window").oldest.0, 10);
    assert!(
        read.backfill_backlog(&common::scope(), 10, i64::MAX)
            .expect("hidden backlog")
            .is_empty()
    );
    drop(read);

    store
        .connection()
        .execute(
            "INSERT INTO chat_list_entries VALUES (7, 1, 'archive', 0, 100, 1, 0)",
            [],
        )
        .expect("restore membership");
    let read = store.read_txn().expect("read restored cursor");
    assert_eq!(
        read.chat_sync_state(&chat_key(100))
            .expect("restored cursor")
            .expect("cursor")
            .window
            .expect("window")
            .oldest
            .0,
        10,
        "reappearance must resume the durable cursor rather than reseed it"
    );
    assert_eq!(
        read.backfill_backlog(&common::scope(), 10, i64::MAX)
            .expect("restored backlog"),
        vec![gramdrive_state::model::identity::ChatId(100)]
    );
}

#[test]
fn invalid_phase_category_and_retry_combinations_are_rejected() {
    let mut store = store_with_account();
    insert_chat(store.connection(), 100);
    let key = chat_key(100);
    let tx = store.write_txn().expect("write");

    let missing_category = progress(ChatContentPhase::Failed, None, true);
    assert!(
        tx.put_chat_content_progress(&key, &missing_category)
            .is_err()
    );

    let retrying_protected = progress(ChatContentPhase::Protected, Some("protected"), true);
    assert!(
        tx.put_chat_content_progress(&key, &retrying_protected)
            .is_err()
    );
}

#[test]
fn progress_is_deleted_with_its_chat() {
    let mut store = store_with_account();
    insert_chat(store.connection(), 100);
    let key = chat_key(100);
    let tx = store.write_txn().expect("write");
    tx.put_chat_content_progress(&key, &progress(ChatContentPhase::Pending, None, false))
        .expect("put");
    tx.commit().expect("commit");

    store
        .connection()
        .execute(
            "DELETE FROM chats WHERE account_id = 7 AND namespace_version = 1 AND chat_id = 100",
            [],
        )
        .expect("delete chat");
    let tx = store.read_txn().expect("read");
    assert_eq!(tx.chat_content_progress(&key).expect("progress"), None);
}

#[test]
fn a_self_fenced_chat_stays_runnable_while_a_source_refusal_does_not() {
    // A degraded chat is this engine's own "re-crawl me" fence — a live gap,
    // a buffer overflow, an edit whose ids could not be retained. Treating it
    // like a terminal source refusal starved every chat that ever hit one:
    // on a real preserved profile 59 of 410 incomplete listed chats sat
    // fenced indefinitely, reachable only if the user opened them in Finder
    // (BUG-260728-2qfzbd).
    use gramdrive_state::model::identity::ChatId;

    let mut store = store_with_account();
    for chat in [100, 200, 300, 400] {
        insert_chat(store.connection(), chat);
        store
            .connection()
            .execute(
                "INSERT INTO chat_list_entries VALUES (7, 1, 'main', 0, ?1, ?1, 0)",
                [chat],
            )
            .expect("list membership");
        // First list membership seeds the cursor row; give each one a
        // distinct last-turn stamp so backlog order is deterministic. The
        // rotation key is the turn, not the last time anything was synced.
        store
            .connection()
            .execute(
                "UPDATE chat_sync_state
                 SET history_complete = 0, last_backfill_at_ms = ?1
                 WHERE account_id = 7 AND namespace_version = 1 AND chat_id = ?1",
                [chat],
            )
            .expect("incomplete cursor");
    }

    let tx = store.write_txn().expect("write");
    // 100: fenced for full recovery, no deadline — runnable now.
    tx.put_chat_content_progress(
        &chat_key(100),
        &ChatContentProgressRecord {
            phase: ChatContentPhase::Degraded,
            failure_category: Some("live-edit-pending".to_owned()),
            retryable: true,
            retry_at_ms: None,
            attempt_count: 1,
            updated_at_ms: 1_000,
        },
    )
    .expect("fenced chat");
    // 200: fenced with a deadline still in the future — waits for it.
    tx.put_chat_content_progress(
        &chat_key(200),
        &ChatContentProgressRecord {
            phase: ChatContentPhase::Degraded,
            failure_category: Some("live-gap".to_owned()),
            retryable: true,
            retry_at_ms: Some(9_000),
            attempt_count: 1,
            updated_at_ms: 1_000,
        },
    )
    .expect("deferred chat");
    // 300: Telegram itself refused the content — retry on demand only.
    tx.put_chat_content_progress(
        &chat_key(300),
        &progress(
            ChatContentPhase::Unavailable,
            Some("history-unavailable"),
            true,
        ),
    )
    .expect("refused chat");
    // 400: ordinary work.
    tx.put_chat_content_progress(
        &chat_key(400),
        &progress(ChatContentPhase::Pending, None, false),
    )
    .expect("pending chat");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read backlog");
    assert_eq!(
        read.backfill_backlog(&common::scope(), 10, 5_000)
            .expect("backlog"),
        vec![ChatId(100), ChatId(400)],
        "a chat fenced for re-crawl is background work; a source refusal and \
         an unexpired retry deadline are not"
    );
    assert_eq!(
        read.backfill_backlog(&common::scope(), 10, 9_000)
            .expect("backlog at the deadline"),
        vec![ChatId(100), ChatId(200), ChatId(400)],
        "the deferred fence becomes runnable the moment its deadline arrives"
    );
}
