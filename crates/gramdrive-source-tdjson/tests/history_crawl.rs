//! The resumable per-chat history crawl, end to end (TASK-260715-26dnp6):
//! the sans-IO [`CrawlMachine`] driven against a deterministic scripted
//! TDLib history, with every per-page commit persisted through the typed
//! `gramdrive-state` repositories in one transaction (SYNC-022) — records
//! via `apply_message_changes`, the `[oldest, newest]` window via
//! `record_chat_sync` — exactly as a composing caller must.
//!
//! The machine is sans-IO, so most suites drive it directly against the
//! fixture server: interruption fixtures then restart the crawl from the
//! durable `chat_sync_state` rows at *every* commit boundary and assert
//! byte-exact convergence — no duplicate events, no missing messages
//! (SYNC-021). One suite additionally drives the full loop through the
//! real runtime over the mock tdjson, proving the request payloads
//! round-trip the correlation path unchanged.

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
    ChatCrawl, ChatUnavailable, CrawlBackoff, CrawlMachine, CrawlPhase, CrawlPlan, CrawlPriority,
    CrawlStep, CrawlWindow, HistoryCommit, UnavailableReason,
};
use gramdrive_source_tdjson::message::{MessageRecord, SenderRef, TopicRef};
use gramdrive_source_tdjson::mock::SentRequest;
use gramdrive_state::StateStore;
use gramdrive_state::repo::{
    AccountRecord, ChatRecord, ChatSyncRecord, ChatType, MessageChange, MessageRevision,
    RetentionMode, SourceKind, SyncWindow,
};

use common::GUARD;

const ACCOUNT_ID: i64 = 11;
const NAMESPACE: u32 = 1;
/// One fixed observation clock for every commit: replay determinism is the
/// point of the suite, and timestamps are source-explicit (SYNC-073).
const OBSERVED_AT_MS: i64 = 1_000;

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
// Fixture messages: TDLib message objects of every planned flavor
// ---------------------------------------------------------------------------

/// A plain text message from a user.
fn text_message(chat_id: i64, id: i64, user_id: i64, text: &str) -> Value {
    json!({
        "@type": "message",
        "id": id,
        "chat_id": chat_id,
        "date": 1_700_000_000 + id,
        "sender_id": {"@type": "messageSenderUser", "user_id": user_id},
        "can_be_saved": true,
        "content": {
            "@type": "messageText",
            "text": {"@type": "formattedText", "text": text, "entities": []},
        },
    })
}

/// A forum-topic message carrying an album key — the supergroup/topic
/// shape of the scope.
fn topic_album_message(chat_id: i64, id: i64, forum_topic_id: i64, album_id: i64) -> Value {
    let mut message = text_message(chat_id, id, 500, "topic post");
    message["topic_id"] = json!({"@type": "messageTopicForum", "forum_topic_id": forum_topic_id});
    message["media_album_id"] = json!(album_id.to_string());
    message
}

/// A channel post: the sender is the chat itself.
fn channel_post(chat_id: i64, id: i64, text: &str) -> Value {
    let mut message = text_message(chat_id, id, 0, text);
    message["sender_id"] = json!({"@type": "messageSenderChat", "chat_id": chat_id});
    message
}

// ---------------------------------------------------------------------------
// The fixture server: a deterministic scripted TDLib history
// ---------------------------------------------------------------------------

/// One scripted chat: its full history keyed by message id, or a scripted
/// per-chat rejection (the left/unsupported condition).
#[derive(Debug, Clone, Default)]
struct FixtureChat {
    messages: BTreeMap<i64, Value>,
    rejection: Option<(i64, &'static str)>,
}

impl FixtureChat {
    fn with_messages(messages: impl IntoIterator<Item = Value>) -> FixtureChat {
        let mut chat = FixtureChat::default();
        for message in messages {
            chat.push(message);
        }
        chat
    }

    fn rejected(code: i64, message: &'static str) -> FixtureChat {
        FixtureChat {
            messages: BTreeMap::new(),
            rejection: Some((code, message)),
        }
    }

