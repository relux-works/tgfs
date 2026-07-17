//! The initial chat-list snapshot machine: TDLib's chat-list loading
//! protocol becomes deterministic, resumable, per-list commits
//! (TASK-260715-30amrq).
//!
//! # What a snapshot is
//!
//! The first thing the local Telegram source does after authorization is
//! discover *what exists*: every chat's canonical metadata and every chat
//! list's exact membership and order — and nothing else. Per SYNC-020 the
//! snapshot never touches history or media; the only requests it issues are
//! `loadChats`, `getChats`, and `getChat`. History traversal
//! (TASK-260715-26dnp6) and live updates (TASK-260715-1c8fea) build on the
//! baseline this machine establishes.
//!
//! # Shape: sans-IO, like [`AuthMachine`](crate::auth::AuthMachine)
//!
//! [`SnapshotMachine`] performs no I/O, holds no client handle, and reads
//! no clock. The caller owns the wiring, one obligation at a time:
//!
//! 1. [`SnapshotMachine::next_step`] names the current obligation —
//!    [`SnapshotStep::Submit`] a request, wait out a
//!    [`SnapshotStep::Backoff`], persist a [`SnapshotStep::Commit`], or
//!    stop at [`SnapshotStep::Done`]. Calling it again without acting
//!    returns the same obligation; it never advances state by itself.
//! 2. Every update from the client's
//!    [`UpdateStream`](crate::runtime::UpdateStream) is fed to
//!    [`SnapshotMachine::on_update`] — the machine consumes
//!    `updateNewChat`, `updateChatPosition`, `updateUser`, and
//!    `updateSupergroup`, and ignores the rest. Feed updates that arrived
//!    before a response *before* feeding the response, which is the order
//!    the runtime delivered them in.
//! 3. The outcome of a submitted request is fed to
//!    [`SnapshotMachine::on_response`].
//!
//! Everything the machine hands the caller is typed, provider-neutral
//! vocabulary; no TDLib JSON crosses outward except the requests the caller
//! ferries verbatim (the DEC-003 direction the auth machine set).
//!
//! # The protocol per list
//!
//! For each planned list (Main, Archive, custom folders — the folder
//! catalog itself is TASK-260715-54nopz's discovery; this machine
//! snapshots whatever lists it is given):
//!
//! 1. **Load.** `loadChats(list, page_size)` repeatedly — TDLib pushes the
//!    loaded chats (`updateNewChat`) and their list positions
//!    (`updateChatPosition`) and answers `ok` per page, then error `404`
//!    when the list is fully loaded. That error is the pagination
//!    terminator, not a failure.
//! 2. **Order witness.** `getChats(list, limit)` returns the loaded list's
//!    chat ids in exact server order. A duplicate id in that answer is a
//!    contract failure (SYNC-003), never silently deduplicated. A full
//!    answer (`len == limit`) is retried with a doubled limit, so a
//!    too-small guess cannot truncate the witness into a silent gap.
//! 3. **Lazy detail resolution.** Any chat the order witness (or a consumed
//!    position update) names that the machine lacks metadata or a position
//!    for is fetched with `getChat(chat_id)` — on demand, never eagerly,
//!    and never anything heavier than the chat object itself (SYNC-020).
//! 4. **Commit.** The machine assembles one [`ListCommit`]: canonical
//!    [`ChatSnapshot`] metadata plus [`ListEntrySnapshot`] rows carrying
//!    Telegram's exact ordering metadata — the opaque int64 `order` and the
//!    pinned flag (DEC-013/POL-1) — sorted pinned-first, then order
//!    descending, then chat id descending, which is both TDLib's list order
//!    and exactly how the state layer's `chat_list` read reproduces it.
//!
//! Membership truth is the *position map*, not the order witness: position
//! updates consumed after `getChats` answered are newer than the answer, so
//! a chat that demonstrably left the list (an explicit order-0 position) is
//! excluded and counted rather than resurrected — while a chat the witness
//! names that the machine knows nothing about even after `getChat` is a
//! gap, failed explicitly (SYNC-003; recovery is re-baselining, SYNC-023).
//!
//! # Resume: list-granular, because that is what TDLib offers
//!
//! `loadChats` has no offset — TDLib itself owns the load position, and its
//! local database is the page cache. The durable unit of progress is
//! therefore the *completed list*: every [`ListCommit`] carries a
//! [`ListCommit::resume_token`] naming the lists finished so far, and the
//! caller persists it in the same transaction as the commit's rows
//! (SYNC-022; the state layer's `ChangeCursor` under
//! [`SNAPSHOT_CURSOR_STREAM`] is the intended carrier, giving the SYNC-004
//! scope rejection for free). [`SnapshotMachine::resume`] skips the lists a
//! token names; an interrupted list restarts from its beginning, which
//! TDLib serves from local storage — re-enumeration, not re-download.
//! Re-running a list is idempotent by construction: canonical chat rows
//! upsert and list membership replaces atomically, so interruption can
//! produce neither duplicates nor gaps.
//!
//! # Flood wait and transient failures (SYNC-044)
//!
//! A request rejected with TDLib code 429 (`Too Many Requests` /
//! `FLOOD_WAIT`) or 500 (transport loss) does not fail the snapshot: the
//! machine arms one [`SnapshotStep::Backoff`] carrying Telegram's stated
//! delay when the message names one, then re-issues the identical request.
//! The machine never sleeps — the caller owns time — and never caps
//! attempts: [`SnapshotBackoff::attempt`] is exposed so the caller's policy
//! can. Every other rejection is fatal and typed
//! ([`SnapshotError::Request`]); the machine stays in the failed state and
//! the durable token is the recovery path.
//!
//! # Preconditions and boundaries
//!
//! The client must be authorized (the [`AuthMachine`](crate::auth) flow
//! reached `Ready`) before the snapshot starts. Usernames are captured
//! opportunistically from the `updateUser`/`updateSupergroup` objects TDLib
//! pushes alongside the chats it loads — no per-chat user/supergroup
//! requests are issued; keeping them current afterwards is the
//! metadata-update flow's job (TASK-260715-1c8fea, SYNC-026). Secret chats
//! are out of v1 scope (POL-4/DEC-016) and excluded from commits, counted
//! in [`ListCommit::excluded_secret`]; a chat type this build does not know
//! fails safe the same way ([`ListCommit::excluded_unsupported`]), never as
//! a panic or a guessed row.

