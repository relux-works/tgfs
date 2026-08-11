//! The initial chat-list snapshot, end to end (TASK-260715-30amrq): the
//! sans-IO [`SnapshotMachine`] driven against the real runtime over a
//! scripted mock TDLib, with every per-list commit persisted through the
//! typed `gramdrive-state` repositories in one transaction (SYNC-022).
//!
//! The fixture server mirrors TDLib's chat-list protocol faithfully:
//! `loadChats` pages push `updateNewChat` (positions empty) followed by
//! explicit `updateChatPosition` events and answer `ok`, then error `404`
//! when the list is exhausted; `getChats` answers the ordered id witness;
//! `getChat` answers the full chat object (pushing the owning user/
//! supergroup object first, as TDLib does). Everything is deterministic:
//! the suites assert exact orders, exact request surfaces, and exact
//! database contents.

// clippy.toml exempts test code keyed on `#[test]` functions; the fixture
// server and driver helpers below sit at module level in an
// integration-test binary. The rationale applies in full — this file links
// into no product artifact (established test-suite pattern, common/mod.rs).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use gramdrive_model::cursor::ChangeCursor;
use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, ChatId, ChatKey, ChatListKey, ChatListKind, FolderId,
    NamespaceVersion,
};
use gramdrive_model::version::MetadataVersion;
use gramdrive_source_tdjson::mock::SentRequest;
use gramdrive_source_tdjson::snapshot::{
    ListCommit, SNAPSHOT_CURSOR_STREAM, SnapshotBackoff, SnapshotChatKind, SnapshotError,
    SnapshotMachine, SnapshotPlan, SnapshotStep,
};
use gramdrive_source_tdjson::{RuntimeConfig, TdClient, TdError, UpdateStream};
use gramdrive_state::StateStore;
use gramdrive_state::repo::{
    AccountRecord, ChatListEntry, ChatRecord, ChatType, RetentionMode, SourceKind,
};

use common::{GUARD, start_runtime, test_config};

const ACCOUNT_ID: i64 = 7;
const NAMESPACE: u32 = 1;
const FOLDER: i32 = 4;

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(ACCOUNT_ID),
        },
        namespace_version: NamespaceVersion(NAMESPACE),
    }
}

fn folder_list() -> ChatListKind {
    ChatListKind::Folder(FolderId(FOLDER))
}

// ---------------------------------------------------------------------------
// The fixture server: a deterministic scripted TDLib over the mock
// ---------------------------------------------------------------------------