    fn push(&mut self, message: Value) {
        let id = message["id"].as_i64().expect("fixture messages carry ids");
        assert!(
            self.messages.insert(id, message).is_none(),
            "fixture message id {id} repeats"
        );
    }
}

/// The scripted server: chats, a queue of scripted flood rejections, and
/// the exact request log.
#[derive(Debug, Default)]
struct FixtureServer {
    chats: BTreeMap<i64, FixtureChat>,
    /// Pending flood rejections: the next `getChatHistory` for the named
    /// chat answers 429 with this stated delay.
    floods: Vec<(i64, u64)>,
    /// Every request answered, as `(chat_id, from_message_id)` in order.
    served: Vec<(i64, i64)>,
}

impl FixtureServer {
    fn new(chats: impl IntoIterator<Item = (i64, FixtureChat)>) -> FixtureServer {
        FixtureServer {
            chats: chats.into_iter().collect(),
            floods: Vec::new(),
            served: Vec::new(),
        }
    }

    /// Answer one `getChatHistory` request the way TDLib does: ids
    /// strictly below `from_message_id` (`0`: from the newest), strictly
    /// descending, at most `limit` of them.
    fn respond(&mut self, request: &Value) -> Result<Value, TdError> {
        assert_eq!(
            request["@type"].as_str(),
            Some("getChatHistory"),
            "the crawl must issue nothing but getChatHistory"
        );
        assert_eq!(request["offset"].as_i64(), Some(0));
        assert_eq!(request["only_local"].as_bool(), Some(false));
        let chat_id = request["chat_id"]
            .as_i64()
            .expect("request carries chat_id");
        let from = request["from_message_id"]
            .as_i64()
            .expect("request carries from_message_id");
        let limit = request["limit"].as_u64().expect("request carries limit") as usize;
        assert!(limit >= 1, "limit must be positive");
        self.served.push((chat_id, from));
        if let Some(position) = self.floods.iter().position(|(id, _)| *id == chat_id) {
            let (_, delay) = self.floods.remove(position);
            return Err(TdError::Td {
                code: 429,
                message: format!("Too Many Requests: retry after {delay}"),
            });
        }
        let Some(chat) = self.chats.get(&chat_id) else {
            return Err(TdError::Td {
                code: 400,
                message: "Chat not found".to_owned(),
            });
        };
        if let Some((code, message)) = chat.rejection {
            return Err(TdError::Td {
                code,
                message: message.to_owned(),
            });
        }
        let page: Vec<Value> = chat
            .messages
            .iter()
            .rev()
            .filter(|(id, _)| from == 0 || **id < from)
            .take(limit)
            .map(|(_, message)| message.clone())
            .collect();
        Ok(json!({
            "@type": "messages",
            "total_count": chat.messages.len(),
            "messages": page,
        }))
    }
}

// ---------------------------------------------------------------------------
// Persistence: exactly what a composing caller must do per commit
// ---------------------------------------------------------------------------

/// A store with the account and every fixture chat's canonical row
/// registered — the snapshot's output, which the crawl builds on.
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
        tx.upsert_chat(&ChatRecord {
            key: chat_key(chat_id),
            chat_type: ChatType::Private,
            title: format!("chat {chat_id}"),
            username: None,
            is_protected: false,
            archive_mode: false,
            metadata_version: MetadataVersion::new("h1").expect("valid token"),
            left_at_ms: None,
            deleted_at_ms: None,
            last_update_at_ms: Some(OBSERVED_AT_MS),
        })
        .expect("chat row upserts");
    }
    tx.commit().expect("commit setup");
    store
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