use std::collections::{HashMap, HashSet, VecDeque};

use serde_json::{Value, json};

use gramdrive_model::identity::{ChatListKind, FolderId};

use crate::error::{TdError, trailing_integer};
use crate::wire::{KindFact, active_username, list_json, parse_chat_kind, parse_list, parse_order};

/// The cursor stream name the composing caller is expected to persist
/// snapshot resume tokens under (one cursor per account; SYNC-004).
pub const SNAPSHOT_CURSOR_STREAM: &str = "chat-list-snapshot";

/// `getChats` limit ceiling — TDLib's limit parameter is int32.
const MAX_ORDER_LIMIT: u32 = i32::MAX as u32;

/// Headroom added to the known member count when sizing the order witness,
/// so concurrent additions during the load do not force a second round.
const ORDER_LIMIT_MARGIN: u32 = 64;

/// Resume-token format version this build reads and writes.
const TOKEN_VERSION: u64 = 1;

/// Which lists one snapshot run covers, and how it pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotPlan {
    /// The lists to snapshot, in commit order. Duplicates are rejected at
    /// machine construction ([`SnapshotError::Plan`]).
    pub lists: Vec<ChatListKind>,
    /// `loadChats` limit per page. `0` is raised to `1`; the default is
    /// [`SnapshotPlan::DEFAULT_PAGE_SIZE`].
    pub page_size: u32,
}

impl SnapshotPlan {
    /// Default `loadChats` page size: large enough that a big account loads
    /// in few round trips, small enough that one page's update burst stays
    /// far below the runtime's default update-queue capacity.
    pub const DEFAULT_PAGE_SIZE: u32 = 100;

    /// A plan over `lists` with the default page size.
    pub fn new(lists: impl Into<Vec<ChatListKind>>) -> SnapshotPlan {
        SnapshotPlan {
            lists: lists.into(),
            page_size: Self::DEFAULT_PAGE_SIZE,
        }
    }
}

/// Telegram chat flavor as the snapshot normalizes it — the provider-facing
/// mirror of the state layer's chat-type vocabulary. Secret and unknown
/// chat types never reach this enum; they are excluded and counted at
/// assembly (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotChatKind {
    /// A one-on-one chat.
    Private,
    /// A basic group.
    Group,
    /// A supergroup.
    Supergroup,
    /// A broadcast channel.
    Channel,
}

/// Canonical metadata of one chat as observed by the snapshot — the facts
/// the caller upserts as the chat's canonical record (SYNC-026: identity is
/// the chat id; everything here is replaceable metadata).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatSnapshot {
    /// Telegram chat id (int53).
    pub chat_id: i64,
    /// Chat flavor.
    pub kind: SnapshotChatKind,
    /// Current title as observed.
    pub title: String,
    /// Public username, when TDLib pushed the owning user/supergroup object
    /// during the load and it carries one.
    pub username: Option<String>,
    /// Telegram's protected-content flag (POL-4).
    pub is_protected: bool,
}

/// Membership and exact server position of one chat in the committed list
/// (DEC-013): Telegram's opaque int64 sort key and the pinned flag,
/// verbatim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListEntrySnapshot {
    /// The member chat.
    pub chat_id: i64,
    /// Telegram's opaque sort position — larger sorts first.
    pub sort_order: i64,
    /// Whether the chat is pinned in this list.
    pub pinned: bool,
}

/// One completed list, ready to persist atomically: canonical chat rows,
/// ordered membership, and the durable resume token that must commit with
/// them (SYNC-022).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListCommit {
    /// The list this commit covers.
    pub list: ChatListKind,
    /// Canonical metadata for every entry's chat, in entry order. A chat
    /// appearing in several lists recurs in each list's commit; upserts
    /// keep the canonical record single (normalized appearances, PRD-013).
    pub chats: Vec<ChatSnapshot>,
    /// The list's membership in exact server order: pinned first, then
    /// order descending, then chat id descending.
    pub entries: Vec<ListEntrySnapshot>,
    /// TDLib's `total_count` from the order witness — diagnostic only;
    /// membership truth is `entries` (module docs).
    pub total_count: Option<u32>,
    /// Secret chats excluded from this list (POL-4: out of v1 scope).
    pub excluded_secret: u32,
    /// Chats of a type this build does not know, excluded fail-safe.
    pub excluded_unsupported: u32,
    /// Chats the order witness named that had demonstrably left the list
    /// (an explicit order-0 position observed) by assembly time.
    pub excluded_removed: u32,
    /// The durable progress token including this list. Persist it in the
    /// same transaction as the commit's rows; feed it back through
    /// [`SnapshotMachine::resume`] after an interruption.
    pub resume_token: Vec<u8>,
}