/// Chat flavor of a fixture chat, with the peer object usernames hang off.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Flavor {
    /// A private chat with this user; `Some` username arrives via
    /// `updateUser`.
    Private(i64, Option<&'static str>),
    /// A basic group (no username).
    Group,
    /// A supergroup with this id; `Some` username via `updateSupergroup`.
    Supergroup(i64, Option<&'static str>),
    /// A broadcast channel with this supergroup id and optional username.
    Channel(i64, Option<&'static str>),
    /// A secret chat (excluded from commits per POL-4).
    Secret,
}

/// One scripted chat: identity, flavor, metadata, and its position in every
/// list it appears in.
#[derive(Debug, Clone)]
struct FixtureChat {
    id: i64,
    flavor: Flavor,
    title: String,
    protected: bool,
    /// `(list, order, pinned)` — one entry per appearance.
    positions: Vec<(ChatListKind, i64, bool)>,
    /// `false`: never announced via `updateNewChat`; reachable only through
    /// the lazy `getChat` path.
    announce: bool,
}

impl FixtureChat {
    fn new(id: i64, title: &str) -> FixtureChat {
        FixtureChat {
            id,
            flavor: Flavor::Private(id + 1000, None),
            title: title.to_owned(),
            protected: false,
            positions: Vec::new(),
            announce: true,
        }
    }

    fn flavor(mut self, flavor: Flavor) -> FixtureChat {
        self.flavor = flavor;
        self
    }

    fn protected(mut self) -> FixtureChat {
        self.protected = true;
        self
    }

    fn at(mut self, list: ChatListKind, order: i64, pinned: bool) -> FixtureChat {
        self.positions.push((list, order, pinned));
        self
    }

    fn lazy(mut self) -> FixtureChat {
        self.announce = false;
        self
    }

    fn type_json(&self) -> Value {
        match self.flavor {
            Flavor::Private(user_id, _) => {
                json!({"@type": "chatTypePrivate", "user_id": user_id})
            }
            Flavor::Group => json!({"@type": "chatTypeBasicGroup", "basic_group_id": 9}),
            Flavor::Supergroup(id, _) => {
                json!({"@type": "chatTypeSupergroup", "supergroup_id": id, "is_channel": false})
            }
            Flavor::Channel(id, _) => {
                json!({"@type": "chatTypeSupergroup", "supergroup_id": id, "is_channel": true})
            }
            Flavor::Secret => json!({"@type": "chatTypeSecret", "secret_chat_id": 5}),
        }
    }

    /// The `updateUser`/`updateSupergroup` event carrying this chat's peer,
    /// when the flavor has one.
    fn peer_event(&self, client_id: i32) -> Option<String> {
        let usernames = |name: Option<&str>| match name {
            Some(name) => json!({"editable_username": name, "active_usernames": [name]}),
            None => json!({"editable_username": "", "active_usernames": []}),
        };
        match self.flavor {
            Flavor::Private(user_id, name) => Some(
                json!({
                    "@type": "updateUser",
                    "user": {"id": user_id, "usernames": usernames(name)},
                    "@client_id": client_id,
                })
                .to_string(),
            ),
            Flavor::Supergroup(id, name) | Flavor::Channel(id, name) => Some(
                json!({
                    "@type": "updateSupergroup",
                    "supergroup": {"id": id, "usernames": usernames(name)},
                    "@client_id": client_id,
                })
                .to_string(),
            ),
            Flavor::Group | Flavor::Secret => None,
        }
    }

    fn chat_json(&self, with_positions: bool) -> Value {
        let positions: Vec<Value> = if with_positions {
            self.positions
                .iter()
                .map(|(list, order, pinned)| position_json(*list, *order, *pinned))
                .collect()
        } else {
            Vec::new()
        };
        json!({
            "@type": "chat",
            "id": self.id,
            "type": self.type_json(),
            "title": self.title,
            "has_protected_content": self.protected,
            "positions": positions,
        })
    }
}

fn list_json(list: ChatListKind) -> Value {
    match list {
        ChatListKind::Main => json!({"@type": "chatListMain"}),
        ChatListKind::Archive => json!({"@type": "chatListArchive"}),
        ChatListKind::Stories => panic!("Stories is derived from storyListMain"),
        ChatListKind::Folder(folder) => {
            json!({"@type": "chatListFolder", "chat_folder_id": folder.0})
        }
    }
}

fn position_json(list: ChatListKind, order: i64, pinned: bool) -> Value {
    // int64 as a decimal string — the shape tdjson actually sends.
    json!({
        "@type": "chatPosition",
        "list": list_json(list),
        "order": order.to_string(),
        "is_pinned": pinned,
    })
}

/// A one-shot scripted failure: the next matching request fails with this
/// TDLib error instead of being served.
#[derive(Debug, Clone)]
struct ScriptedFailure {
    request_type: &'static str,
    code: i64,
    message: &'static str,
}

/// The deterministic TDLib double behind the mock's responder.
struct FixtureServer {
    chats: BTreeMap<i64, FixtureChat>,
    page_size_served: BTreeMap<String, usize>,
    announced: HashSet<i64>,
    failures: Vec<ScriptedFailure>,
}

impl FixtureServer {
    fn new(chats: Vec<FixtureChat>) -> FixtureServer {
        FixtureServer {
            chats: chats.into_iter().map(|chat| (chat.id, chat)).collect(),
            page_size_served: BTreeMap::new(),
            announced: HashSet::new(),
            failures: Vec::new(),
        }
    }

    fn fail_next(&mut self, request_type: &'static str, code: i64, message: &'static str) {
        self.failures.push(ScriptedFailure {
            request_type,
            code,
            message,
        });
    }

    /// The list's members in exact server order (pinned first, then order
    /// descending, then id descending), with their `(order, pinned)`.
    fn server_order(&self, list: ChatListKind) -> Vec<(i64, i64, bool)> {
        let mut members: Vec<(i64, i64, bool)> = self
            .chats
            .values()
            .filter_map(|chat| {
                chat.positions
                    .iter()
                    .find(|(l, order, _)| *l == list && *order != 0)
                    .map(|(_, order, pinned)| (chat.id, *order, *pinned))
            })
            .collect();
        members.sort_by(|a, b| b.2.cmp(&a.2).then(b.1.cmp(&a.1)).then(b.0.cmp(&a.0)));
        members
    }

    fn error_event(extra: u64, client_id: i32, code: i64, message: &str) -> String {
        json!({
            "@type": "error", "code": code, "message": message,
            "@extra": extra, "@client_id": client_id,
        })
        .to_string()
    }

    fn respond(&mut self, sent: &SentRequest) -> Vec<String> {
        let request: Value = serde_json::from_str(&sent.json).expect("requests are JSON");
        let request_type = request["@type"].as_str().expect("requests carry @type");
        let extra = sent.extra().expect("the runtime injects @extra");
        let client = sent.client_id;
        if let Some(at) = self
            .failures
            .iter()
            .position(|failure| failure.request_type == request_type)
        {
            let failure = self.failures.remove(at);
            return vec![Self::error_event(
                extra,
                client,
                failure.code,
                failure.message,
            )];
        }
        match request_type {
            "loadChats" => {
                let list = parse_list(&request["chat_list"]);
                let limit = request["limit"].as_u64().expect("loadChats carries limit") as usize;
                let members = self.server_order(list);
                let served = self
                    .page_size_served
                    .entry(format!("{list:?}"))
                    .or_insert(0);
                if *served >= members.len() {
                    return vec![Self::error_event(extra, client, 404, "Not Found")];
                }
                let page: Vec<(i64, i64, bool)> =
                    members[*served..].iter().take(limit).copied().collect();
                *served += page.len();
                let mut events = Vec::new();
                for (chat_id, order, pinned) in page {
                    let chat = self.chats.get(&chat_id).expect("member exists").clone();
                    if !chat.announce {
                        continue;
                    }
                    if self.announced.insert(chat_id) {
                        if let Some(peer) = chat.peer_event(client) {
                            events.push(peer);
                        }
                        events.push(
                            json!({
                                "@type": "updateNewChat",
                                "chat": chat.chat_json(false),
                                "@client_id": client,
                            })
                            .to_string(),
                        );
                    }
                    events.push(
                        json!({
                            "@type": "updateChatPosition",
                            "chat_id": chat_id,
                            "position": position_json(list, order, pinned),
                            "@client_id": client,
                        })
                        .to_string(),
                    );
                }
                events.push(common::ok_response(extra, client));
                events
            }
            "getChats" => {
                let list = parse_list(&request["chat_list"]);
                let limit = request["limit"].as_u64().expect("getChats carries limit") as usize;
                let members = self.server_order(list);
                let ids: Vec<i64> = members.iter().map(|(id, _, _)| *id).take(limit).collect();
                vec![
                    json!({
                        "@type": "chats",
                        "total_count": members.len(),
                        "chat_ids": ids,
                        "@extra": extra, "@client_id": client,
                    })
                    .to_string(),
                ]
            }
            "getChat" => {
                let chat_id = request["chat_id"]
                    .as_i64()
                    .expect("getChat carries chat_id");
                match self.chats.get(&chat_id).cloned() {
                    None => vec![Self::error_event(extra, client, 400, "CHAT_ID_INVALID")],
                    Some(chat) => {
                        let mut events = Vec::new();
                        if let Some(peer) = chat.peer_event(client) {
                            events.push(peer);
                        }
                        let mut answer = chat.chat_json(true);
                        answer["@extra"] = json!(extra);
                        answer["@client_id"] = json!(client);
                        events.push(answer.to_string());
                        events
                    }
                }
            }
            other => panic!("the snapshot issued an unexpected request type {other}"),
        }
    }
}

fn parse_list(value: &Value) -> ChatListKind {
    match value["@type"].as_str().expect("chat_list carries @type") {
        "chatListMain" => ChatListKind::Main,
        "chatListArchive" => ChatListKind::Archive,
        "chatListFolder" => ChatListKind::Folder(FolderId(
            value["chat_folder_id"].as_i64().expect("folder id") as i32,
        )),
        other => panic!("unexpected chat list {other}"),
    }
}

// ---------------------------------------------------------------------------
// Driving and persistence
// ---------------------------------------------------------------------------

/// Drain everything the update stream already buffered into the machine.
fn pump_updates(machine: &mut SnapshotMachine, updates: &UpdateStream) {
    while let Ok(update) = updates.try_recv() {
        machine.on_update(&update);
    }
}

/// What a completed drive observed.
#[derive(Debug, Default)]
struct DriveLog {
    backoffs: Vec<SnapshotBackoff>,
    commits: Vec<ListCommit>,
}

/// Drive the machine until `Done`, a scripted interruption, or an error:
/// submit each request, wait for its response, pump buffered updates first
/// (arrival order), then feed the response. `stop_after` commits are
/// applied before the drive stops early — the interruption fixture.
fn drive(
    machine: &mut SnapshotMachine,
    client: &TdClient,
    updates: &UpdateStream,
    stop_after: Option<usize>,
    mut apply: impl FnMut(&ListCommit),
) -> Result<DriveLog, SnapshotError> {
    let mut log = DriveLog::default();
    loop {
        match machine.next_step()? {
            SnapshotStep::Submit(request) => {
                let pending = client.request(request).expect("request submits");
                let outcome = pending
                    .wait_timeout(GUARD)
                    .unwrap_or_else(|_| panic!("a fixture response must arrive within the guard"));
                pump_updates(machine, updates);
                machine.on_response(outcome)?;
            }
            SnapshotStep::Backoff(backoff) => {
                // Sans-IO: the machine never sleeps, and a deterministic
                // test does not either — the advice is recorded instead.
                log.backoffs.push(backoff);
            }
            SnapshotStep::Commit(commit) => {
                apply(&commit);
                log.commits.push(*commit);
                if stop_after.is_some_and(|stop| log.commits.len() >= stop) {
                    return Ok(log);
                }
            }
            SnapshotStep::Done => return Ok(log),
        }
    }
}

/// A store with the snapshot's account registered.
fn store_with_account() -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open in-memory store");
    let tx = store.write_txn().expect("write txn");
    tx.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Test Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        display_timezone: "UTC".to_owned(),
        retention_mode: RetentionMode::Mirror,
        archive_mode: false,
        secret_ref: None,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
    })
    .expect("account row");
    tx.commit().expect("commit account");
    store
}