/// Persist one commit atomically: the page's records and the window facts
/// in one transaction (SYNC-022).
fn apply_commit(store: &mut StateStore, commit: &HistoryCommit) {
    let tx = store.write_txn().expect("write txn");
    let chat = chat_key(commit.chat_id);
    let changes: Vec<MessageChange> = commit
        .records
        .iter()
        .map(|record| MessageChange::Observed(revision_of(record)))
        .collect();
    tx.apply_message_changes(&chat, &changes)
        .expect("message batch applies");
    tx.record_chat_sync(
        &chat,
        &ChatSyncRecord {
            window: commit.window.map(|window| SyncWindow {
                oldest: MessageId(window.oldest_message_id),
                newest: MessageId(window.newest_message_id),
            }),
            history_complete: commit.history_complete,
            last_sync_at_ms: Some(OBSERVED_AT_MS),
        },
    )
    .expect("sync state records");
    tx.commit().expect("commit transaction");
}

/// Seed exactly what a Run-1 crawl of an empty chat leaves behind:
/// `{window: None, history_complete: true}` with no messages — the
/// machine's own durable output, asserted by the unit suite's
/// `empty_chat_completes_without_a_window`. Applied through the same
/// commit path so the durable row is byte-identical to a real empty
/// commit.
fn seed_empty_complete(store: &mut StateStore, chat_id: i64) {
    apply_commit(
        store,
        &HistoryCommit {
            chat_id,
            records: Vec::new(),
            window: None,
            history_complete: true,
            skipped_malformed: 0,
        },
    );
}

/// Rebuild the crawl plan from the durable per-chat cursors — the whole
/// of what resuming is (module docs of `history`).
fn plan_from_store(store: &mut StateStore, chat_ids: &[i64], page_size: u32) -> CrawlPlan {
    let tx = store.read_txn().expect("read txn");
    let chats = chat_ids
        .iter()
        .map(|&chat_id| {
            match tx
                .chat_sync_state(&chat_key(chat_id))
                .expect("sync state reads")
            {
                None => ChatCrawl::new(chat_id),
                Some(record) => ChatCrawl {
                    chat_id,
                    window: record.window.map(|window| CrawlWindow {
                        oldest_message_id: window.oldest.0,
                        newest_message_id: window.newest.0,
                    }),
                    history_complete: record.history_complete,
                    priority: CrawlPriority::Background,
                },
            }
        })
        .collect();
    CrawlPlan { chats, page_size }
}

fn stored_message_ids(store: &mut StateStore, chat_id: i64) -> Vec<i64> {
    let tx = store.read_txn().expect("read txn");
    tx.messages_after(&chat_key(chat_id), MessageId(0), 100_000)
        .expect("messages read")
        .iter()
        .map(|message| message.message_id.0)
        .collect()
}

fn stored_event_count(store: &mut StateStore, chat_id: i64) -> usize {
    let tx = store.read_txn().expect("read txn");
    tx.events_after(&chat_key(chat_id), 0, 100_000)
        .expect("events read")
        .len()
}

fn stored_sync(store: &mut StateStore, chat_id: i64) -> ChatSyncRecord {
    let tx = store.read_txn().expect("read txn");
    tx.chat_sync_state(&chat_key(chat_id))
        .expect("sync state reads")
        .expect("sync state exists")
}

// ---------------------------------------------------------------------------
// Driving
// ---------------------------------------------------------------------------

/// What a completed drive observed.
#[derive(Debug, Default)]
struct DriveLog {
    commits: Vec<HistoryCommit>,
    backoffs: Vec<CrawlBackoff>,
    unavailable: Vec<ChatUnavailable>,
}

/// Drive the machine against the fixture server until `Done` or a
/// scripted interruption, persisting every commit as it arrives. The
/// machine is sans-IO, so the fixture answers requests directly; the
/// runtime path is proven separately below.
fn drive(
    machine: &mut CrawlMachine,
    server: &mut FixtureServer,
    store: &mut StateStore,
    stop_after_commits: Option<usize>,
) -> DriveLog {
    let mut log = DriveLog::default();
    loop {
        match machine.next_step().expect("crawl step") {
            CrawlStep::Submit(request) => {
                let outcome = server.respond(&request);
                machine.on_response(outcome).expect("response folds");
            }
            CrawlStep::Backoff(backoff) => log.backoffs.push(backoff),
            CrawlStep::Commit(commit) => {
                apply_commit(store, &commit);
                log.commits.push(*commit);
                if stop_after_commits.is_some_and(|stop| log.commits.len() >= stop) {
                    return log;
                }
            }
            CrawlStep::Unavailable(unavailable) => log.unavailable.push(*unavailable),
            CrawlStep::Done => return log,
        }
    }
}