/// Flood-wait/transient-failure advice: wait, then call
/// [`SnapshotMachine::next_step`] again — it re-issues the identical
/// request (SYNC-044).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotBackoff {
    /// The wait Telegram stated, when its message carried one. `None` for
    /// a transport failure (code 500) or an unstated flood wait; the
    /// caller's retry policy owns the delay then.
    pub retry_after_secs: Option<u64>,
    /// How many times this request has failed retryably, starting at 1.
    /// The machine never caps attempts; a caller's policy can.
    pub attempt: u32,
}

/// The caller's current obligation, from [`SnapshotMachine::next_step`].
#[derive(Debug, Clone)]
pub enum SnapshotStep {
    /// Submit this request on the account's client and feed the outcome to
    /// [`SnapshotMachine::on_response`]. Submit it exactly once per
    /// returned step; `next_step` repeats the obligation until the
    /// response is fed.
    Submit(Value),
    /// The last request hit flood control or a transport failure: wait,
    /// then call `next_step` again to re-issue it.
    Backoff(SnapshotBackoff),
    /// A list is complete: persist the commit atomically (rows plus
    /// [`ListCommit::resume_token`]), then call `next_step` to continue.
    Commit(Box<ListCommit>),
    /// Every planned list is committed.
    Done,
}

/// Which snapshot request a failure belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotRequest {
    /// `loadChats` — one pagination step.
    LoadChats,
    /// `getChats` — the order witness.
    GetChats,
    /// `getChat` — lazy detail resolution.
    GetChat,
}

impl SnapshotRequest {
    fn as_str(self) -> &'static str {
        match self {
            SnapshotRequest::LoadChats => "loadChats",
            SnapshotRequest::GetChats => "getChats",
            SnapshotRequest::GetChat => "getChat",
        }
    }
}