/// Persist one commit exactly as a composing caller must: canonical chat
/// rows, the list's membership, and the resume-token cursor, atomically
/// (SYNC-022).
fn apply_commit(store: &mut StateStore, commit: &ListCommit) {
    let tx = store.write_txn().expect("write txn");
    for chat in &commit.chats {
        tx.upsert_chat(&ChatRecord {
            key: ChatKey {
                scope: scope(),
                chat_id: ChatId(chat.chat_id),
            },
            chat_type: match chat.kind {
                SnapshotChatKind::Private => ChatType::Private,
                SnapshotChatKind::Group => ChatType::Group,
                SnapshotChatKind::Supergroup => ChatType::Supergroup,
                SnapshotChatKind::Channel => ChatType::Channel,
            },
            title: chat.title.clone(),
            username: chat.username.clone(),
            is_protected: chat.is_protected,
            archive_mode: false,
            metadata_version: MetadataVersion::new("s1").expect("valid token"),
            left_at_ms: None,
            deleted_at_ms: None,
            last_update_at_ms: Some(1_000),
        })
        .expect("chat row upserts");
    }
    let entries: Vec<ChatListEntry> = commit
        .entries
        .iter()
        .map(|entry| ChatListEntry {
            chat_id: ChatId(entry.chat_id),
            sort_order: entry.sort_order,
            pinned: entry.pinned,
        })
        .collect();
    tx.replace_chat_list(
        &ChatListKey {
            scope: scope(),
            kind: commit.list,
        },
        &entries,
    )
    .expect("list membership replaces");
    let cursor =
        ChangeCursor::new(scope(), commit.resume_token.clone()).expect("token fits a cursor");
    tx.put_cursor(SNAPSHOT_CURSOR_STREAM, &cursor, 1_000)
        .expect("cursor persists");
    tx.commit().expect("commit transaction");
}