// ---------------------------------------------------------------------------
// Suites
// ---------------------------------------------------------------------------

/// Every chat flavor of the scope crawls to completion: boundaries are
/// recorded, topic/album/sender facts survive normalization, an empty
/// chat completes windowless, and progress is observable per chat.
#[test]
fn full_crawl_covers_every_flavor_and_records_boundaries() {
    let private = 100;
    let group = 200;
    let supergroup = 300;
    let channel = 400;
    let empty = 500;
    let chat_ids = [private, group, supergroup, channel, empty];
    let mut server = FixtureServer::new([
        (
            private,
            FixtureChat::with_messages((1..=7).map(|id| text_message(private, id, 42, "hi"))),
        ),
        (
            group,
            FixtureChat::with_messages((1..=3).map(|id| text_message(group, id * 10, 43, "g"))),
        ),
        (
            supergroup,
            FixtureChat::with_messages(
                (1..=4).map(|id| topic_album_message(supergroup, id, 9, 777)),
            ),
        ),
        (
            channel,
            FixtureChat::with_messages((1..=2).map(|id| channel_post(channel, id, "post"))),
        ),
        (empty, FixtureChat::default()),
    ]);
    let mut store = store_with_chats(&chat_ids);
    let plan = CrawlPlan {
        chats: chat_ids.iter().copied().map(ChatCrawl::new).collect(),
        page_size: 3,
    };
    let mut machine = CrawlMachine::new(plan).expect("plan is valid");
    let log = drive(&mut machine, &mut server, &mut store, None);
    assert!(log.backoffs.is_empty());
    assert!(log.unavailable.is_empty());

    // Exactly the fixture's messages, no more and no fewer, per chat.
    assert_eq!(
        stored_message_ids(&mut store, private),
        (1..=7).collect::<Vec<_>>()
    );
    assert_eq!(stored_message_ids(&mut store, group), vec![10, 20, 30]);
    assert_eq!(
        stored_message_ids(&mut store, supergroup),
        (1..=4).collect::<Vec<_>>()
    );
    assert_eq!(stored_message_ids(&mut store, channel), vec![1, 2]);
    assert_eq!(stored_message_ids(&mut store, empty), Vec::<i64>::new());

    // Boundaries and completion, as the durable rows record them.
    let sync = stored_sync(&mut store, private);
    assert_eq!(
        sync.window,
        Some(SyncWindow {
            oldest: MessageId(1),
            newest: MessageId(7),
        })
    );
    assert!(sync.history_complete);
    let sync = stored_sync(&mut store, empty);
    assert_eq!(sync.window, None);
    assert!(sync.history_complete);

    // Topic, album, and channel-sender facts survived normalization.
    let supergroup_records: Vec<&MessageRecord> = log
        .commits
        .iter()
        .filter(|commit| commit.chat_id == supergroup)
        .flat_map(|commit| &commit.records)
        .collect();
    assert!(!supergroup_records.is_empty());
    for record in supergroup_records {
        assert_eq!(record.topic, Some(TopicRef::Forum { forum_topic_id: 9 }));
        assert_eq!(record.album_id, Some(777));
    }
    let channel_record = log
        .commits
        .iter()
        .filter(|commit| commit.chat_id == channel)
        .flat_map(|commit| &commit.records)
        .next()
        .expect("the channel committed records");
    assert_eq!(channel_record.sender, SenderRef::Chat { chat_id: channel });

    // Per-chat progress is observable, and every chat finished.
    let progress = machine.progress();
    assert_eq!(progress.len(), chat_ids.len());
    for chat in &progress {
        assert_eq!(chat.phase, CrawlPhase::Complete, "chat {}", chat.chat_id);
        assert!(chat.history_complete);
    }

    // Bounded batches: no answer ever exceeded the page size (the server
    // asserted the request shape; this asserts the machine's chunking).
    for commit in &log.commits {
        assert!(commit.records.len() <= 3);
    }
}

