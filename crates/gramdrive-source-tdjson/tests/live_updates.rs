//! The ordered live message update loop, end to end (TASK-260715-10p5zp):
//! the sans-IO [`LiveMachine`] fed TDLib's message push updates against a
//! deterministic scripted history, with every commit persisted through the
//! typed `gramdrive-state` repositories in one transaction (SYNC-022) —
//! records via `apply_message_changes`, the cursor advance merged into the
//! stored `chat_sync_state` window under the same transaction — exactly as
//! a composing caller must.
//!
//! The caller here embodies the merge discipline the machine's contract
//! names: a live commit's `advance_newest` only ever *raises* the stored
//! window's newest (the stored `oldest` and `history_complete` are kept),
//! and a crawl commit's window bounds merge min/max against the stored row
//! — so an in-progress backfill and the live loop never clobber each
//! other's checkpoints. The suites then assert the acceptance criteria
//! straight from the store: gaps recover before the cursor is published
//! (SYNC-023), a crash at any commit boundary never leaves a cursor ahead
//! of state, and duplicate/out-of-order delivery is idempotent (SYNC-021).

// clippy.toml exempts test code keyed on `#[test]` functions; the fixture
// server and driver helpers below sit at module level in an
// integration-test binary. The rationale applies in full — this file links
// into no product artifact (established test-suite pattern, common/mod.rs).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, ChatId, ChatKey, MessageId, NamespaceVersion, SchemaFamily,
};
use gramdrive_model::version::MetadataVersion;
use gramdrive_source_tdjson::TdError;
use gramdrive_source_tdjson::history::{
    ChatCrawl, CrawlMachine, CrawlPlan, CrawlPriority, CrawlStep, CrawlWindow, HistoryCommit,
};
use gramdrive_source_tdjson::live::{LiveChat, LiveCommit, LiveMachine, LivePlan, LiveStep};
use gramdrive_source_tdjson::message::{MessageRecord, SenderRef};
use gramdrive_source_tdjson::mock::SentRequest;
use gramdrive_state::StateStore;
use gramdrive_state::repo::{
    AccountRecord, ChatRecord, ChatSyncRecord, ChatType, MessageChange, MessageEventKind,
    MessageRevision, RetentionMode, SourceKind, SyncWindow,
};

use common::GUARD;

use gramdrive_source_tdjson::live::LiveChange;

const ACCOUNT_ID: i64 = 13;
const NAMESPACE: u32 = 1;
/// One fixed observation clock for every commit: replay determinism is the
/// point of the suite, and timestamps are source-explicit (SYNC-073).
const OBSERVED_AT_MS: i64 = 2_000;

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(ACCOUNT_ID),
        },
        namespace_version: NamespaceVersion(NAMESPACE),
    }
}

fn chat_key(chat_id: i64) -> ChatKey {
    ChatKey {
        scope: scope(),
        chat_id: ChatId(chat_id),
    }
}

// ---------------------------------------------------------------------------
// Fixture messages and updates
// ---------------------------------------------------------------------------

/// A plain text message from a user.
fn text_message(chat_id: i64, id: i64, text: &str) -> Value {
    json!({
        "@type": "message",
        "id": id,
        "chat_id": chat_id,
        "date": 1_700_000_000 + id,
        "sender_id": {"@type": "messageSenderUser", "user_id": 42},
        "can_be_saved": true,
        "content": {
            "@type": "messageText",
            "text": {"@type": "formattedText", "text": text, "entities": []},
        },
    })
}

fn new_message_update(message: &Value) -> Value {
    json!({"@type": "updateNewMessage", "message": message})
}

fn content_update(chat_id: i64, message_id: i64) -> Value {
    json!({"@type": "updateMessageContent", "chat_id": chat_id, "message_id": message_id})
}

fn delete_update(chat_id: i64, ids: &[i64]) -> Value {
    json!({
        "@type": "updateDeleteMessages",
        "chat_id": chat_id,
        "message_ids": ids,
        "is_permanent": true,
        "from_cache": false,
    })
}

// ---------------------------------------------------------------------------
// The fixture server: a deterministic scripted, *mutable* TDLib history
// ---------------------------------------------------------------------------

/// The scripted server: per-chat histories that the test mutates as the
/// "world" changes (growth while offline, edits, deletions), answering
/// both requests the live loop issues.
#[derive(Debug, Default)]
struct FixtureServer {
    chats: BTreeMap<i64, BTreeMap<i64, Value>>,
    /// Pending flood rejections: the next request for the named chat
    /// answers 429 with this stated delay.
    floods: Vec<(i64, u64)>,
}

impl FixtureServer {
    fn new(chats: impl IntoIterator<Item = (i64, Vec<Value>)>) -> FixtureServer {
        let mut server = FixtureServer::default();
        for (chat_id, messages) in chats {
            for message in messages {
                server.grow(chat_id, message);
            }
        }
        server
    }