/// The persisted resume token, restored the way a resuming caller must:
/// through the cursor repository with its SYNC-004 scope check.
fn stored_token(store: &mut StateStore) -> Vec<u8> {
    let tx = store.read_txn().expect("read txn");
    let cursor = tx
        .cursor(scope(), SNAPSHOT_CURSOR_STREAM)
        .expect("cursor reads")
        .expect("cursor exists");
    cursor.payload().to_vec()
}

fn read_list(store: &mut StateStore, kind: ChatListKind) -> Vec<ChatListEntry> {
    let tx = store.read_txn().expect("read txn");
    tx.chat_list(&ChatListKey {
        scope: scope(),
        kind,
    })
    .expect("list reads")
}

fn read_chat(store: &mut StateStore, chat_id: i64) -> ChatRecord {
    let tx = store.read_txn().expect("read txn");
    tx.chat(&ChatKey {
        scope: scope(),
        chat_id: ChatId(chat_id),
    })
    .expect("chat reads")
    .expect("chat exists")
}

/// Snapshot fixtures need headroom above the default queue capacity only in
/// the large suite; using it everywhere keeps the config single.
fn snapshot_config() -> RuntimeConfig {
    RuntimeConfig {
        update_queue_capacity: 8192,
        ..test_config()
    }
}

type ServerHandle = Arc<Mutex<FixtureServer>>;