/// The interruption fixture (SYNC-021): a crawl killed after *every*
/// possible commit boundary and restarted from the durable rows converges
/// to exactly the uninterrupted result — no duplicate events, no missing
/// messages.
#[test]
fn restart_at_every_commit_boundary_resumes_exactly() {
    let chat = 700;
    let ids: Vec<i64> = (1..=23).collect();
    let fixture = || {
        FixtureServer::new([(
            chat,
            FixtureChat::with_messages(ids.iter().map(|&id| text_message(chat, id, 42, "m"))),
        )])
    };

    // The reference: one uninterrupted run.
    let mut reference_store = store_with_chats(&[chat]);
    let mut machine =
        CrawlMachine::new(plan_from_store(&mut reference_store, &[chat], 5)).expect("valid plan");
    let reference = drive(&mut machine, &mut fixture(), &mut reference_store, None);
    let reference_ids = stored_message_ids(&mut reference_store, chat);
    assert_eq!(reference_ids, ids);
    let total_commits = reference.commits.len();
    assert!(total_commits > 2, "the fixture must page several times");

    for stop_after in 1..total_commits {
        let mut store = store_with_chats(&[chat]);
        let mut machine =
            CrawlMachine::new(plan_from_store(&mut store, &[chat], 5)).expect("valid plan");
        let interrupted = drive(&mut machine, &mut fixture(), &mut store, Some(stop_after));
        assert_eq!(interrupted.commits.len(), stop_after);
        // The machine is dropped here — the crash. A fresh one resumes
        // from nothing but the durable rows.
        let mut machine =
            CrawlMachine::new(plan_from_store(&mut store, &[chat], 5)).expect("valid plan");
        drive(&mut machine, &mut fixture(), &mut store, None);

        assert_eq!(
            stored_message_ids(&mut store, chat),
            reference_ids,
            "stop after {stop_after}: message set must match the uninterrupted run"
        );
        assert_eq!(
            stored_event_count(&mut store, chat),
            ids.len(),
            "stop after {stop_after}: every message observed exactly once — \
             replayed pages must append nothing"
        );
        let sync = stored_sync(&mut store, chat);
        assert_eq!(
            sync.window,
            Some(SyncWindow {
                oldest: MessageId(1),
                newest: MessageId(23),
            }),
            "stop after {stop_after}"
        );
        assert!(sync.history_complete, "stop after {stop_after}");
    }
}