/// Why the snapshot failed. Every variant is terminal for the machine —
/// [`SnapshotMachine::next_step`] keeps returning it — and recovery is a
/// fresh machine resumed from the last durable token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// The plan is invalid (duplicate lists).
    Plan {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// A resume token could not be accepted: unknown format version,
    /// unparseable bytes, or an unknown list token. Explicit rejection,
    /// never a silent fresh start (SYNC-004); the caller re-baselines by
    /// clearing the cursor and running a full snapshot.
    ResumeToken {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// A snapshot request failed with an error that is neither the
    /// `loadChats` end-of-list `404` nor retryable flood/transport advice.
    Request {
        /// Which request failed.
        request: SnapshotRequest,
        /// The failure as the runtime typed it.
        source: TdError,
    },
    /// A response or a strictly-required object did not have the shape the
    /// tdjson protocol promises.
    Malformed {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The order witness listed the same chat twice — a pagination
    /// contract failure (SYNC-003).
    DuplicateListing {
        /// The chat id listed more than once.
        chat_id: i64,
    },
    /// The order witness named a chat the machine could not place in the
    /// list even after lazy resolution — a gap (SYNC-003); the recovery is
    /// source-level re-baselining (SYNC-023).
    MissingPosition {
        /// The chat the list claims that has no position in it.
        chat_id: i64,
    },
    /// The caller fed a response while no request was outstanding.
    NoRequestOutstanding,
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SnapshotError::Plan { detail } => write!(f, "invalid snapshot plan: {detail}"),
            SnapshotError::ResumeToken { detail } => {
                write!(f, "snapshot resume token rejected: {detail}")
            }
            SnapshotError::Request { request, source } => {
                write!(f, "snapshot request {} failed: {source}", request.as_str())
            }
            SnapshotError::Malformed { detail } => {
                write!(f, "malformed snapshot data: {detail}")
            }
            SnapshotError::DuplicateListing { chat_id } => {
                write!(f, "chat {chat_id} listed twice by the order witness")
            }
            SnapshotError::MissingPosition { chat_id } => {
                write!(
                    f,
                    "chat {chat_id} is listed but has no position in the list"
                )
            }
            SnapshotError::NoRequestOutstanding => {
                write!(
                    f,
                    "a response was fed while no snapshot request was outstanding"
                )
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

// ---------------------------------------------------------------------------
// Observed facts
// ---------------------------------------------------------------------------

/// Canonical facts of one chat as last observed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatFacts {
    kind: KindFact,
    title: String,
    is_protected: bool,
}

/// One chat's last-observed position in one list. `Removed` records an
/// explicit order-0 position — knowledge that the chat left the list, which
/// must not be confused with never having heard of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionFact {
    Present { order: i64, pinned: bool },
    Removed,
}

// ---------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------

/// One request in flight (or awaiting re-issue after a backoff).
#[derive(Debug)]
struct Outstanding {
    request: SnapshotRequest,
    payload: Value,
    /// The chat a `getChat` resolves; `None` for the list-level requests.
    detail_chat: Option<i64>,
    /// Retryable failures so far.
    attempt: u32,
    /// Armed by a retryable failure; returned once by `next_step`.
    pending_backoff: Option<SnapshotBackoff>,
}

/// Where the machine stands inside the current list (or the run).
#[derive(Debug)]
enum Phase {
    /// Pick the next pending list, or finish the run.
    NextList,
    /// Paginating `loadChats` on the current list.
    Loading,
    /// Awaiting/issuing the order witness with this limit.
    Ordering { limit: u32 },
    /// Resolving details lazily; `order` and `total` are kept for assembly.
    Resolving {
        order: Vec<i64>,
        total: Option<u32>,
        queue: VecDeque<i64>,
    },
    /// A commit is assembled and waiting for the caller to take it.
    CommitReady(Box<ListCommit>),
    /// Every planned list is committed.
    Finished,
    /// Terminal failure; every call reports it.
    Failed(Box<SnapshotError>),
}

/// The deterministic initial chat-list snapshot machine for one authorized
/// account's client. Sans-IO; the caller owns the wiring (module docs).
#[derive(Debug)]
pub struct SnapshotMachine {
    lists: Vec<ChatListKind>,
    page_size: u32,
    done: HashSet<ChatListKind>,
    current: Option<ChatListKind>,
    phase: Phase,
    outstanding: Option<Outstanding>,
    facts: HashMap<i64, ChatFacts>,
    positions: HashMap<ChatListKind, HashMap<i64, PositionFact>>,
    user_names: HashMap<i64, Option<String>>,
    supergroup_names: HashMap<i64, Option<String>>,
}

impl SnapshotMachine {
    /// A fresh snapshot over `plan`, starting from nothing.
    pub fn new(plan: SnapshotPlan) -> Result<SnapshotMachine, SnapshotError> {
        let mut seen = HashSet::new();
        for list in &plan.lists {
            if !seen.insert(*list) {
                return Err(SnapshotError::Plan {
                    detail: format!("list {} appears twice", list_token(*list)),
                });
            }
        }
        Ok(SnapshotMachine {
            lists: plan.lists,
            page_size: plan.page_size.max(1),
            done: HashSet::new(),
            current: None,
            phase: Phase::NextList,
            outstanding: None,
            facts: HashMap::new(),
            positions: HashMap::new(),
            user_names: HashMap::new(),
            supergroup_names: HashMap::new(),
        })
    }

    /// A snapshot over `plan` resuming from a previously persisted
    /// [`ListCommit::resume_token`]: the lists the token names commit
    /// nothing again; the first unfinished list restarts from its
    /// beginning (module docs).
    ///
    /// A token naming a list the plan no longer contains is accepted — the
    /// entry is dropped from future tokens, and re-planning the list later
    /// simply re-runs it idempotently. An unreadable or unknown-version
    /// token is rejected explicitly, never treated as an empty one
    /// (SYNC-004).
    pub fn resume(plan: SnapshotPlan, token: &[u8]) -> Result<SnapshotMachine, SnapshotError> {
        let done = parse_token(token)?;
        let mut machine = SnapshotMachine::new(plan)?;
        machine.done = done
            .into_iter()
            .filter(|list| machine.lists.contains(list))
            .collect();
        Ok(machine)
    }

    /// The caller's current obligation. Idempotent: without an intervening
    /// [`SnapshotMachine::on_response`] or commit pickup, the same
    /// obligation is returned again (a [`SnapshotStep::Backoff`] is
    /// returned once per failure, then the re-issue).
    pub fn next_step(&mut self) -> Result<SnapshotStep, SnapshotError> {
        if let Phase::Failed(error) = &self.phase {
            return Err((**error).clone());
        }
        if let Some(outstanding) = &mut self.outstanding {
            if let Some(backoff) = outstanding.pending_backoff.take() {
                return Ok(SnapshotStep::Backoff(backoff));
            }
            return Ok(SnapshotStep::Submit(outstanding.payload.clone()));
        }
        loop {
            match &mut self.phase {
                Phase::NextList => {
                    let next = self
                        .lists
                        .iter()
                        .copied()
                        .find(|list| !self.done.contains(list));
                    match next {
                        None => {
                            self.phase = Phase::Finished;
                            return Ok(SnapshotStep::Done);
                        }
                        Some(list) => {
                            self.current = Some(list);
                            self.phase = Phase::Loading;
                        }
                    }
                }
                Phase::Loading => {
                    let list = self.current_list()?;
                    let payload = json!({
                        "@type": "loadChats",
                        "chat_list": list_json(list),
                        "limit": self.page_size,
                    });
                    return Ok(self.submit(SnapshotRequest::LoadChats, payload, None));
                }
                Phase::Ordering { limit } => {
                    let limit = *limit;
                    let list = self.current_list()?;
                    let payload = json!({
                        "@type": "getChats",
                        "chat_list": list_json(list),
                        "limit": limit,
                    });
                    return Ok(self.submit(SnapshotRequest::GetChats, payload, None));
                }
                Phase::Resolving { queue, .. } => {
                    // Assembly happens in `on_response` the moment the queue
                    // drains, so a `Resolving` phase reached here always has
                    // work left.
                    let Some(chat_id) = queue.front().copied() else {
                        return Err(self.fail(SnapshotError::Malformed {
                            detail: "resolving phase with an empty queue".to_owned(),
                        }));
                    };
                    let payload = json!({"@type": "getChat", "chat_id": chat_id});
                    return Ok(self.submit(SnapshotRequest::GetChat, payload, Some(chat_id)));
                }
                Phase::CommitReady(_) => {
                    let Phase::CommitReady(commit) =
                        std::mem::replace(&mut self.phase, Phase::NextList)
                    else {
                        // The match arm above proves the variant.
                        return Err(self.fail(SnapshotError::Malformed {
                            detail: "commit phase changed underneath the machine".to_owned(),
                        }));
                    };
                    self.done.insert(commit.list);
                    self.current = None;
                    return Ok(SnapshotStep::Commit(commit));
                }
                Phase::Finished => return Ok(SnapshotStep::Done),
                Phase::Failed(error) => return Err((**error).clone()),
            }
        }
    }

    /// Feed the outcome of the request the last [`SnapshotStep::Submit`]
    /// named. Feeding a response with nothing outstanding is
    /// [`SnapshotError::NoRequestOutstanding`].
    pub fn on_response(&mut self, outcome: Result<Value, TdError>) -> Result<(), SnapshotError> {
        if let Phase::Failed(error) = &self.phase {
            return Err((**error).clone());
        }
        let Some(mut outstanding) = self.outstanding.take() else {
            return Err(self.fail(SnapshotError::NoRequestOutstanding));
        };
        match outcome {
            Ok(value) => match outstanding.request {
                SnapshotRequest::LoadChats => Ok(()),
                SnapshotRequest::GetChats => self.on_order_witness(&value),
                SnapshotRequest::GetChat => self.on_chat_detail(outstanding.detail_chat, &value),
            },
            Err(error) => {
                // The end-of-list 404 is `loadChats`'s pagination
                // terminator, not a failure.
                if outstanding.request == SnapshotRequest::LoadChats
                    && matches!(&error, TdError::Td { code, .. } if *code == 404)
                {
                    let limit = self.order_limit();
                    self.phase = Phase::Ordering { limit };
                    return Ok(());
                }
                if let Some(retry_after_secs) = retryable_after(&error) {
                    outstanding.attempt = outstanding.attempt.saturating_add(1);
                    outstanding.pending_backoff = Some(SnapshotBackoff {
                        retry_after_secs,
                        attempt: outstanding.attempt,
                    });
                    self.outstanding = Some(outstanding);
                    return Ok(());
                }
                Err(self.fail(SnapshotError::Request {
                    request: outstanding.request,
                    source: error,
                }))
            }
        }
    }

    /// Feed one update from the client's stream. The machine consumes
    /// `updateNewChat`, `updateChatPosition`, `updateUser`, and
    /// `updateSupergroup`; everything else is ignored. An unparseable
    /// consumed update is skipped — lazy `getChat` resolution is the
    /// safety net, and the response path validates strictly.
    pub fn on_update(&mut self, update: &Value) {
        match update.get("@type").and_then(Value::as_str) {
            Some("updateNewChat") => {
                if let Some(chat) = update.get("chat") {
                    let _ = self.ingest_chat(chat);
                }
            }
            Some("updateChatPosition") => {
                let Some(chat_id) = update.get("chat_id").and_then(Value::as_i64) else {
                    return;
                };
                if let Some(position) = update.get("position") {
                    self.ingest_position(chat_id, position);
                }
            }
            Some("updateUser") => {
                let Some(user) = update.get("user") else {
                    return;
                };
                let Some(user_id) = user.get("id").and_then(Value::as_i64) else {
                    return;
                };
                self.user_names.insert(user_id, active_username(user));
            }
            Some("updateSupergroup") => {
                let Some(supergroup) = update.get("supergroup") else {
                    return;
                };
                let Some(id) = supergroup.get("id").and_then(Value::as_i64) else {
                    return;
                };
                self.supergroup_names
                    .insert(id, active_username(supergroup));
            }
            _ => {}
        }
    }

    // -- internals ----------------------------------------------------------

    fn submit(
        &mut self,
        request: SnapshotRequest,
        payload: Value,
        detail_chat: Option<i64>,
    ) -> SnapshotStep {
        let step = SnapshotStep::Submit(payload.clone());
        self.outstanding = Some(Outstanding {
            request,
            payload,
            detail_chat,
            attempt: 0,
            pending_backoff: None,
        });
        step
    }

    fn fail(&mut self, error: SnapshotError) -> SnapshotError {
        self.phase = Phase::Failed(Box::new(error.clone()));
        error
    }

    fn current_list(&mut self) -> Result<ChatListKind, SnapshotError> {
        match self.current {
            Some(list) => Ok(list),
            None => Err(self.fail(SnapshotError::Malformed {
                detail: "no current list mid-phase".to_owned(),
            })),
        }
    }

    /// The order-witness limit: every member the machine already knows
    /// about, plus margin for concurrent additions.
    fn order_limit(&self) -> u32 {
        let known = self
            .current
            .and_then(|list| self.positions.get(&list))
            .map(|map| {
                map.values()
                    .filter(|fact| matches!(fact, PositionFact::Present { .. }))
                    .count()
            })
            .unwrap_or(0);
        u32::try_from(known)
            .unwrap_or(MAX_ORDER_LIMIT)
            .saturating_add(ORDER_LIMIT_MARGIN)
            .min(MAX_ORDER_LIMIT)
    }

    fn on_order_witness(&mut self, value: &Value) -> Result<(), SnapshotError> {
        let list = self.current_list()?;
        let Phase::Ordering { limit } = self.phase else {
            return Err(self.fail(SnapshotError::Malformed {
                detail: "order witness answered outside the ordering phase".to_owned(),
            }));
        };
        let Some(ids) = value.get("chat_ids").and_then(Value::as_array) else {
            return Err(self.fail(SnapshotError::Malformed {
                detail: "chats answer without a chat_ids array".to_owned(),
            }));
        };
        let mut order = Vec::with_capacity(ids.len());
        let mut seen = HashSet::with_capacity(ids.len());
        for id in ids {
            let Some(chat_id) = id.as_i64() else {
                return Err(self.fail(SnapshotError::Malformed {
                    detail: format!("non-integer chat id {id} in a chats answer"),
                }));
            };
            if !seen.insert(chat_id) {
                return Err(self.fail(SnapshotError::DuplicateListing { chat_id }));
            }
            order.push(chat_id);
        }
        // A full answer may be a truncated witness; retry with a doubled
        // limit until the answer is strictly shorter than the ask.
        if order.len() == limit as usize && limit < MAX_ORDER_LIMIT {
            self.phase = Phase::Ordering {
                limit: limit.saturating_mul(2).min(MAX_ORDER_LIMIT),
            };
            return Ok(());
        }
        let total = value
            .get("total_count")
            .and_then(Value::as_u64)
            .and_then(|total| u32::try_from(total).ok());
        // Lazy resolution covers two shapes of ignorance: a witnessed chat
        // with no metadata or no position for this list, and a chat known
        // only from a position update (no `updateNewChat` seen).
        let position_map = self.positions.get(&list);
        let mut queue = VecDeque::new();
        let mut queued = HashSet::new();
        for &chat_id in &order {
            let has_position = position_map.is_some_and(|map| map.contains_key(&chat_id));
            if (!self.facts.contains_key(&chat_id) || !has_position) && queued.insert(chat_id) {
                queue.push_back(chat_id);
            }
        }
        if let Some(map) = position_map {
            for (&chat_id, fact) in map {
                if matches!(fact, PositionFact::Present { .. })
                    && !self.facts.contains_key(&chat_id)
                    && queued.insert(chat_id)
                {
                    queue.push_back(chat_id);
                }
            }
        }
        if queue.is_empty() {
            let commit = self.assemble(list, &order, total)?;
            self.phase = Phase::CommitReady(Box::new(commit));
        } else {
            self.phase = Phase::Resolving {
                order,
                total,
                queue,
            };
        }
        Ok(())
    }

    fn on_chat_detail(
        &mut self,
        expected: Option<i64>,
        value: &Value,
    ) -> Result<(), SnapshotError> {
        let ingested = match self.ingest_chat(value) {
            Ok(chat_id) => chat_id,
            Err(detail) => return Err(self.fail(SnapshotError::Malformed { detail })),
        };
        if expected != Some(ingested) {
            return Err(self.fail(SnapshotError::Malformed {
                detail: format!(
                    "getChat answered chat {ingested}, expected {}",
                    expected.map_or_else(|| "none".to_owned(), |id| id.to_string())
                ),
            }));
        }
        let Phase::Resolving { queue, .. } = &mut self.phase else {
            return Err(self.fail(SnapshotError::Malformed {
                detail: "chat detail answered outside the resolving phase".to_owned(),
            }));
        };
        queue.pop_front();
        if queue.is_empty() {
            let Phase::Resolving { order, total, .. } =
                std::mem::replace(&mut self.phase, Phase::NextList)
            else {
                // The let-else above proves the variant.
                return Err(self.fail(SnapshotError::Malformed {
                    detail: "resolving phase changed underneath the machine".to_owned(),
                }));
            };
            let list = self.current_list()?;
            let commit = self.assemble(list, &order, total)?;
            self.phase = Phase::CommitReady(Box::new(commit));
        }
        Ok(())
    }

    /// Ingest one TDLib chat object: canonical facts plus every list
    /// position it carries. Strict about the members that name the chat
    /// (`id`), lenient about the rest — an unknown chat type is recorded as
    /// such and excluded at assembly rather than failing the run.
    fn ingest_chat(&mut self, chat: &Value) -> Result<i64, String> {
        let Some(chat_id) = chat.get("id").and_then(Value::as_i64) else {
            return Err(format!("chat object without an integer id: {chat}"));
        };
        let kind = match chat.get("type") {
            Some(kind) => parse_chat_kind(kind),
            None => KindFact::Unsupported,
        };
        let title = chat
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let is_protected = chat
            .get("has_protected_content")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.facts.insert(
            chat_id,
            ChatFacts {
                kind,
                title,
                is_protected,
            },
        );
        if let Some(positions) = chat.get("positions").and_then(Value::as_array) {
            for position in positions {
                self.ingest_position(chat_id, position);
            }
        }
        Ok(chat_id)
    }

    /// Ingest one `chatPosition` object for `chat_id`. Positions for list
    /// shapes this build does not know, and malformed positions, are
    /// skipped: the strict gap check at assembly is the backstop.
    fn ingest_position(&mut self, chat_id: i64, position: &Value) {
        let Some(list) = position.get("list").and_then(parse_list) else {
            return;
        };
        let Some(order) = position.get("order").and_then(parse_order) else {
            return;
        };
        let fact = if order == 0 {
            PositionFact::Removed
        } else {
            PositionFact::Present {
                order,
                pinned: position
                    .get("is_pinned")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        };
        self.positions
            .entry(list)
            .or_default()
            .insert(chat_id, fact);
    }

    /// Assemble the commit for `list`: verify the order witness against the
    /// position map, then emit the map's members in exact server order.
    fn assemble(
        &mut self,
        list: ChatListKind,
        order: &[i64],
        total: Option<u32>,
    ) -> Result<ListCommit, SnapshotError> {
        let positions = self.positions.get(&list).cloned().unwrap_or_default();
        let mut excluded_removed: u32 = 0;
        for &chat_id in order {
            match positions.get(&chat_id) {
                Some(PositionFact::Present { .. }) => {}
                Some(PositionFact::Removed) => {
                    excluded_removed = excluded_removed.saturating_add(1);
                }
                None => return Err(self.fail(SnapshotError::MissingPosition { chat_id })),
            }
        }
        let mut members: Vec<ListEntrySnapshot> = positions
            .iter()
            .filter_map(|(&chat_id, fact)| match fact {
                PositionFact::Present { order, pinned } => Some(ListEntrySnapshot {
                    chat_id,
                    sort_order: *order,
                    pinned: *pinned,
                }),
                PositionFact::Removed => None,
            })
            .collect();
        // Exact server order: pinned first, then Telegram's opaque order
        // descending, ties by chat id descending — TDLib's own sort pair,
        // and byte-for-byte the state layer's `chat_list` read order.
        members.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then(b.sort_order.cmp(&a.sort_order))
                .then(b.chat_id.cmp(&a.chat_id))
        });
        let mut chats = Vec::with_capacity(members.len());
        let mut entries = Vec::with_capacity(members.len());
        let mut excluded_secret: u32 = 0;
        let mut excluded_unsupported: u32 = 0;
        for entry in members {
            let Some(facts) = self.facts.get(&entry.chat_id) else {
                // Every member either had facts before the witness or went
                // through lazy resolution; a hole here is machine breakage.
                return Err(self.fail(SnapshotError::Malformed {
                    detail: format!("member chat {} has no metadata at assembly", entry.chat_id),
                }));
            };
            let (kind, username) = match &facts.kind {
                KindFact::Private { user_id } => (
                    SnapshotChatKind::Private,
                    self.user_names.get(user_id).cloned().flatten(),
                ),
                KindFact::Group => (SnapshotChatKind::Group, None),
                KindFact::Supergroup { supergroup_id } => (
                    SnapshotChatKind::Supergroup,
                    self.supergroup_names.get(supergroup_id).cloned().flatten(),
                ),
                KindFact::Channel { supergroup_id } => (
                    SnapshotChatKind::Channel,
                    self.supergroup_names.get(supergroup_id).cloned().flatten(),
                ),
                KindFact::Secret => {
                    excluded_secret = excluded_secret.saturating_add(1);
                    continue;
                }
                KindFact::Unsupported => {
                    excluded_unsupported = excluded_unsupported.saturating_add(1);
                    continue;
                }
            };
            chats.push(ChatSnapshot {
                chat_id: entry.chat_id,
                kind,
                title: facts.title.clone(),
                username,
                is_protected: facts.is_protected,
            });
            entries.push(entry);
        }
        let resume_token = self.encode_token_with(list);
        Ok(ListCommit {
            list,
            chats,
            entries,
            total_count: total,
            excluded_secret,
            excluded_unsupported,
            excluded_removed,
            resume_token,
        })
    }

    /// The durable token naming every committed list plus `list`, in plan
    /// order — deterministic bytes for a deterministic snapshot.
    fn encode_token_with(&self, list: ChatListKind) -> Vec<u8> {
        let done: Vec<Value> = self
            .lists
            .iter()
            .copied()
            .filter(|candidate| *candidate == list || self.done.contains(candidate))
            .map(|candidate| Value::String(list_token(candidate)))
            .collect();
        json!({"v": TOKEN_VERSION, "done": done})
            .to_string()
            .into_bytes()
    }
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

/// The stable text token of a list kind inside a resume token.
fn list_token(list: ChatListKind) -> String {
    match list {
        ChatListKind::Main => "main".to_owned(),
        ChatListKind::Archive => "archive".to_owned(),
        ChatListKind::Folder(folder) => format!("folder:{}", folder.0),
    }
}

/// Parse one list token back; unknown text is an explicit rejection.
fn parse_list_token(text: &str) -> Result<ChatListKind, SnapshotError> {
    match text {
        "main" => Ok(ChatListKind::Main),
        "archive" => Ok(ChatListKind::Archive),
        _ => match text.strip_prefix("folder:") {
            Some(id) => id
                .parse::<i32>()
                .map(|id| ChatListKind::Folder(FolderId(id)))
                .map_err(|_| SnapshotError::ResumeToken {
                    detail: format!("unparseable folder id in '{text}'"),
                }),
            None => Err(SnapshotError::ResumeToken {
                detail: format!("unknown list token '{text}'"),
            }),
        },
    }
}

/// Parse a resume token's bytes into the completed-list set.
fn parse_token(token: &[u8]) -> Result<HashSet<ChatListKind>, SnapshotError> {
    let value: Value =
        serde_json::from_slice(token).map_err(|error| SnapshotError::ResumeToken {
            detail: format!("token is not JSON: {error}"),
        })?;
    let version = value.get("v").and_then(Value::as_u64);
    if version != Some(TOKEN_VERSION) {
        return Err(SnapshotError::ResumeToken {
            detail: format!(
                "unsupported token version {}",
                version.map_or_else(|| "none".to_owned(), |version| version.to_string())
            ),
        });
    }
    let Some(done) = value.get("done").and_then(Value::as_array) else {
        return Err(SnapshotError::ResumeToken {
            detail: "token without a done array".to_owned(),
        });
    };
    let mut lists = HashSet::with_capacity(done.len());
    for entry in done {
        let Some(text) = entry.as_str() else {
            return Err(SnapshotError::ResumeToken {
                detail: format!("non-string done entry {entry}"),
            });
        };
        if !lists.insert(parse_list_token(text)?) {
            return Err(SnapshotError::ResumeToken {
                detail: format!("list token '{text}' repeats"),
            });
        }
    }
    Ok(lists)
}

/// Retryable-failure classification (SYNC-044): `Some(stated delay)` for
/// flood control (code 429 / `FLOOD_WAIT`), `Some(None)` for TDLib's
/// transport failures (code 500). Everything else is fatal for the run.
fn retryable_after(error: &TdError) -> Option<Option<u64>> {
    match error {
        TdError::Td { code, message } => {
            if *code == 429
                || message.starts_with("Too Many Requests")
                || message.starts_with("FLOOD_WAIT")
            {
                Some(trailing_integer(message))
            } else if *code == 500 {
                Some(None)
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(lists: &[ChatListKind]) -> SnapshotPlan {
        SnapshotPlan::new(lists.to_vec())
    }

    #[test]
    fn plan_rejects_duplicate_lists() {
        let err = SnapshotMachine::new(plan(&[ChatListKind::Main, ChatListKind::Main]))
            .expect_err("duplicate lists must be rejected");
        assert!(matches!(err, SnapshotError::Plan { .. }), "{err}");
    }

    #[test]
    fn page_size_zero_is_raised_to_one() {
        let mut machine = SnapshotMachine::new(SnapshotPlan {
            lists: vec![ChatListKind::Main],
            page_size: 0,
        })
        .expect("plan is valid");
        let step = machine.next_step().expect("first step");
        let SnapshotStep::Submit(request) = step else {
            panic!("expected a submit, got {step:?}");
        };
        assert_eq!(request.get("limit").and_then(Value::as_u64), Some(1));
    }

    #[test]
    fn resume_token_round_trips_and_rejects_foreign_shapes() {
        let folder = ChatListKind::Folder(FolderId(7));
        let mut machine =
            SnapshotMachine::new(plan(&[ChatListKind::Main, ChatListKind::Archive, folder]))
                .expect("plan is valid");
        machine.done.insert(ChatListKind::Main);
        let token = machine.encode_token_with(folder);
        let parsed = parse_token(&token).expect("token round-trips");
        assert_eq!(
            parsed,
            HashSet::from([ChatListKind::Main, folder]),
            "token names exactly the committed lists"
        );

        for (bytes, name) in [
            (b"not json".to_vec(), "non-JSON"),
            (br#"{"v":2,"done":[]}"#.to_vec(), "future version"),
            (br#"{"done":[]}"#.to_vec(), "missing version"),
            (br#"{"v":1}"#.to_vec(), "missing done"),
            (br#"{"v":1,"done":[3]}"#.to_vec(), "non-string entry"),
            (br#"{"v":1,"done":["weekly"]}"#.to_vec(), "unknown token"),
            (br#"{"v":1,"done":["folder:x"]}"#.to_vec(), "bad folder id"),
            (
                br#"{"v":1,"done":["main","main"]}"#.to_vec(),
                "repeated list",
            ),
        ] {
            let err = parse_token(&bytes).expect_err(name);
            assert!(matches!(err, SnapshotError::ResumeToken { .. }), "{name}");
        }
    }

    #[test]
    fn resume_drops_lists_outside_the_plan() {
        let token = br#"{"v":1,"done":["main","folder:9"]}"#;
        let machine =
            SnapshotMachine::resume(plan(&[ChatListKind::Main, ChatListKind::Archive]), token)
                .expect("token is valid");
        assert_eq!(machine.done, HashSet::from([ChatListKind::Main]));
    }

    #[test]
    fn retryable_classification_matches_flood_and_transport_only() {
        let flood = TdError::Td {
            code: 429,
            message: "Too Many Requests: retry after 17".to_owned(),
        };
        assert_eq!(retryable_after(&flood), Some(Some(17)));
        let flood_bare = TdError::Td {
            code: 420,
            message: "FLOOD_WAIT_120".to_owned(),
        };
        assert_eq!(retryable_after(&flood_bare), Some(Some(120)));
        let transport = TdError::Td {
            code: 500,
            message: "Failed to connect".to_owned(),
        };
        assert_eq!(retryable_after(&transport), Some(None));
        let fatal = TdError::Td {
            code: 400,
            message: "CHAT_ID_INVALID".to_owned(),
        };
        assert_eq!(retryable_after(&fatal), None);
        assert_eq!(retryable_after(&TdError::ClientClosed), None);
        assert_eq!(retryable_after(&TdError::Shutdown), None);
    }

    #[test]
    fn response_without_an_outstanding_request_is_a_typed_failure() {
        let mut machine = SnapshotMachine::new(plan(&[ChatListKind::Main])).expect("valid plan");
        let err = machine
            .on_response(Ok(json!({"@type": "ok"})))
            .expect_err("nothing outstanding");
        assert_eq!(err, SnapshotError::NoRequestOutstanding);
        // Terminal: the machine keeps reporting the failure.
        let repeat = machine.next_step().expect_err("machine is poisoned");
        assert_eq!(repeat, SnapshotError::NoRequestOutstanding);
    }
}