/// Wire a fixture server behind a fresh runtime; returns the driving ends.
fn start_fixture(
    chats: Vec<FixtureChat>,
) -> (
    gramdrive_source_tdjson::TdRuntime,
    gramdrive_source_tdjson::mock::MockHandle,
    TdClient,
    UpdateStream,
    ServerHandle,
) {
    let (runtime, handle) = start_runtime(snapshot_config());
    let server = Arc::new(Mutex::new(FixtureServer::new(chats)));
    let responder = Arc::clone(&server);
    handle.set_responder(move |sent| {
        responder
            .lock()
            .expect("fixture lock is never poisoned")
            .respond(sent)
    });
    let (client, updates) = runtime.create_client().expect("client registers");
    (runtime, handle, client, updates, server)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// The medium fixture: Main, Archive, and one folder; pinned chats, every
/// flavor, a protected chat, a multi-list appearance, a lazy chat, and a
/// secret chat.
fn medium_fixture() -> Vec<FixtureChat> {
    vec![
        // Pinned head of Main; also appears in the folder — one canonical
        // chat, two appearances (PRD-013).
        FixtureChat::new(101, "Alice")
            .flavor(Flavor::Private(1101, Some("alice")))
            .at(ChatListKind::Main, 9_000, true)
            .at(folder_list(), 4_000, false),
        FixtureChat::new(102, "Work Group")
            .flavor(Flavor::Group)
            .at(ChatListKind::Main, 8_000, false),
        FixtureChat::new(103, "News Channel")
            .flavor(Flavor::Channel(2103, Some("dailynews")))
            .protected()
            .at(ChatListKind::Main, 7_000, false),
        // Never announced via updateNewChat: only the lazy getChat path
        // can resolve it.
        FixtureChat::new(104, "Lazy Supergroup")
            .flavor(Flavor::Supergroup(2104, Some("lazygroup")))
            .at(ChatListKind::Main, 6_000, false)
            .lazy(),
        // A secret chat in Main: excluded from the commit, counted.
        FixtureChat::new(105, "Secret")
            .flavor(Flavor::Secret)
            .at(ChatListKind::Main, 5_000, false),
        // Archive members; int64 orders beyond int53 exercise the string
        // wire shape.
        FixtureChat::new(201, "Archived Bot")
            .flavor(Flavor::Private(1201, None))
            .at(ChatListKind::Archive, 4_611_686_018_427_387_904, false),
        FixtureChat::new(202, "Archived Channel")
            .flavor(Flavor::Channel(2202, None))
            .at(ChatListKind::Archive, 3_000, false),
        // Folder-only member.
        FixtureChat::new(301, "Folder Friend")
            .flavor(Flavor::Private(1301, Some("folderfriend")))
            .at(folder_list(), 5_000, true),
    ]
}

fn full_plan() -> SnapshotPlan {
    SnapshotPlan::new(vec![
        ChatListKind::Main,
        ChatListKind::Archive,
        folder_list(),
    ])
}

/// The large synthetic fixture: `main` chats in Main (a pinned head and a
/// long tail across many pages) and `archive` chats in Archive.
fn large_fixture(main: i64, archive: i64) -> Vec<FixtureChat> {
    let mut chats = Vec::new();
    for n in 0..main {
        let id = 10_000 + n;
        let pinned = n < 7;
        // Orders descend as ids ascend, with pinned orders above the tail —
        // mirroring TDLib's pinned-first order space.
        let order = if pinned {
            1_000_000_000 + n
        } else {
            500_000_000 - n
        };
        let mut chat = FixtureChat::new(id, &format!("Chat {n}"))
            .flavor(Flavor::Private(100_000 + n, None))
            .at(ChatListKind::Main, order, pinned);
        // A sparse tail of lazy chats keeps the getChat path hot at scale.
        if n % 97 == 0 {
            chat = chat.lazy();
        }
        chats.push(chat);
    }
    for n in 0..archive {
        let id = 50_000 + n;
        chats.push(
            FixtureChat::new(id, &format!("Archived {n}"))
                .flavor(Flavor::Private(200_000 + n, None))
                .at(ChatListKind::Archive, 400_000_000 - n, false),
        );
    }
    chats
}

/// Assert the persisted list matches the fixture's server order exactly —
/// same members, same sequence, same ordering metadata, no duplicates.
fn assert_list_exact(store: &mut StateStore, server: &ServerHandle, list: ChatListKind) {
    let persisted = read_list(store, list);
    let expected: Vec<(i64, i64, bool)> = {
        let guard = server.lock().expect("fixture lock");
        guard
            .server_order(list)
            .into_iter()
            // Secret chats are excluded from persistence by design.
            .filter(|(id, _, _)| guard.chats[id].flavor != Flavor::Secret)
            .collect()
    };
    let got: Vec<(i64, i64, bool)> = persisted
        .iter()
        .map(|entry| (entry.chat_id.0, entry.sort_order, entry.pinned))
        .collect();
    assert_eq!(got, expected, "exact server order for {list:?}");
    let unique: HashSet<i64> = persisted.iter().map(|entry| entry.chat_id.0).collect();
    assert_eq!(unique.len(), persisted.len(), "no duplicates in {list:?}");
}

// ---------------------------------------------------------------------------
// Suites
// ---------------------------------------------------------------------------

#[test]
fn full_snapshot_persists_exact_order_metadata_and_appearances() {
    let (_runtime, handle, client, updates, server) = start_fixture(medium_fixture());
    let mut store = store_with_account();
    let mut machine = SnapshotMachine::new(full_plan()).expect("valid plan");
    let log = drive(&mut machine, &client, &updates, None, |commit| {
        apply_commit(&mut store, commit)
    })
    .expect("snapshot completes");

    assert_eq!(log.commits.len(), 3, "one commit per planned list");
    assert!(log.backoffs.is_empty());

    // Exact order and ordering metadata per list, straight from the store.
    for list in [ChatListKind::Main, ChatListKind::Archive, folder_list()] {
        assert_list_exact(&mut store, &server, list);
    }

    // Canonical metadata: flavor, title, protection, and the usernames the
    // load pushed — including the lazily resolved chat's.
    let alice = read_chat(&mut store, 101);
    assert_eq!(alice.chat_type, ChatType::Private);
    assert_eq!(alice.title, "Alice");
    assert_eq!(alice.username.as_deref(), Some("alice"));
    assert!(!alice.is_protected);
    let news = read_chat(&mut store, 103);
    assert_eq!(news.chat_type, ChatType::Channel);
    assert!(news.is_protected);
    assert_eq!(news.username.as_deref(), Some("dailynews"));
    let lazy = read_chat(&mut store, 104);
    assert_eq!(lazy.chat_type, ChatType::Supergroup);
    assert_eq!(lazy.title, "Lazy Supergroup");
    assert_eq!(lazy.username.as_deref(), Some("lazygroup"));
    let bot = read_chat(&mut store, 201);
    assert_eq!(bot.username, None);

    // Normalized appearances (PRD-013): chat 101 appears in Main and the
    // folder as two membership rows over one canonical record.
    let main_ids: Vec<i64> = read_list(&mut store, ChatListKind::Main)
        .iter()
        .map(|entry| entry.chat_id.0)
        .collect();
    let folder_ids: Vec<i64> = read_list(&mut store, folder_list())
        .iter()
        .map(|entry| entry.chat_id.0)
        .collect();
    assert!(main_ids.contains(&101) && folder_ids.contains(&101));

    // The secret chat is excluded and counted, never persisted (POL-4).
    let main_commit = &log.commits[0];
    assert_eq!(main_commit.list, ChatListKind::Main);
    assert_eq!(main_commit.excluded_secret, 1);
    assert_eq!(main_commit.excluded_unsupported, 0);
    assert!(!main_ids.contains(&105));

    // SYNC-020: metadata only — the whole request surface is the three
    // snapshot requests; nothing touched history or media.
    let kinds: HashSet<String> = handle
        .take_sent()
        .iter()
        .filter_map(SentRequest::request_type)
        .collect();
    assert_eq!(
        kinds,
        HashSet::from([
            "loadChats".to_owned(),
            "getChats".to_owned(),
            "getChat".to_owned(),
        ])
    );
}

#[test]
fn large_snapshot_interrupts_and_resumes_without_duplicates_or_gaps() {
    let fixture = large_fixture(1_500, 300);
    let plan = SnapshotPlan {
        lists: vec![ChatListKind::Main, ChatListKind::Archive],
        page_size: 128,
    };
    let mut store = store_with_account();

    // First run: interrupted after Main commits — mid-snapshot, with the
    // Archive list untouched.
    {
        let (_runtime, _handle, client, updates, _server) = start_fixture(fixture.clone());
        let mut machine = SnapshotMachine::new(plan.clone()).expect("valid plan");
        let log = drive(&mut machine, &client, &updates, Some(1), |commit| {
            apply_commit(&mut store, commit)
        })
        .expect("first run reaches the interruption point");
        assert_eq!(log.commits.len(), 1);
        assert_eq!(log.commits[0].list, ChatListKind::Main);
        assert_eq!(log.commits[0].entries.len(), 1_500, "no gaps in Main");
    }
    assert!(read_list(&mut store, ChatListKind::Archive).is_empty());

    // Second run resumes from the durable cursor: Main is skipped entirely,
    // Archive completes.
    let token = stored_token(&mut store);
    let (_runtime, handle, client, updates, server) = start_fixture(fixture);
    let mut machine = SnapshotMachine::resume(plan, &token).expect("token accepted");
    let log = drive(&mut machine, &client, &updates, None, |commit| {
        apply_commit(&mut store, commit)
    })
    .expect("resumed run completes");
    assert_eq!(log.commits.len(), 1, "only the pending list runs");
    assert_eq!(log.commits[0].list, ChatListKind::Archive);

    // The resumed run never touched Main: every list-level request names
    // the archive list only.
    for sent in handle.take_sent() {
        let request: Value = serde_json::from_str(&sent.json).expect("JSON");
        if let Some(chat_list) = request.get("chat_list") {
            assert_eq!(
                chat_list["@type"].as_str(),
                Some("chatListArchive"),
                "resume must skip the committed Main list: {request}"
            );
        }
    }

    // No duplicates, no gaps, exact order — on both lists.
    assert_list_exact(&mut store, &server, ChatListKind::Main);
    assert_list_exact(&mut store, &server, ChatListKind::Archive);
    assert_eq!(read_list(&mut store, ChatListKind::Main).len(), 1_500);
    assert_eq!(read_list(&mut store, ChatListKind::Archive).len(), 300);

    // The final cursor names both lists as done.
    let final_token = stored_token(&mut store);
    let resumed = SnapshotMachine::resume(
        SnapshotPlan::new(vec![ChatListKind::Main, ChatListKind::Archive]),
        &final_token,
    );
    assert!(resumed.is_ok(), "final token stays readable");
}

#[test]
fn flood_wait_and_transport_failures_back_off_and_retry() {
    let chats = vec![
        FixtureChat::new(101, "Alice")
            .flavor(Flavor::Private(1101, Some("alice")))
            .at(ChatListKind::Main, 9_000, true),
        FixtureChat::new(102, "Bob").at(ChatListKind::Main, 8_000, false),
    ];
    let (_runtime, _handle, client, updates, server) = start_fixture(chats);
    server.lock().expect("fixture lock").fail_next(
        "loadChats",
        429,
        "Too Many Requests: retry after 7",
    );
    server
        .lock()
        .expect("fixture lock")
        .fail_next("getChats", 500, "Failed to connect");

    let mut store = store_with_account();
    let mut machine =
        SnapshotMachine::new(SnapshotPlan::new(vec![ChatListKind::Main])).expect("valid plan");
    let log = drive(&mut machine, &client, &updates, None, |commit| {
        apply_commit(&mut store, commit)
    })
    .expect("snapshot completes despite the failures");

    assert_eq!(
        log.backoffs,
        vec![
            SnapshotBackoff {
                retry_after_secs: Some(7),
                attempt: 1,
            },
            SnapshotBackoff {
                retry_after_secs: None,
                attempt: 1,
            },
        ],
        "flood wait carries Telegram's stated delay; transport carries none"
    );
    assert_list_exact(&mut store, &server, ChatListKind::Main);
}

#[test]
fn concurrent_removal_during_load_is_excluded_not_a_gap() {
    // Chat 102 is loaded normally, then an explicit order-0 position —
    // "left the list" — arrives before the order witness is processed.
    let chats = vec![
        FixtureChat::new(101, "Alice").at(ChatListKind::Main, 9_000, false),
        FixtureChat::new(102, "Leaver").at(ChatListKind::Main, 8_000, false),
    ];
    let (_runtime, handle, client, updates, _server) = start_fixture(chats);
    let mut store = store_with_account();
    let mut machine =
        SnapshotMachine::new(SnapshotPlan::new(vec![ChatListKind::Main])).expect("valid plan");

    // Drive manually so the removal lands between the load and the witness.
    let mut removed = false;
    let log = loop {
        match machine.next_step().expect("machine healthy") {
            SnapshotStep::Submit(request) => {
                if request["@type"] == "getChats" && !removed {
                    removed = true;
                    handle.push_event(
                        &json!({
                            "@type": "updateChatPosition",
                            "chat_id": 102,
                            "position": {
                                "@type": "chatPosition",
                                "list": {"@type": "chatListMain"},
                                "order": "0",
                                "is_pinned": false,
                            },
                            "@client_id": client.client_id(),
                        })
                        .to_string(),
                    );
                }
                let pending = client.request(request).expect("request submits");
                let outcome = pending
                    .wait_timeout(GUARD)
                    .unwrap_or_else(|_| panic!("response within the guard"));
                pump_updates(&mut machine, &updates);
                machine.on_response(outcome).expect("response accepted");
            }
            SnapshotStep::Backoff(_) => {}
            SnapshotStep::Commit(commit) => {
                apply_commit(&mut store, &commit);
                break *commit;
            }
            SnapshotStep::Done => panic!("commit must arrive before done"),
        }
    };

    assert_eq!(log.excluded_removed, 1, "the leaver is counted, not a gap");
    let ids: Vec<i64> = read_list(&mut store, ChatListKind::Main)
        .iter()
        .map(|entry| entry.chat_id.0)
        .collect();
    assert_eq!(ids, vec![101], "the removed chat is not persisted");
}

#[test]
fn duplicate_witness_and_unresolvable_gap_are_contract_failures() {
    // Duplicate id in the order witness (SYNC-003).
    {
        let (_runtime, handle, client, _updates, _server) =
            start_fixture(vec![FixtureChat::new(101, "Alice").at(
                ChatListKind::Main,
                9_000,
                false,
            )]);
        handle.set_responder(move |sent: &SentRequest| {
            let request: Value = serde_json::from_str(&sent.json).expect("JSON");
            let extra = sent.extra().expect("extra");
            let client_id = sent.client_id;
            match request["@type"].as_str().expect("@type") {
                "loadChats" => vec![FixtureServer::error_event(
                    extra,
                    client_id,
                    404,
                    "Not Found",
                )],
                "getChats" => vec![
                    json!({
                        "@type": "chats", "total_count": 2, "chat_ids": [101, 101],
                        "@extra": extra, "@client_id": client_id,
                    })
                    .to_string(),
                ],
                other => panic!("unexpected {other}"),
            }
        });
        let mut machine =
            SnapshotMachine::new(SnapshotPlan::new(vec![ChatListKind::Main])).expect("valid plan");
        let err = drive(&mut machine, &client, &_updates, None, |_| {})
            .expect_err("duplicate listing must fail");
        assert_eq!(err, SnapshotError::DuplicateListing { chat_id: 101 });
        // Terminal: the machine keeps reporting the same failure.
        assert_eq!(
            machine.next_step().expect_err("machine stays failed"),
            SnapshotError::DuplicateListing { chat_id: 101 }
        );
    }

    // A witnessed chat that even lazy resolution cannot place in the list
    // is an explicit gap (SYNC-003/SYNC-023), never a silent skip.
    {
        let (_runtime, handle, client, _updates, _server) = start_fixture(Vec::new());
        handle.set_responder(move |sent: &SentRequest| {
            let request: Value = serde_json::from_str(&sent.json).expect("JSON");
            let extra = sent.extra().expect("extra");
            let client_id = sent.client_id;
            match request["@type"].as_str().expect("@type") {
                "loadChats" => vec![FixtureServer::error_event(
                    extra,
                    client_id,
                    404,
                    "Not Found",
                )],
                "getChats" => vec![
                    json!({
                        "@type": "chats", "total_count": 1, "chat_ids": [999],
                        "@extra": extra, "@client_id": client_id,
                    })
                    .to_string(),
                ],
                "getChat" => vec![
                    // A chat object with no position for Main.
                    json!({
                        "@type": "chat", "id": 999,
                        "type": {"@type": "chatTypePrivate", "user_id": 1},
                        "title": "Ghost", "positions": [],
                        "@extra": extra, "@client_id": client_id,
                    })
                    .to_string(),
                ],
                other => panic!("unexpected {other}"),
            }
        });
        let mut machine =
            SnapshotMachine::new(SnapshotPlan::new(vec![ChatListKind::Main])).expect("valid plan");
        let err = drive(&mut machine, &client, &_updates, None, |_| {})
            .expect_err("an unresolvable listing must fail");
        assert_eq!(err, SnapshotError::MissingPosition { chat_id: 999 });
    }
}

#[test]
fn fatal_request_errors_poison_the_machine_with_typed_detail() {
    let (_runtime, _handle, client, updates, server) =
        start_fixture(vec![FixtureChat::new(101, "Alice").at(
            ChatListKind::Main,
            9_000,
            false,
        )]);
    server
        .lock()
        .expect("fixture lock")
        .fail_next("loadChats", 401, "AUTH_KEY_UNREGISTERED");
    let mut machine =
        SnapshotMachine::new(SnapshotPlan::new(vec![ChatListKind::Main])).expect("valid plan");
    let err = drive(&mut machine, &client, &updates, None, |_| {})
        .expect_err("a fatal rejection must fail the run");
    assert_eq!(
        err,
        SnapshotError::Request {
            request: gramdrive_source_tdjson::SnapshotRequest::LoadChats,
            source: TdError::Td {
                code: 401,
                message: "AUTH_KEY_UNREGISTERED".to_owned(),
            },
        }
    );
}

#[test]
fn empty_lists_commit_empty_membership() {
    let (_runtime, _handle, client, updates, _server) = start_fixture(Vec::new());
    let mut store = store_with_account();
    let mut machine = SnapshotMachine::new(full_plan()).expect("valid plan");
    let log = drive(&mut machine, &client, &updates, None, |commit| {
        apply_commit(&mut store, commit)
    })
    .expect("empty snapshot completes");
    assert_eq!(log.commits.len(), 3);
    for commit in &log.commits {
        assert!(commit.entries.is_empty() && commit.chats.is_empty());
    }
    assert!(read_list(&mut store, ChatListKind::Main).is_empty());
    // The cursor still records completion, so a resume skips everything.
    let token = stored_token(&mut store);
    let mut resumed = SnapshotMachine::resume(full_plan(), &token).expect("token accepted");
    assert!(matches!(
        resumed.next_step().expect("healthy"),
        SnapshotStep::Done
    ));
}

#[test]
fn multi_page_snapshot_recovers_ordering_from_string_int64_orders() {
    // Orders near int64's ceiling only survive the string wire shape; a
    // float round-trip would corrupt them and scramble the list.
    let big = i64::MAX - 3;
    let chats: Vec<FixtureChat> = (0..5)
        .map(|n| {
            FixtureChat::new(100 + n, &format!("Big {n}")).at(ChatListKind::Main, big - n, false)
        })
        .collect();
    let (_runtime, _handle, client, updates, server) = start_fixture(chats);
    let mut store = store_with_account();
    let mut machine = SnapshotMachine::new(SnapshotPlan {
        lists: vec![ChatListKind::Main],
        page_size: 2,
    })
    .expect("valid plan");
    drive(&mut machine, &client, &updates, None, |commit| {
        apply_commit(&mut store, commit)
    })
    .expect("snapshot completes");
    assert_list_exact(&mut store, &server, ChatListKind::Main);
    let persisted = read_list(&mut store, ChatListKind::Main);
    assert_eq!(persisted[0].sort_order, big, "int64 order survives exactly");
}