/// The empty-complete → active flavor of the every-commit-boundary
/// fixture (TASK-260715-26dnp6 review: anchor-gap). A chat Run 1 committed
/// as empty (`window: None, history_complete: true` — the machine's own
/// durable output) gains *more than one page* of messages during
/// downtime. Resuming Anchors a fresh window while the plan still carries
/// the stale `history_complete=true`; killed after *every* commit
/// boundary, the crawl must still converge gap-free. Without the anchor
/// fold resetting completeness, a crash right after the anchor commit
/// persists `history_complete=true` over a partial window, and the next
/// resume's catch-up concludes `Complete` and skips the backfill — the
/// oldest ids orphaned, silently. This pins that class shut.
#[test]
fn resume_of_a_grown_empty_complete_chat_resumes_exactly() {
    let chat = 750;
    let ids: Vec<i64> = (1..=13).collect();
    let fixture = || {
        FixtureServer::new([(
            chat,
            FixtureChat::with_messages(ids.iter().map(|&id| text_message(chat, id, 42, "grown"))),
        )])
    };

    // The reference: seed the empty-complete row, then one uninterrupted
    // resume against the history that arrived during downtime.
    let mut reference_store = store_with_chats(&[chat]);
    seed_empty_complete(&mut reference_store, chat);
    let mut machine =
        CrawlMachine::new(plan_from_store(&mut reference_store, &[chat], 5)).expect("valid plan");
    let reference = drive(&mut machine, &mut fixture(), &mut reference_store, None);
    let reference_ids = stored_message_ids(&mut reference_store, chat);
    assert_eq!(
        reference_ids, ids,
        "the resumed crawl must recover the full grown history"
    );
    let ref_sync = stored_sync(&mut reference_store, chat);
    assert_eq!(
        ref_sync.window,
        Some(SyncWindow {
            oldest: MessageId(1),
            newest: MessageId(13),
        })
    );
    assert!(ref_sync.history_complete);
    let total_commits = reference.commits.len();
    assert!(
        total_commits > 2,
        "the grown history must page several times"
    );

    for stop_after in 1..total_commits {
        let mut store = store_with_chats(&[chat]);
        seed_empty_complete(&mut store, chat);
        let mut machine =
            CrawlMachine::new(plan_from_store(&mut store, &[chat], 5)).expect("valid plan");
        let interrupted = drive(&mut machine, &mut fixture(), &mut store, Some(stop_after));
        assert_eq!(interrupted.commits.len(), stop_after);
        // The machine is dropped here — the crash right at (or after) the
        // anchor commit. A fresh one resumes from the durable rows alone.
        let mut machine =
            CrawlMachine::new(plan_from_store(&mut store, &[chat], 5)).expect("valid plan");
        drive(&mut machine, &mut fixture(), &mut store, None);

        assert_eq!(
            stored_message_ids(&mut store, chat),
            reference_ids,
            "stop after {stop_after}: message set must match the uninterrupted resume — \
             no id orphaned by a stale completeness flag"
        );
        assert_eq!(
            stored_event_count(&mut store, chat),
            ids.len(),
            "stop after {stop_after}: every message observed exactly once"
        );
        let sync = stored_sync(&mut store, chat);
        assert_eq!(
            sync.window,
            Some(SyncWindow {
                oldest: MessageId(1),
                newest: MessageId(13),
            }),
            "stop after {stop_after}"
        );
        assert!(
            sync.history_complete,
            "stop after {stop_after}: completeness holds only once the backward \
             phase reached the empty answer"
        );
    }
}

/// Downtime catch-up: messages that arrived while the crawl was down are
/// fetched newest-first until the committed window reconnects, the newest
/// boundary advances, and an interruption mid-catch-up loses nothing.
#[test]
fn catch_up_after_downtime_extends_the_newest_boundary() {
    let chat = 800;
    let old_ids: Vec<i64> = (1..=6).collect();
    let mut server = FixtureServer::new([(
        chat,
        FixtureChat::with_messages(old_ids.iter().map(|&id| text_message(chat, id, 42, "old"))),
    )]);
    let mut store = store_with_chats(&[chat]);
    let mut machine =
        CrawlMachine::new(plan_from_store(&mut store, &[chat], 3)).expect("valid plan");
    drive(&mut machine, &mut server, &mut store, None);
    assert!(stored_sync(&mut store, chat).history_complete);

    // Downtime: seven newer messages arrive (several catch-up pages).
    for id in 7..=13 {
        server
            .chats
            .get_mut(&chat)
            .expect("chat exists")
            .push(text_message(chat, id, 42, "new"));
    }

    // First resume attempt dies after one catch-up commit — the durable
    // newest must not have moved (the window stays contiguous).
    server.served.clear();
    let mut machine =
        CrawlMachine::new(plan_from_store(&mut store, &[chat], 3)).expect("valid plan");
    drive(&mut machine, &mut server, &mut store, Some(1));
    let sync = stored_sync(&mut store, chat);
    assert_eq!(
        sync.window,
        Some(SyncWindow {
            oldest: MessageId(1),
            newest: MessageId(6),
        }),
        "an unconnected catch-up commit must not advance the durable newest"
    );

    // Second resume completes. The window now spans everything, every
    // message is stored exactly once, and no backfill request was issued
    // (history was already complete).
    let mut machine =
        CrawlMachine::new(plan_from_store(&mut store, &[chat], 3)).expect("valid plan");
    drive(&mut machine, &mut server, &mut store, None);
    assert_eq!(
        stored_message_ids(&mut store, chat),
        (1..=13).collect::<Vec<_>>()
    );
    assert_eq!(
        stored_event_count(&mut store, chat),
        13,
        "re-observed overlap pages must append nothing"
    );
    let sync = stored_sync(&mut store, chat);
    assert_eq!(
        sync.window,
        Some(SyncWindow {
            oldest: MessageId(1),
            newest: MessageId(13),
        })
    );
    assert!(sync.history_complete);
    assert!(
        !server.served.iter().any(|&(_, from)| from == 1),
        "a complete chat must never page below its oldest again"
    );
}