    /// Append one message to a chat's history (a message that "arrived").
    fn grow(&mut self, chat_id: i64, message: Value) {
        let id = message["id"].as_i64().expect("fixture messages carry ids");
        assert!(
            self.chats
                .entry(chat_id)
                .or_default()
                .insert(id, message)
                .is_none(),
            "fixture message id {id} repeats"
        );
    }

    /// Replace one message's text and stamp an edit date — the "world"
    /// edited it.
    fn edit(&mut self, chat_id: i64, id: i64, text: &str) {
        let message = self
            .chats
            .get_mut(&chat_id)
            .and_then(|chat| chat.get_mut(&id))
            .expect("edited message exists");
        message["content"]["text"]["text"] = json!(text);
        message["edit_date"] = json!(1_700_000_000 + id + 500);
    }

    /// Remove one message — the "world" deleted it.
    fn remove(&mut self, chat_id: i64, id: i64) {
        self.chats
            .get_mut(&chat_id)
            .and_then(|chat| chat.remove(&id))
            .expect("removed message exists");
    }

    /// The current fixture message object — what a live update carries.
    fn message(&self, chat_id: i64, id: i64) -> Value {
        self.chats
            .get(&chat_id)
            .and_then(|chat| chat.get(&id))
            .cloned()
            .expect("fixture message exists")
    }