/// Flood control honored end to end: the stated delay surfaces as backoff
/// advice, the identical request is re-issued, and the crawl completes.
#[test]
fn flood_wait_is_honored_and_the_crawl_completes() {
    let chat = 900;
    let mut server = FixtureServer::new([(
        chat,
        FixtureChat::with_messages((1..=4).map(|id| text_message(chat, id, 42, "m"))),
    )]);
    server.floods.push((chat, 17));
    let mut store = store_with_chats(&[chat]);
    let mut machine =
        CrawlMachine::new(plan_from_store(&mut store, &[chat], 10)).expect("valid plan");
    let log = drive(&mut machine, &mut server, &mut store, None);
    assert_eq!(log.backoffs.len(), 1);
    assert_eq!(log.backoffs[0].retry_after_secs, Some(17));
    assert_eq!(log.backoffs[0].attempt, 1);
    assert_eq!(
        server.served[0], server.served[1],
        "after the wait, the identical request is re-issued"
    );
    assert_eq!(
        stored_message_ids(&mut store, chat),
        (1..=4).collect::<Vec<_>>()
    );
    assert!(stored_sync(&mut store, chat).history_complete);
}

/// A left/inaccessible chat fails explicitly and alone: the rejection is
/// typed, nothing of that chat is persisted, and every other chat
/// completes.
#[test]
fn an_unavailable_chat_is_explicit_and_the_rest_complete() {
    let healthy = 1000;
    let left = 1100;
    let mut server = FixtureServer::new([
        (
            healthy,
            FixtureChat::with_messages((1..=3).map(|id| text_message(healthy, id, 42, "m"))),
        ),
        (left, FixtureChat::rejected(400, "CHANNEL_PRIVATE")),
    ]);
    let mut store = store_with_chats(&[healthy, left]);
    let mut machine =
        CrawlMachine::new(plan_from_store(&mut store, &[healthy, left], 10)).expect("valid plan");
    let log = drive(&mut machine, &mut server, &mut store, None);
    assert_eq!(log.unavailable.len(), 1);
    assert_eq!(log.unavailable[0].chat_id, left);
    assert!(matches!(
        &log.unavailable[0].reason,
        UnavailableReason::Rejected {
            source: TdError::Td { code: 400, message }
        } if message == "CHANNEL_PRIVATE"
    ));
    assert_eq!(stored_message_ids(&mut store, healthy), vec![1, 2, 3]);
    assert!(stored_sync(&mut store, healthy).history_complete);
    assert_eq!(stored_message_ids(&mut store, left), Vec::<i64>::new());
    let progress = machine.progress();
    assert_eq!(progress[0].phase, CrawlPhase::Complete);
    assert_eq!(progress[1].phase, CrawlPhase::Unavailable);
}

/// Scheduling: equal-priority chats round-robin page by page (a huge
/// history cannot starve the rest), and a visibility boost mid-run takes
/// every following page until the boosted chat is done.
#[test]
fn priority_favors_visible_chats_and_equals_round_robin() {
    let huge = 1200;
    let small = 1300;
    let boosted = 1400;
    let mut server = FixtureServer::new([
        (
            huge,
            FixtureChat::with_messages((1..=30).map(|id| text_message(huge, id, 42, "h"))),
        ),
        (
            small,
            FixtureChat::with_messages((1..=4).map(|id| text_message(small, id, 42, "s"))),
        ),
        (
            boosted,
            FixtureChat::with_messages((1..=8).map(|id| text_message(boosted, id, 42, "b"))),
        ),
    ]);
    let mut store = store_with_chats(&[huge, small, boosted]);
    let mut machine = CrawlMachine::new(plan_from_store(&mut store, &[huge, small, boosted], 2))
        .expect("valid plan");

    // Drive by hand: boost after the third commit, then observe.
    let mut commits = 0usize;
    let mut served_after_boost: Vec<i64> = Vec::new();
    let mut boosted_done_at: Option<usize> = None;
    loop {
        match machine.next_step().expect("crawl step") {
            CrawlStep::Submit(request) => {
                if commits > 3 && boosted_done_at.is_none() {
                    served_after_boost.push(request["chat_id"].as_i64().expect("chat id"));
                }
                let outcome = server.respond(&request);
                machine.on_response(outcome).expect("response folds");
            }
            CrawlStep::Commit(commit) => {
                apply_commit(&mut store, &commit);
                commits += 1;
                if commits == 3 {
                    assert!(machine.set_priority(boosted, CrawlPriority::Visible));
                }
                if commit.chat_id == boosted && commit.history_complete {
                    boosted_done_at = Some(commits);
                }
            }
            CrawlStep::Backoff(_) | CrawlStep::Unavailable(_) => {
                panic!("this fixture scripts neither floods nor rejections")
            }
            CrawlStep::Done => break,
        }
    }

    // The first three pages round-robin the three equal chats in plan
    // order — breadth, not depth.
    assert_eq!(
        server.served[..3]
            .iter()
            .map(|&(chat, _)| chat)
            .collect::<Vec<_>>(),
        vec![huge, small, boosted]
    );
    // After the boost, every page until the boosted chat finished was its
    // own.
    assert!(boosted_done_at.is_some(), "the boosted chat finished");
    assert!(
        !served_after_boost.is_empty()
            && served_after_boost
                .iter()
                .take_while(|&&chat| chat == boosted)
                .count()
                >= served_after_boost.iter().filter(|&&c| c == boosted).count(),
        "after the boost the visible chat is served exclusively until done: {served_after_boost:?}"
    );
    // Everything still completes.
    assert_eq!(stored_message_ids(&mut store, huge).len(), 30);
    assert_eq!(stored_message_ids(&mut store, small).len(), 4);
    assert_eq!(stored_message_ids(&mut store, boosted).len(), 8);
}

/// The same loop through the real runtime over the mock tdjson: request
/// payloads round-trip the correlation path unchanged and the commits
/// land identically (the wiring the composing caller will use).
#[test]
fn the_crawl_round_trips_through_the_real_runtime() {
    let chat = 1500;
    let server = Arc::new(Mutex::new(FixtureServer::new([(
        chat,
        FixtureChat::with_messages((1..=5).map(|id| text_message(chat, id, 42, "m"))),
    )])));
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
    let (client, _updates) = runtime.create_client().expect("client registers");
    let mut store = store_with_chats(&[chat]);
    let mut machine =
        CrawlMachine::new(plan_from_store(&mut store, &[chat], 2)).expect("valid plan");
    loop {
        match machine.next_step().expect("crawl step") {
            CrawlStep::Submit(request) => {
                let pending = client.request(request).expect("request submits");
                let outcome = pending
                    .wait_timeout(GUARD)
                    .unwrap_or_else(|_| panic!("a fixture response must arrive within the guard"));
                machine.on_response(outcome).expect("response folds");
            }
            CrawlStep::Commit(commit) => apply_commit(&mut store, &commit),
            CrawlStep::Backoff(_) | CrawlStep::Unavailable(_) => {
                panic!("this fixture scripts neither floods nor rejections")
            }
            CrawlStep::Done => break,
        }
    }
    runtime.shutdown();
    assert_eq!(
        stored_message_ids(&mut store, chat),
        (1..=5).collect::<Vec<_>>()
    );
    assert!(stored_sync(&mut store, chat).history_complete);
}