    /// Answer one request the way TDLib does.
    fn respond(&mut self, request: &Value) -> Result<Value, TdError> {
        let chat_id = request["chat_id"]
            .as_i64()
            .expect("request carries chat_id");
        if let Some(position) = self.floods.iter().position(|(id, _)| *id == chat_id) {
            let (_, delay) = self.floods.remove(position);
            return Err(TdError::Td {
                code: 429,
                message: format!("Too Many Requests: retry after {delay}"),
            });
        }
        match request["@type"].as_str() {
            Some("getChatHistory") => {
                assert_eq!(request["offset"].as_i64(), Some(0));
                assert_eq!(request["only_local"].as_bool(), Some(false));
                let from = request["from_message_id"]
                    .as_i64()
                    .expect("request carries from_message_id");
                let limit = request["limit"].as_u64().expect("request carries limit") as usize;
                assert!(limit >= 1, "limit must be positive");
                let Some(chat) = self.chats.get(&chat_id) else {
                    return Err(TdError::Td {
                        code: 400,
                        message: "Chat not found".to_owned(),
                    });
                };
                let page: Vec<Value> = chat
                    .iter()
                    .rev()
                    .filter(|(id, _)| from == 0 || **id < from)
                    .take(limit)
                    .map(|(_, message)| message.clone())
                    .collect();
                Ok(json!({
                    "@type": "messages",
                    "total_count": chat.len(),
                    "messages": page,
                }))
            }
            Some("getMessage") => {
                let message_id = request["message_id"]
                    .as_i64()
                    .expect("request carries message_id");
                match self
                    .chats
                    .get(&chat_id)
                    .and_then(|chat| chat.get(&message_id))
                {
                    Some(message) => Ok(message.clone()),
                    None => Err(TdError::Td {
                        code: 404,
                        message: "Not Found".to_owned(),
                    }),
                }
            }
            other => panic!("the live loop must not issue {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Persistence: exactly what a composing caller must do per commit
// ---------------------------------------------------------------------------

/// A store with the account and every fixture chat's canonical row
/// registered — the snapshot's output, which the live loop builds on.
fn store_with_chats(chat_ids: &[i64]) -> StateStore {
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
        created_at_ms: OBSERVED_AT_MS,
        updated_at_ms: OBSERVED_AT_MS,
    })
    .expect("account row");
    for &chat_id in chat_ids {
        upsert_chat_row(&tx, chat_id);
    }
    tx.commit().expect("commit setup");
    store
}

fn upsert_chat_row(tx: &gramdrive_state::repo::WriteTxn<'_>, chat_id: i64) {
    tx.upsert_chat(&ChatRecord {
        key: chat_key(chat_id),
        chat_type: ChatType::Private,
        title: format!("chat {chat_id}"),
        username: None,
        is_protected: false,
        archive_mode: false,
        metadata_version: MetadataVersion::new("l1").expect("valid token"),
        left_at_ms: None,
        deleted_at_ms: None,
        last_update_at_ms: Some(OBSERVED_AT_MS),
    })
    .expect("chat row upserts");
}

/// The revision a caller derives from one normalized record. The payload
/// is the record's deterministic debug form — good enough for the replay
/// byte-comparison the suite is about; the real serialization schema is
/// the composing caller's own contract.
fn revision_of(record: &MessageRecord) -> MessageRevision {
    MessageRevision {
        message_id: MessageId(record.message_id),
        sender_id: match record.sender {
            SenderRef::User { user_id } => Some(user_id),
            SenderRef::Chat { chat_id } => Some(chat_id),
            SenderRef::Unknown { .. } => None,
        },
        sent_at_ms: record.sent_at_ms,
        edited_at_ms: record.edited_at_ms,
        observed_at_ms: OBSERVED_AT_MS,
        payload_schema: SchemaFamily(1),
        payload: format!("{record:?}").into_bytes(),
    }
}

/// Persist one live commit atomically: the ordered changes plus the
/// cursor advance *merged into the stored window* — newest only ever
/// rises, the stored `oldest` and `history_complete` are kept, and no
/// window is ever established here (the machine's caller contract).
fn apply_live_commit(store: &mut StateStore, commit: &LiveCommit) {
    let tx = store.write_txn().expect("write txn");
    let chat = chat_key(commit.chat_id);
    let changes: Vec<MessageChange> = commit
        .changes
        .iter()
        .map(|change| match change {
            LiveChange::Observed(record) => MessageChange::Observed(revision_of(record)),
            LiveChange::Deleted { message_id } => MessageChange::Deleted {
                message_id: MessageId(*message_id),
                observed_at_ms: OBSERVED_AT_MS,
            },
        })
        .collect();
    tx.apply_message_changes(&chat, &changes)
        .expect("message batch applies");
    if let Some(advance) = commit.advance_newest {
        let stored = tx
            .read()
            .chat_sync_state(&chat)
            .expect("sync state reads")
            .expect("a cursor advance implies a committed window");
        let window = stored
            .window
            .expect("a cursor advance implies a committed window");
        tx.record_chat_sync(
            &chat,
            &ChatSyncRecord {
                window: Some(SyncWindow {
                    oldest: window.oldest,
                    newest: MessageId(window.newest.0.max(advance)),
                }),
                history_complete: stored.history_complete,
                last_sync_at_ms: Some(OBSERVED_AT_MS),
            },
        )
        .expect("sync state records");
    }
    tx.commit().expect("commit transaction");
}

/// Persist one crawl commit with the same merge discipline from the other
/// side: window bounds merge min/max against the stored row, so a live
/// advance that already raised `newest` is never regressed by a crawl
/// page committed after it. Completion is the crawl's to state.
fn apply_crawl_commit_merged(store: &mut StateStore, commit: &HistoryCommit) {
    let tx = store.write_txn().expect("write txn");
    let chat = chat_key(commit.chat_id);
    let changes: Vec<MessageChange> = commit
        .records
        .iter()
        .map(|record| MessageChange::Observed(revision_of(record)))
        .collect();
    tx.apply_message_changes(&chat, &changes)
        .expect("message batch applies");
    let stored = tx
        .read()
        .chat_sync_state(&chat)
        .expect("sync state reads")
        .and_then(|record| record.window);
    let window = match (commit.window, stored) {
        (None, stored) => stored,
        (Some(window), None) => Some(SyncWindow {
            oldest: MessageId(window.oldest_message_id),
            newest: MessageId(window.newest_message_id),
        }),
        (Some(window), Some(stored)) => Some(SyncWindow {
            oldest: MessageId(window.oldest_message_id.min(stored.oldest.0)),
            newest: MessageId(window.newest_message_id.max(stored.newest.0)),
        }),
    };
    tx.record_chat_sync(
        &chat,
        &ChatSyncRecord {
            window,
            history_complete: commit.history_complete,
            last_sync_at_ms: Some(OBSERVED_AT_MS),
        },
    )
    .expect("sync state records");
    tx.commit().expect("commit transaction");
}

/// Seed a chat's already-crawled state: the records and the committed
/// window, exactly as a finished (or partial) crawl leaves them.
fn seed_history(store: &mut StateStore, chat_id: i64, messages: &[Value], complete: bool) {
    let mut oldest = i64::MAX;
    let mut newest = i64::MIN;
    let tx = store.write_txn().expect("write txn");
    let chat = chat_key(chat_id);
    let changes: Vec<MessageChange> = messages
        .iter()
        .map(|message| {
            let record = gramdrive_source_tdjson::message::normalize_message(message)
                .expect("fixture messages normalize");
            oldest = oldest.min(record.message_id);
            newest = newest.max(record.message_id);
            MessageChange::Observed(revision_of(&record))
        })
        .collect();
    assert!(!changes.is_empty(), "seed at least one message");
    tx.apply_message_changes(&chat, &changes)
        .expect("seed applies");
    tx.record_chat_sync(
        &chat,
        &ChatSyncRecord {
            window: Some(SyncWindow {
                oldest: MessageId(oldest),
                newest: MessageId(newest),
            }),
            history_complete: complete,
            last_sync_at_ms: Some(OBSERVED_AT_MS),
        },
    )
    .expect("seed records");
    tx.commit().expect("commit seed");
}

// ---------------------------------------------------------------------------
// Reading back
// ---------------------------------------------------------------------------

fn stored_message_ids(store: &mut StateStore, chat_id: i64) -> Vec<i64> {
    let tx = store.read_txn().expect("read txn");
    tx.messages_after(&chat_key(chat_id), MessageId(0), 100_000)
        .expect("messages read")
        .iter()
        .map(|message| message.message_id.0)
        .collect()
}

fn stored_event_kinds(store: &mut StateStore, chat_id: i64) -> Vec<(i64, MessageEventKind)> {
    let tx = store.read_txn().expect("read txn");
    tx.events_after(&chat_key(chat_id), 0, 100_000)
        .expect("events read")
        .iter()
        .map(|event| (event.message_id.0, event.kind))
        .collect()
}

fn stored_sync(store: &mut StateStore, chat_id: i64) -> ChatSyncRecord {
    let tx = store.read_txn().expect("read txn");
    tx.chat_sync_state(&chat_key(chat_id))
        .expect("sync state reads")
        .expect("sync state exists")
}

fn is_deleted(store: &mut StateStore, chat_id: i64, message_id: i64) -> bool {
    let tx = store.read_txn().expect("read txn");
    tx.message(&gramdrive_model::identity::MessageKey {
        chat: chat_key(chat_id),
        message_id: MessageId(message_id),
    })
    .expect("message reads")
    .expect("message exists")
    .is_deleted
}

/// The publication invariant (SYNC-023): the durable cursor never claims
/// coverage it does not have — every id the fixture's history holds
/// inside the stored window must be present in the store. (The store may
/// hold *more*: messages deleted from the fixture stay as POL-3 rows.)
fn assert_cursor_covered(store: &mut StateStore, server: &FixtureServer, chat_id: i64) {
    let Some(window) = stored_sync(store, chat_id).window else {
        return;
    };
    let stored = stored_message_ids(store, chat_id);
    let missing: Vec<i64> = server
        .chats
        .get(&chat_id)
        .map(|chat| {
            chat.keys()
                .filter(|id| **id >= window.oldest.0 && **id <= window.newest.0)
                .filter(|id| !stored.contains(id))
                .copied()
                .collect()
        })
        .unwrap_or_default();
    assert!(
        missing.is_empty(),
        "cursor [{}, {}] claims ids the store never observed: {missing:?}",
        window.oldest.0,
        window.newest.0
    );
}

// ---------------------------------------------------------------------------
// Driving
// ---------------------------------------------------------------------------

/// What a completed drive observed.
#[derive(Debug, Default)]
struct DriveLog {
    commits: Vec<LiveCommit>,
    backoffs: u32,
    unresolved: Vec<i64>,
    degraded: Vec<i64>,
}

/// Drive the machine against the fixture server until `Idle` or a
/// scripted interruption, persisting every commit as it arrives.
fn drive(
    machine: &mut LiveMachine,
    server: &mut FixtureServer,
    store: &mut StateStore,
    stop_after_commits: Option<usize>,
) -> DriveLog {
    let mut log = DriveLog::default();
    loop {
        match machine.next_step().expect("live step") {
            LiveStep::Submit(request) => {
                let outcome = server.respond(&request);
                machine.on_response(outcome).expect("response folds");
            }
            LiveStep::Backoff(_) => log.backoffs += 1,
            LiveStep::Commit(commit) => {
                apply_live_commit(store, &commit);
                log.commits.push(*commit);
                if stop_after_commits.is_some_and(|stop| log.commits.len() >= stop) {
                    return log;
                }
            }
            LiveStep::Unresolved { chat_id } => log.unresolved.push(chat_id),
            LiveStep::Degraded(degraded) => log.degraded.push(degraded.chat_id),
            LiveStep::Idle => return log,
        }
    }
}

/// Rebuild the live plan from the durable per-chat cursors — resuming is
/// nothing but the plan carrying them back in.
fn plan_from_store(store: &mut StateStore, chat_ids: &[i64], page_size: u32) -> LivePlan {
    let tx = store.read_txn().expect("read txn");
    let chats = chat_ids
        .iter()
        .map(|&chat_id| LiveChat {
            chat_id,
            newest_message_id: tx
                .chat_sync_state(&chat_key(chat_id))
                .expect("sync state reads")
                .and_then(|record| record.window)
                .map(|window| window.newest.0),
        })
        .collect();
    LivePlan { chats, page_size }
}

// ---------------------------------------------------------------------------
// Suites
// ---------------------------------------------------------------------------

/// The full live flow: a new message bridges and advances the cursor,
/// further messages extend it directly, an edit refreshes into an
/// `edited` event, a deletion becomes a tombstone — and the stored
/// window's `oldest`/`history_complete` never move under any of it.
#[test]
fn live_messages_edits_and_deletes_flow_into_the_event_log() {
    let chat = 100;
    let history: Vec<Value> = (1..=3).map(|id| text_message(chat, id, "old")).collect();
    let mut server = FixtureServer::new([(chat, history.clone())]);
    let mut store = store_with_chats(&[chat]);
    seed_history(&mut store, chat, &history, true);

    let mut machine =
        LiveMachine::new(plan_from_store(&mut store, &[chat], 100)).expect("plan is valid");

    // A new message arrives live; the bridge connects on its first page.
    server.grow(chat, text_message(chat, 4, "four"));
    machine.on_update(&new_message_update(&server.message(chat, 4)));
    let log = drive(&mut machine, &mut server, &mut store, None);
    assert!(log.unresolved.is_empty() && log.degraded.is_empty());
    assert_eq!(
        stored_sync(&mut store, chat)
            .window
            .expect("window")
            .newest
            .0,
        4
    );

    // Verified now: the next message advances without any request.
    server.grow(chat, text_message(chat, 5, "five"));
    machine.on_update(&new_message_update(&server.message(chat, 5)));
    let log = drive(&mut machine, &mut server, &mut store, None);
    assert_eq!(log.commits.len(), 1);
    assert_eq!(log.commits[0].advance_newest, Some(5));

    // An edit refreshes through getMessage into an `edited` event.
    server.edit(chat, 4, "four, edited");
    machine.on_update(&content_update(chat, 4));
    drive(&mut machine, &mut server, &mut store, None);

    // A deletion becomes a tombstone.
    server.remove(chat, 2);
    machine.on_update(&delete_update(chat, &[2]));
    drive(&mut machine, &mut server, &mut store, None);

    assert_eq!(stored_message_ids(&mut store, chat), vec![1, 2, 3, 4, 5]);
    assert!(is_deleted(&mut store, chat, 2));
    assert!(!is_deleted(&mut store, chat, 4));
    let events = stored_event_kinds(&mut store, chat);
    // Seeded 1..=3 observed; 4 observed then edited; 5 observed; 2 deleted.
    let of = |id: i64| -> Vec<MessageEventKind> {
        events
            .iter()
            .filter(|(message, _)| *message == id)
            .map(|(_, kind)| *kind)
            .collect()
    };
    assert_eq!(
        of(4),
        vec![MessageEventKind::Observed, MessageEventKind::Edited]
    );
    assert_eq!(
        of(2),
        vec![MessageEventKind::Observed, MessageEventKind::Deleted]
    );
    assert_eq!(of(5), vec![MessageEventKind::Observed]);

    let sync = stored_sync(&mut store, chat);
    assert_eq!(
        sync.window,
        Some(SyncWindow {
            oldest: MessageId(1),
            newest: MessageId(5),
        }),
        "oldest never moves under the live loop"
    );
    assert!(
        sync.history_complete,
        "completion is the crawl's, untouched"
    );
}

/// The gap fixture (SYNC-023): messages arrived while the process was
/// down; a live message triggers the bridge, and at *every* persisted
/// commit the cursor-coverage invariant holds — the stored newest never
/// names a range with unobserved fixture messages inside it.
#[test]
fn gaps_recover_before_the_cursor_is_published() {
    let chat = 200;
    let seeded: Vec<Value> = (1..=10).map(|id| text_message(chat, id, "m")).collect();
    let mut server = FixtureServer::new([(chat, seeded.clone())]);
    let mut store = store_with_chats(&[chat]);
    seed_history(&mut store, chat, &seeded, false);

    // Offline growth: 11..=14 landed while nothing was listening.
    for id in 11..=14 {
        server.grow(chat, text_message(chat, id, "offline"));
    }
    // Then 15 arrives live.
    server.grow(chat, text_message(chat, 15, "live"));
    let mut machine =
        LiveMachine::new(plan_from_store(&mut store, &[chat], 3)).expect("plan is valid");
    machine.on_update(&new_message_update(&server.message(chat, 15)));

    // Drive step by step, checking the invariant after every commit.
    let mut commits = Vec::new();
    loop {
        match machine.next_step().expect("live step") {
            LiveStep::Submit(request) => {
                let outcome = server.respond(&request);
                machine.on_response(outcome).expect("response folds");
            }
            LiveStep::Commit(commit) => {
                apply_live_commit(&mut store, &commit);
                assert_cursor_covered(&mut store, &server, chat);
                commits.push(*commit);
            }
            LiveStep::Idle => break,
            other => panic!("unexpected step {other:?}"),
        }
    }

    // The cursor moved exactly once, on the connecting commit, and only
    // after the gap pages were already down.
    let advances: Vec<Option<i64>> = commits.iter().map(|c| c.advance_newest).collect();
    let moved: Vec<i64> = advances.iter().flatten().copied().collect();
    assert_eq!(
        moved,
        vec![15],
        "one advance, to the live top: {advances:?}"
    );
    assert_eq!(
        advances.last().expect("commits exist"),
        &Some(15),
        "the advance rides the last (connecting) commit"
    );
    assert_eq!(
        stored_message_ids(&mut store, chat),
        (1..=15).collect::<Vec<_>>()
    );
    let sync = stored_sync(&mut store, chat);
    assert_eq!(
        sync.window,
        Some(SyncWindow {
            oldest: MessageId(1),
            newest: MessageId(15),
        })
    );
}

/// The interruption fixture (the crash half of SYNC-022): the loop killed
/// after *every* possible commit boundary leaves a durable state whose
/// cursor claims nothing uncovered, and a fresh machine planned from the
/// durable rows and re-fed the same updates converges to exactly the
/// uninterrupted result — no duplicate events, no missing messages.
#[test]
fn restart_at_every_commit_boundary_converges_exactly() {
    let chat = 300;
    let seeded: Vec<Value> = (1..=5).map(|id| text_message(chat, id, "m")).collect();
    let grown: Vec<i64> = (6..=12).collect();
    let live: Vec<i64> = vec![13, 14, 15];
    let fixture = || {
        let mut server = FixtureServer::new([(chat, seeded.clone())]);
        for &id in grown.iter().chain(&live) {
            server.grow(chat, text_message(chat, id, "m"));
        }
        server
    };
    let feed = |machine: &mut LiveMachine, server: &FixtureServer| {
        for &id in &live {
            machine.on_update(&new_message_update(&server.message(chat, id)));
        }
    };

    // The reference: one uninterrupted run.
    let mut reference_store = store_with_chats(&[chat]);
    seed_history(&mut reference_store, chat, &seeded, false);
    let mut server = fixture();
    let mut machine =
        LiveMachine::new(plan_from_store(&mut reference_store, &[chat], 3)).expect("plan is valid");
    feed(&mut machine, &server);
    let reference = drive(&mut machine, &mut server, &mut reference_store, None);
    let reference_ids = stored_message_ids(&mut reference_store, chat);
    assert_eq!(reference_ids, (1..=15).collect::<Vec<_>>());
    let reference_events = stored_event_kinds(&mut reference_store, chat);
    assert_eq!(
        reference_events.len(),
        15,
        "every message observed exactly once — overlapping live records \
         and bridge pages must coalesce"
    );
    let total_commits = reference.commits.len();
    assert!(total_commits > 2, "the fixture must page several times");

    for stop_after in 1..total_commits {
        let mut store = store_with_chats(&[chat]);
        seed_history(&mut store, chat, &seeded, false);
        let mut server = fixture();
        let mut machine =
            LiveMachine::new(plan_from_store(&mut store, &[chat], 3)).expect("plan is valid");
        feed(&mut machine, &server);
        let interrupted = drive(&mut machine, &mut server, &mut store, Some(stop_after));
        assert_eq!(interrupted.commits.len(), stop_after);
        // The machine is dropped here — the crash. The durable state must
        // already satisfy the publication invariant.
        assert_cursor_covered(&mut store, &server, chat);

        // A fresh machine resumes from nothing but the durable rows; the
        // live updates replay (at-least-once delivery).
        let mut server = fixture();
        let mut machine =
            LiveMachine::new(plan_from_store(&mut store, &[chat], 3)).expect("plan is valid");
        feed(&mut machine, &server);
        drive(&mut machine, &mut server, &mut store, None);

        assert_eq!(
            stored_message_ids(&mut store, chat),
            reference_ids,
            "stop after {stop_after}: message set must match the uninterrupted run"
        );
        assert_eq!(
            stored_event_kinds(&mut store, chat).len(),
            reference_events.len(),
            "stop after {stop_after}: replayed observations must append nothing"
        );
        assert_eq!(
            stored_sync(&mut store, chat).window,
            Some(SyncWindow {
                oldest: MessageId(1),
                newest: MessageId(15),
            }),
            "stop after {stop_after}"
        );
    }
}

/// Duplicate and out-of-order delivery (SYNC-021): everything re-fed,
/// deletions before observations, edits after deletions — the store
/// converges and nothing resurrects or forges.
#[test]
fn duplicate_and_out_of_order_updates_are_idempotent() {
    let chat = 400;
    let seeded: Vec<Value> = (1..=4).map(|id| text_message(chat, id, "m")).collect();
    let mut server = FixtureServer::new([(chat, seeded.clone())]);
    let mut store = store_with_chats(&[chat]);
    seed_history(&mut store, chat, &seeded, true);
    let mut machine =
        LiveMachine::new(plan_from_store(&mut store, &[chat], 100)).expect("plan is valid");

    // A deletion of a message never observed anywhere: skipped, never a
    // forged row (POL-3).
    machine.on_update(&delete_update(chat, &[99]));
    // A deletion observed, then the same deletion again, then an edit
    // signal for the deleted message (its refresh answers 404 after the
    // fixture removal).
    server.remove(chat, 3);
    machine.on_update(&delete_update(chat, &[3]));
    machine.on_update(&delete_update(chat, &[3]));
    machine.on_update(&content_update(chat, 3));
    // A new message delivered twice.
    server.grow(chat, text_message(chat, 5, "five"));
    let five = new_message_update(&server.message(chat, 5));
    machine.on_update(&five);
    machine.on_update(&five);
    let log = drive(&mut machine, &mut server, &mut store, None);
    assert!(log.degraded.is_empty() && log.unresolved.is_empty());

    let baseline_events = stored_event_kinds(&mut store, chat);
    assert_eq!(
        stored_message_ids(&mut store, chat),
        vec![1, 2, 3, 4, 5],
        "no forged row for the never-observed 99"
    );
    assert!(is_deleted(&mut store, chat, 3));
    // 1,2,3,4 observed at seed; 3 deleted once; 5 observed once.
    assert_eq!(baseline_events.len(), 6, "{baseline_events:?}");

    // Full replay of the same updates: a fixed point.
    machine.on_update(&delete_update(chat, &[99]));
    machine.on_update(&delete_update(chat, &[3]));
    machine.on_update(&content_update(chat, 3));
    machine.on_update(&five);
    drive(&mut machine, &mut server, &mut store, None);
    assert_eq!(
        stored_event_kinds(&mut store, chat),
        baseline_events,
        "replay must append nothing"
    );
    assert!(is_deleted(&mut store, chat, 3), "no resurrection");
    assert_eq!(
        stored_sync(&mut store, chat).window,
        Some(SyncWindow {
            oldest: MessageId(1),
            newest: MessageId(5),
        })
    );
}

/// The crawl/live interplay (the story's crawl/live boundary): a backfill
/// in progress while live messages arrive. Both sides commit through the
/// merging caller; neither loses the other's checkpoint, and every
/// message lands exactly once.
#[test]
fn in_progress_backfill_and_live_updates_never_lose_state() {
    let chat = 500;
    // Full server history 1..=10; the store has only [8, 10] crawled.
    let mut server = FixtureServer::new([(
        chat,
        (1..=10).map(|id| text_message(chat, id, "m")).collect(),
    )]);
    let seeded: Vec<Value> = (8..=10).map(|id| text_message(chat, id, "m")).collect();
    let mut store = store_with_chats(&[chat]);
    seed_history(&mut store, chat, &seeded, false);

    // Live arrivals 11..=12 exist by the time the machines start.
    for id in 11..=12 {
        server.grow(chat, text_message(chat, id, "m"));
    }
    let mut live = LiveMachine::new(plan_from_store(&mut store, &[chat], 3)).expect("plan");
    let mut crawl = CrawlMachine::new(CrawlPlan {
        chats: vec![ChatCrawl {
            chat_id: chat,
            window: Some(CrawlWindow {
                oldest_message_id: 8,
                newest_message_id: 10,
            }),
            history_complete: false,
            priority: CrawlPriority::Background,
        }],
        page_size: 3,
    })
    .expect("plan");
    live.on_update(&new_message_update(&server.message(chat, 11)));
    live.on_update(&new_message_update(&server.message(chat, 12)));

    // Interleave: one step each, strictly alternating, until both rest.
    // Message 13 arrives mid-backfill, after the live boundary verified.
    let mut crawl_done = false;
    let mut grew_13 = false;
    loop {
        let live_step = live.next_step().expect("live step");
        let live_idle = matches!(live_step, LiveStep::Idle);
        match live_step {
            LiveStep::Submit(request) => {
                let outcome = server.respond(&request);
                live.on_response(outcome).expect("response folds");
            }
            LiveStep::Commit(commit) => apply_live_commit(&mut store, &commit),
            LiveStep::Idle => {}
            other => panic!("unexpected live step {other:?}"),
        }
        if live_idle && !grew_13 {
            grew_13 = true;
            server.grow(chat, text_message(chat, 13, "m"));
            live.on_update(&new_message_update(&server.message(chat, 13)));
            continue;
        }
        if !crawl_done {
            match crawl.next_step().expect("crawl step") {
                CrawlStep::Submit(request) => {
                    let outcome = server.respond(&request);
                    crawl.on_response(outcome).expect("response folds");
                }
                CrawlStep::Commit(commit) => apply_crawl_commit_merged(&mut store, &commit),
                CrawlStep::Done => crawl_done = true,
                other => panic!("unexpected crawl step {other:?}"),
            }
        } else if live_idle && grew_13 {
            break;
        }
    }

    assert_eq!(
        stored_message_ids(&mut store, chat),
        (1..=13).collect::<Vec<_>>()
    );
    assert_eq!(
        stored_event_kinds(&mut store, chat).len(),
        13,
        "every message observed exactly once across both machines"
    );
    let sync = stored_sync(&mut store, chat);
    assert_eq!(
        sync.window,
        Some(SyncWindow {
            oldest: MessageId(1),
            newest: MessageId(13),
        }),
        "the merged cursor keeps the crawl's oldest and the live newest"
    );
    assert!(sync.history_complete, "the backfill finished");
}

/// An update naming a chat the plan never knew: buffered, reported once,
/// and replayed after the caller registers the chat — foreign-key safe.
#[test]
fn an_unresolved_chat_tracks_and_replays_through_the_store() {
    let known = 600;
    let ghost = 601;
    let seeded: Vec<Value> = vec![text_message(known, 1, "m")];
    let mut server = FixtureServer::new([(known, seeded.clone())]);
    let mut store = store_with_chats(&[known]);
    seed_history(&mut store, known, &seeded, true);
    let mut machine =
        LiveMachine::new(plan_from_store(&mut store, &[known], 100)).expect("plan is valid");

    server.grow(ghost, text_message(ghost, 7, "ghost"));
    machine.on_update(&new_message_update(&server.message(ghost, 7)));
    let log = drive(&mut machine, &mut server, &mut store, None);
    assert_eq!(log.unresolved, vec![ghost]);
    assert!(
        log.commits.is_empty(),
        "nothing commits before the chat exists"
    );

    // The caller resolves the chat (canonical row first — the FK), then
    // tracks it; the buffer replays.
    {
        let tx = store.write_txn().expect("write txn");
        upsert_chat_row(&tx, ghost);
        tx.commit().expect("commit chat row");
    }
    assert!(machine.track_chat(ghost, None));
    let log = drive(&mut machine, &mut server, &mut store, None);
    assert_eq!(log.commits.len(), 1);
    assert_eq!(stored_message_ids(&mut store, ghost), vec![7]);
    let tx = store.read_txn().expect("read txn");
    assert!(
        tx.chat_sync_state(&chat_key(ghost))
            .expect("sync state reads")
            .is_none(),
        "no cursor is ever established for an uncrawled chat"
    );
}

/// The same loop through the real runtime over the mock tdjson: updates
/// arrive over the client's update stream, re-fetch payloads round-trip
/// the correlation path unchanged, and the commits land identically (the
/// wiring the composing caller will use).
#[test]
fn the_live_loop_round_trips_through_the_real_runtime() {
    let chat = 700;
    let seeded: Vec<Value> = (1..=3).map(|id| text_message(chat, id, "m")).collect();
    let server = Arc::new(Mutex::new(FixtureServer::new([(chat, seeded.clone())])));
    let mut store = store_with_chats(&[chat]);
    seed_history(&mut store, chat, &seeded, true);

    let (runtime, handle) = common::start_runtime(common::test_config());
    let responder_server = Arc::clone(&server);
    handle.set_responder(move |sent: &SentRequest| {
        let request: Value = serde_json::from_str(&sent.json).expect("request is JSON");
        let extra = sent.extra().expect("runtime injects @extra");
        let mut answer = match responder_server
            .lock()
            .expect("fixture lock")
            .respond(&request)
        {
            Ok(answer) => answer,
            Err(TdError::Td { code, message }) => json!({
                "@type": "error",
                "code": code,
                "message": message,
            }),
            Err(other) => panic!("the fixture only scripts TDLib errors, got {other}"),
        };
        answer["@extra"] = json!(extra);
        answer["@client_id"] = json!(sent.client_id);
        vec![answer.to_string()]
    });
    let (client, updates) = runtime.create_client().expect("client registers");
    let mut machine =
        LiveMachine::new(plan_from_store(&mut store, &[chat], 2)).expect("plan is valid");

    // Two live messages and an edit arrive over the update stream.
    {
        let mut server = server.lock().expect("fixture lock");
        server.grow(chat, text_message(chat, 4, "four"));
        server.grow(chat, text_message(chat, 5, "five"));
        let events = [
            new_message_update(&server.message(chat, 4)),
            new_message_update(&server.message(chat, 5)),
        ];
        for event in events {
            let mut event = event.clone();
            event["@client_id"] = json!(client.client_id());
            handle.push_event(&event.to_string());
        }
    }
    for _ in 0..2 {
        let update = updates
            .recv_timeout(GUARD)
            .unwrap_or_else(|error| panic!("update must arrive within the guard: {error:?}"));
        machine.on_update(&update);
    }

    loop {
        match machine.next_step().expect("live step") {
            LiveStep::Submit(request) => {
                let pending = client.request(request).expect("request submits");
                let outcome = pending
                    .wait_timeout(GUARD)
                    .unwrap_or_else(|_| panic!("a fixture response must arrive within the guard"));
                machine.on_response(outcome).expect("response folds");
            }
            LiveStep::Commit(commit) => apply_live_commit(&mut store, &commit),
            LiveStep::Idle => break,
            other => panic!("this fixture scripts no {other:?}"),
        }
    }
    runtime.shutdown();

    assert_eq!(
        stored_message_ids(&mut store, chat),
        (1..=5).collect::<Vec<_>>()
    );
    assert_eq!(
        stored_sync(&mut store, chat).window,
        Some(SyncWindow {
            oldest: MessageId(1),
            newest: MessageId(5),
        })
    );
}

/// Flood control on a bridge: the backoff surfaces, the identical request
/// re-issues, and the run completes as if never throttled (SYNC-044).
#[test]
fn a_flooded_bridge_backs_off_and_completes() {
    let chat = 800;
    let seeded: Vec<Value> = (1..=2).map(|id| text_message(chat, id, "m")).collect();
    let mut server = FixtureServer::new([(chat, seeded.clone())]);
    let mut store = store_with_chats(&[chat]);
    seed_history(&mut store, chat, &seeded, true);
    server.floods.push((chat, 30));

    let mut machine =
        LiveMachine::new(plan_from_store(&mut store, &[chat], 100)).expect("plan is valid");
    server.grow(chat, text_message(chat, 3, "three"));
    machine.on_update(&new_message_update(&server.message(chat, 3)));
    let log = drive(&mut machine, &mut server, &mut store, None);
    assert_eq!(log.backoffs, 1, "the flood surfaced as one backoff");
    assert_eq!(stored_message_ids(&mut store, chat), vec![1, 2, 3]);
    assert_eq!(
        stored_sync(&mut store, chat)
            .window
            .expect("window")
            .newest
            .0,
        3
    );
}
