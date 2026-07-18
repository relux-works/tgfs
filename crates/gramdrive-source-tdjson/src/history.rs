//! The resumable per-chat history crawl machine: TDLib's `getChatHistory`
//! paging becomes deterministic, prioritized, per-batch commits with
//! durable per-chat cursors (TASK-260715-26dnp6).
//!
//! # What the crawl is
//!
//! After the initial chat-list snapshot ([`crate::snapshot`]) established
//! *what exists*, this machine walks each chat's normal message history —
//! metadata-first, never a media download (SYNC-020; attachments surface
//! as descriptors inside [`MessageRecord`], hydration is the engine's
//! separate concern) — and turns every fetched page into normalized
//! records via [`normalize_message`] (TASK-260715-1ynmct). The only
//! request it issues is `getChatHistory` (TGC-21: normal API methods, no
//! takeout). Topics, albums, replies, and protection facts are per-record
//! facts the normalizer already carries; a topic-structured supergroup is
//! crawled as one chat, its topics arriving as [`MessageRecord::topic`]
//! refs.
//!
//! # Shape: sans-IO, like [`SnapshotMachine`](crate::snapshot::SnapshotMachine)
//!
//! [`CrawlMachine`] performs no I/O, holds no client handle, and reads no
//! clock. The caller owns the wiring, one obligation at a time:
//!
//! 1. [`CrawlMachine::next_step`] names the current obligation — submit a
//!    request ([`CrawlStep::Submit`]), wait out a [`CrawlStep::Backoff`],
//!    persist a [`CrawlStep::Commit`], record a
//!    [`CrawlStep::Unavailable`], or stop at [`CrawlStep::Done`]. Calling
//!    it again without acting returns the same obligation.
//! 2. The outcome of a submitted request is fed to
//!    [`CrawlMachine::on_response`].
//!
//! The machine consumes no update stream: live messages, edits and
//! deletions are the ordered update loop's feed (TASK-260715-10p5zp).
//! This machine's contract with that loop is the *newest boundary*: every
//! run first reconnects a chat's committed window to the present (the
//! catch-up phase below), so the update loop only ever has to extend
//! `newest` with what it sees live.
//!
//! # The protocol per chat
//!
//! `getChatHistory(chat_id, from_message_id, offset: 0, limit,
//! only_local: false)` answers a `messages` object whose entries descend
//! strictly by message id, all ids strictly below `from_message_id`
//! (`0` meaning "from the newest message"). An empty answer means the end
//! of history in the paged direction. On that contract the machine runs
//! three phases per chat:
//!
//! - **Anchor** (no committed window yet): one page from `0`. An empty
//!   chat commits `history_complete` with no window; otherwise the page's
//!   `[min, max]` becomes the initial window and paging turns backward.
//! - **Catch-up** (a committed window exists): pages from `0` descend
//!   until one connects to the committed `newest` (an id at or below it),
//!   then `newest` advances to the top of the first catch-up page. Pages
//!   *before* the connection commit their records under the *unchanged*
//!   window: the durable window only ever describes contiguous coverage,
//!   so a crash mid-catch-up re-fetches those pages and the state layer's
//!   idempotent replay (SYNC-021) skips them. Re-observed records below
//!   the boundary are deliberate — a recent edit surfaces that way.
//! - **Backward** (the backfill): pages from the window's `oldest`,
//!   moving `oldest` down with each commit until an empty answer marks
//!   [`HistoryCommit::history_complete`].
//!
//! Every answered page yields exactly one commit; the caller persists the
//! records and the commit's window/completion facts (the state layer's
//! `chat_sync_state` row) in one transaction (SYNC-022).
//!
//! # Resume: the cursor is the `chat_sync_state` row
//!
//! Unlike the snapshot, the crawl needs no opaque resume token: its
//! durable cursor is structured per-chat state — the `[oldest, newest]`
//! window plus the completion flag — and the state layer already owns
//! that shape (SYNC-021). A resuming caller reads those rows back and
//! rebuilds the [`CrawlPlan`] with each [`ChatCrawl::window`] /
//! [`ChatCrawl::history_complete`] filled in; account/namespace scoping
//! (SYNC-004) is the row key's job. Restart can produce neither
//! duplicates nor gaps: backward pages fetch strictly below the committed
//! `oldest`, catch-up commits never advance the window past what they
//! connected, and replayed batches are idempotent by message identity.
//!
//! # Bounded scheduling and priority
//!
//! One request is outstanding at a time (SEC-031: bounded concurrency),
//! and the scheduler re-picks the chat after *every* page: highest
//! [`CrawlPriority`] first, then the fewest pages served this run, then
//! plan order. Equal-priority chats therefore round-robin page by page —
//! a huge history cannot starve the rest of the account — while a
//! [`CrawlPriority::Visible`] chat (on screen, or explicitly requested)
//! takes every page until it is done. [`CrawlMachine::set_priority`]
//! takes effect at the next pick; the in-flight page is never abandoned.
//! Per-chat progress is observable at any time via
//! [`CrawlMachine::progress`].
//!
//! # Flood wait, pacing, and per-chat failure
//!
//! A request rejected with TDLib code 429 (`Too Many Requests` /
//! `FLOOD_WAIT`) or 500 (transport loss) arms one [`CrawlStep::Backoff`]
//! carrying Telegram's stated delay when the message names one, then the
//! identical request is re-issued (SYNC-044, TGC-22: the stated wait is
//! honored exactly, never a tight retry loop). The machine never sleeps —
//! the caller owns time, and the local-backfill pacing policy (request
//! spacing between pages, retry-attempt caps via
//! [`CrawlBackoff::attempt`]) composes at that seam.
//!
//! Every *other* TDLib rejection is a fact about one chat, not the run: a
//! left channel, a Telegram-side restriction, a chat this account can no
//! longer read. The machine emits [`CrawlStep::Unavailable`] naming the
//! chat and the typed reason — unavailable history is explicit, never a
//! silent skip — and crawling continues with the remaining chats. Only
//! runtime-level failures (client closed, shutdown, protocol breakage)
//! are fatal for the machine ([`CrawlError::Request`]); the durable
//! per-chat cursors are the recovery path.
//!
//! A page that violates the paging contract — a duplicate or ascending
//! id, an id at or above `from_message_id`, a message of another chat —
//! also fails that one chat explicitly ([`UnavailableReason::PageContract`],
//! the SYNC-003 rule at message granularity): advancing a cursor over a
//! lying page would corrupt the window's meaning. A malformed message
//! *object* inside an otherwise sound page degrades instead: it is
//! counted in [`HistoryCommit::skipped_malformed`] and its id still moves
//! the cursor, so one broken object can neither wedge the crawl nor
//! silently vanish.

use serde_json::{Value, json};

use crate::error::{TdError, retryable_after};
use crate::message::{MessageRecord, normalize_message};

/// TDLib's hard `getChatHistory` page-size ceiling.
const MAX_PAGE_SIZE: u32 = 100;

/// The contiguous `[oldest, newest]` span of message ids a chat's crawl
/// has normalized — the durable per-chat cursor (SYNC-021). The state
/// layer's `chat_sync_state` row is the intended carrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrawlWindow {
    /// Oldest message id inside the window.
    pub oldest_message_id: i64,
    /// Newest message id inside the window.
    pub newest_message_id: i64,
}

/// Scheduling weight of one chat, lowest to highest. `Ord` follows the
/// declaration order: [`CrawlPriority::Visible`] outranks
/// [`CrawlPriority::Requested`] outranks [`CrawlPriority::Background`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CrawlPriority {
    /// Ordinary backfill; the default.
    Background,
    /// The user asked for this chat (an explicit fetch, a pin).
    Requested,
    /// The chat is on screen right now.
    Visible,
}

/// One chat in the crawl plan: identity, the durable cursor read back
/// from the state layer (both `None`/`false` for a chat never crawled),
/// and the starting priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatCrawl {
    /// Telegram chat id (int53).
    pub chat_id: i64,
    /// The committed window, when one exists (`chat_sync_state`).
    pub window: Option<CrawlWindow>,
    /// Whether backfill already reached the beginning of history.
    pub history_complete: bool,
    /// Starting scheduling weight.
    pub priority: CrawlPriority,
}

impl ChatCrawl {
    /// A never-crawled chat at background priority.
    pub fn new(chat_id: i64) -> ChatCrawl {
        ChatCrawl {
            chat_id,
            window: None,
            history_complete: false,
            priority: CrawlPriority::Background,
        }
    }
}

/// Which chats one crawl run covers, and how it pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlPlan {
    /// The chats to crawl. Duplicate ids are rejected at machine
    /// construction ([`CrawlError::Plan`]).
    pub chats: Vec<ChatCrawl>,
    /// `getChatHistory` limit per page, clamped to `1..=100` (TDLib's
    /// ceiling); the default is [`CrawlPlan::DEFAULT_PAGE_SIZE`].
    pub page_size: u32,
}

impl CrawlPlan {
    /// Default page size: TDLib's maximum. The server is free to answer
    /// fewer; asking for the ceiling minimizes request count, which is
    /// what flood control meters.
    pub const DEFAULT_PAGE_SIZE: u32 = 100;

    /// A plan over `chats` with the default page size.
    pub fn new(chats: impl Into<Vec<ChatCrawl>>) -> CrawlPlan {
        CrawlPlan {
            chats: chats.into(),
            page_size: Self::DEFAULT_PAGE_SIZE,
        }
    }
}

/// One answered page, ready to persist atomically: the batch's normalized
/// records plus the window/completion facts that must commit with them
/// (SYNC-022).
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryCommit {
    /// The chat this commit covers.
    pub chat_id: i64,
    /// The page's records in wire order (newest first). Records at or
    /// below a previously committed `newest` are re-observations — the
    /// state layer's idempotent replay decides what each one *is*.
    pub records: Vec<MessageRecord>,
    /// The durable window after this commit; `None` only for a chat with
    /// no history at all.
    pub window: Option<CrawlWindow>,
    /// Whether backfill has reached the beginning of history.
    pub history_complete: bool,
    /// Message objects in the page that could not be normalized or
    /// carried no usable id — counted, never silently dropped.
    pub skipped_malformed: u32,
}

/// Why one chat's history cannot be crawled. Explicit per-chat state
/// (story AC: unavailable history is never a silent skip); availability
/// may return, so the next run simply plans the chat again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnavailableReason {
    /// TDLib rejected `getChatHistory` for this chat with an error that
    /// is neither flood control nor a transport failure — a left or
    /// inaccessible chat, a Telegram-side restriction.
    Rejected {
        /// The rejection as the runtime typed it.
        source: TdError,
    },
    /// A page violated the paging contract (duplicate or ascending ids,
    /// an id at or above `from_message_id`, a message of another chat) —
    /// advancing the cursor over it would corrupt the window (SYNC-003).
    PageContract {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
}

/// One chat whose history this run cannot crawl, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatUnavailable {
    /// The chat.
    pub chat_id: i64,
    /// Why it is unavailable.
    pub reason: UnavailableReason,
}

/// Flood-wait/transient-failure advice: wait, then call
/// [`CrawlMachine::next_step`] again — it re-issues the identical request
/// (SYNC-044).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrawlBackoff {
    /// The wait Telegram stated, when its message carried one. `None`
    /// for a transport failure (code 500) or an unstated flood wait; the
    /// caller's retry policy owns the delay then.
    pub retry_after_secs: Option<u64>,
    /// How many times this request has failed retryably, starting at 1.
    /// The machine never caps attempts; the caller's pacing policy can.
    pub attempt: u32,
}

/// The caller's current obligation, from [`CrawlMachine::next_step`].
#[derive(Debug, Clone)]
pub enum CrawlStep {
    /// Submit this request on the account's client and feed the outcome
    /// to [`CrawlMachine::on_response`]. Submit it exactly once per
    /// returned step; `next_step` repeats the obligation until the
    /// response is fed.
    Submit(Value),
    /// The last request hit flood control or a transport failure: wait,
    /// then call `next_step` again to re-issue it.
    Backoff(CrawlBackoff),
    /// A page is normalized: persist the records and the window facts
    /// atomically, then call `next_step` to continue.
    Commit(Box<HistoryCommit>),
    /// One chat's history is unavailable: record it, then call
    /// `next_step` — the remaining chats continue.
    Unavailable(Box<ChatUnavailable>),
    /// Every planned chat is complete or explicitly unavailable.
    Done,
}

/// Why the crawl failed as a whole. Every variant is terminal for the
/// machine — [`CrawlMachine::next_step`] keeps returning it — and
/// recovery is a fresh machine planned from the durable per-chat cursors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrawlError {
    /// The plan is invalid (duplicate chats, an inverted window).
    Plan {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// A request failed at the runtime level (client closed, shutdown,
    /// protocol breakage) — nothing chat-specific to record.
    Request {
        /// The failure as the runtime typed it.
        source: TdError,
    },
    /// A response did not have the shape the tdjson protocol promises,
    /// or the machine's own state broke an invariant.
    Malformed {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The caller fed a response while no request was outstanding.
    NoRequestOutstanding,
}

impl std::fmt::Display for CrawlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrawlError::Plan { detail } => write!(f, "invalid crawl plan: {detail}"),
            CrawlError::Request { source } => {
                write!(f, "history request failed at the runtime level: {source}")
            }
            CrawlError::Malformed { detail } => write!(f, "malformed history data: {detail}"),
            CrawlError::NoRequestOutstanding => {
                write!(
                    f,
                    "a response was fed while no history request was outstanding"
                )
            }
        }
    }
}

impl std::error::Error for CrawlError {}

/// Which side of a chat's window the crawl is working, as
/// [`CrawlMachine::progress`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrawlPhase {
    /// Not yet scheduled this run.
    Pending,
    /// Working the newest side: anchoring a fresh chat or reconnecting a
    /// committed window to the present.
    CatchingUp,
    /// Working the oldest side: the backfill proper.
    Backfilling,
    /// The window reaches the beginning of history and the present.
    Complete,
    /// Failed explicitly for this run ([`CrawlStep::Unavailable`]).
    Unavailable,
}

/// One chat's observable crawl progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatProgress {
    /// The chat.
    pub chat_id: i64,
    /// Current scheduling weight.
    pub priority: CrawlPriority,
    /// Which side of the window is being worked.
    pub phase: CrawlPhase,
    /// The window as of the last commit handed to the caller.
    pub window: Option<CrawlWindow>,
    /// Whether backfill has reached the beginning of history.
    pub history_complete: bool,
    /// Pages answered for this chat this run.
    pub pages_served: u64,
    /// Records handed to the caller for this chat this run.
    pub records_emitted: u64,
}

// ---------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------

/// Where one chat stands. `Anchor` and `CatchUp` both work the newest
/// side and report as [`CrawlPhase::CatchingUp`]; they differ in whether
/// a committed window bounds the descent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Pending,
    /// First page of a chat with no committed window, from `0`.
    Anchor,
    /// Descending from the present toward the committed `newest`. `top`
    /// is the first catch-up page's maximum id (the newest boundary once
    /// connected); `floor` is where the descent stands (`None`: not
    /// started, page from `0`).
    CatchUp {
        top: Option<i64>,
        floor: Option<i64>,
    },
    /// Descending below the committed `oldest`.
    Backward,
    Complete,
    Unavailable,
}

impl Phase {
    fn has_work(self) -> bool {
        !matches!(self, Phase::Complete | Phase::Unavailable)
    }
}

/// One chat's live state inside the machine.
#[derive(Debug)]
struct ChatState {
    chat_id: i64,
    priority: CrawlPriority,
    window: Option<CrawlWindow>,
    history_complete: bool,
    phase: Phase,
    pages_served: u64,
    records_emitted: u64,
}

/// One request in flight (or awaiting re-issue after a backoff).
#[derive(Debug)]
struct Outstanding {
    chat_index: usize,
    from_message_id: i64,
    payload: Value,
    attempt: u32,
    pending_backoff: Option<CrawlBackoff>,
}

/// A step produced by a response, waiting for the caller to take it.
#[derive(Debug)]
enum Emit {
    Commit(Box<HistoryCommit>),
    Unavailable(Box<ChatUnavailable>),
}

/// The deterministic resumable history crawl machine for one authorized
/// account's client. Sans-IO; the caller owns the wiring (module docs).
#[derive(Debug)]
pub struct CrawlMachine {
    chats: Vec<ChatState>,
    page_size: u32,
    outstanding: Option<Outstanding>,
    pending: Option<Emit>,
    failed: Option<Box<CrawlError>>,
}

impl CrawlMachine {
    /// A crawl over `plan`. Chats with a committed window resume from it;
    /// the rest anchor fresh — resumption is the plan carrying the
    /// durable cursors back in (module docs).
    pub fn new(plan: CrawlPlan) -> Result<CrawlMachine, CrawlError> {
        let mut seen = std::collections::HashSet::with_capacity(plan.chats.len());
        for chat in &plan.chats {
            if !seen.insert(chat.chat_id) {
                return Err(CrawlError::Plan {
                    detail: format!("chat {} appears twice", chat.chat_id),
                });
            }
            if let Some(window) = &chat.window
                && window.oldest_message_id > window.newest_message_id
            {
                return Err(CrawlError::Plan {
                    detail: format!(
                        "chat {} window is inverted ({} > {})",
                        chat.chat_id, window.oldest_message_id, window.newest_message_id
                    ),
                });
            }
        }
        Ok(CrawlMachine {
            chats: plan
                .chats
                .into_iter()
                .map(|chat| ChatState {
                    chat_id: chat.chat_id,
                    priority: chat.priority,
                    window: chat.window,
                    history_complete: chat.history_complete,
                    phase: Phase::Pending,
                    pages_served: 0,
                    records_emitted: 0,
                })
                .collect(),
            page_size: plan.page_size.clamp(1, MAX_PAGE_SIZE),
            outstanding: None,
            pending: None,
            failed: None,
        })
    }

    /// Change one chat's scheduling weight; `false` if the plan does not
    /// contain the chat. Takes effect at the next pick — the in-flight
    /// page is never abandoned.
    pub fn set_priority(&mut self, chat_id: i64, priority: CrawlPriority) -> bool {
        match self.chats.iter_mut().find(|chat| chat.chat_id == chat_id) {
            Some(chat) => {
                chat.priority = priority;
                true
            }
            None => false,
        }
    }

    /// Every chat's observable progress, in plan order.
    pub fn progress(&self) -> Vec<ChatProgress> {
        self.chats
            .iter()
            .map(|chat| ChatProgress {
                chat_id: chat.chat_id,
                priority: chat.priority,
                phase: match chat.phase {
                    Phase::Pending => CrawlPhase::Pending,
                    Phase::Anchor | Phase::CatchUp { .. } => CrawlPhase::CatchingUp,
                    Phase::Backward => CrawlPhase::Backfilling,
                    Phase::Complete => CrawlPhase::Complete,
                    Phase::Unavailable => CrawlPhase::Unavailable,
                },
                window: chat.window,
                history_complete: chat.history_complete,
                pages_served: chat.pages_served,
                records_emitted: chat.records_emitted,
            })
            .collect()
    }

    /// The caller's current obligation. Idempotent: without an
    /// intervening [`CrawlMachine::on_response`] or step pickup, the same
    /// obligation is returned again (a [`CrawlStep::Backoff`] is returned
    /// once per failure, then the re-issue).
    pub fn next_step(&mut self) -> Result<CrawlStep, CrawlError> {
        if let Some(error) = &self.failed {
            return Err((**error).clone());
        }
        if let Some(emit) = self.pending.take() {
            return Ok(match emit {
                Emit::Commit(commit) => CrawlStep::Commit(commit),
                Emit::Unavailable(unavailable) => CrawlStep::Unavailable(unavailable),
            });
        }
        if let Some(outstanding) = &mut self.outstanding {
            if let Some(backoff) = outstanding.pending_backoff.take() {
                return Ok(CrawlStep::Backoff(backoff));
            }
            return Ok(CrawlStep::Submit(outstanding.payload.clone()));
        }
        let Some(chat_index) = self.pick_next() else {
            return Ok(CrawlStep::Done);
        };
        let chat = &mut self.chats[chat_index];
        if chat.phase == Phase::Pending {
            chat.phase = match chat.window {
                None => Phase::Anchor,
                Some(_) => Phase::CatchUp {
                    top: None,
                    floor: None,
                },
            };
        }
        let from_message_id = match chat.phase {
            Phase::Anchor => 0,
            Phase::CatchUp { floor, .. } => floor.unwrap_or(0),
            Phase::Backward => match &chat.window {
                Some(window) => window.oldest_message_id,
                None => {
                    return Err(self.fail(CrawlError::Malformed {
                        detail: "backward phase without a window".to_owned(),
                    }));
                }
            },
            Phase::Pending | Phase::Complete | Phase::Unavailable => {
                return Err(self.fail(CrawlError::Malformed {
                    detail: "a workless phase was scheduled".to_owned(),
                }));
            }
        };
        let payload = json!({
            "@type": "getChatHistory",
            "chat_id": self.chats[chat_index].chat_id,
            "from_message_id": from_message_id,
            "offset": 0,
            "limit": self.page_size,
            "only_local": false,
        });
        self.outstanding = Some(Outstanding {
            chat_index,
            from_message_id,
            payload: payload.clone(),
            attempt: 0,
            pending_backoff: None,
        });
        Ok(CrawlStep::Submit(payload))
    }

    /// Feed the outcome of the request the last [`CrawlStep::Submit`]
    /// named. Feeding a response with nothing outstanding is
    /// [`CrawlError::NoRequestOutstanding`].
    pub fn on_response(&mut self, outcome: Result<Value, TdError>) -> Result<(), CrawlError> {
        if let Some(error) = &self.failed {
            return Err((**error).clone());
        }
        let Some(mut outstanding) = self.outstanding.take() else {
            return Err(self.fail(CrawlError::NoRequestOutstanding));
        };
        match outcome {
            Ok(value) => self.on_page(outstanding.chat_index, outstanding.from_message_id, &value),
            Err(error) => {
                if let Some(retry_after_secs) = retryable_after(&error) {
                    outstanding.attempt = outstanding.attempt.saturating_add(1);
                    outstanding.pending_backoff = Some(CrawlBackoff {
                        retry_after_secs,
                        attempt: outstanding.attempt,
                    });
                    self.outstanding = Some(outstanding);
                    return Ok(());
                }
                match error {
                    // A non-retryable TDLib rejection is a fact about the
                    // one chat (left, restricted, gone) — never the run.
                    error @ TdError::Td { .. } => {
                        self.mark_unavailable(
                            outstanding.chat_index,
                            UnavailableReason::Rejected { source: error },
                        );
                        Ok(())
                    }
                    other => Err(self.fail(CrawlError::Request { source: other })),
                }
            }
        }
    }

    // -- internals ----------------------------------------------------------

    fn fail(&mut self, error: CrawlError) -> CrawlError {
        self.failed = Some(Box::new(error.clone()));
        error
    }

    /// The next chat to page: highest priority, then fewest pages served
    /// this run, then plan order — deterministic, and round-robin among
    /// equals (module docs).
    fn pick_next(&self) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (index, chat) in self.chats.iter().enumerate() {
            if !chat.phase.has_work() {
                continue;
            }
            let better = match best {
                None => true,
                Some(current) => {
                    let leader = &self.chats[current];
                    match chat.priority.cmp(&leader.priority) {
                        std::cmp::Ordering::Greater => true,
                        std::cmp::Ordering::Less => false,
                        std::cmp::Ordering::Equal => chat.pages_served < leader.pages_served,
                    }
                }
            };
            if better {
                best = Some(index);
            }
        }
        best
    }

    fn mark_unavailable(&mut self, chat_index: usize, reason: UnavailableReason) {
        let chat = &mut self.chats[chat_index];
        chat.phase = Phase::Unavailable;
        self.pending = Some(Emit::Unavailable(Box::new(ChatUnavailable {
            chat_id: chat.chat_id,
            reason,
        })));
    }

    /// Fold one successful `getChatHistory` answer into the owning chat's
    /// phase and arm the resulting commit.
    fn on_page(&mut self, chat_index: usize, from: i64, value: &Value) -> Result<(), CrawlError> {
        let entries = match value.get("messages") {
            None | Some(Value::Null) => &[][..],
            Some(Value::Array(items)) => items.as_slice(),
            Some(other) => {
                return Err(self.fail(CrawlError::Malformed {
                    detail: format!("messages member is not an array: {other}"),
                }));
            }
        };
        let page = match parse_entries(self.chats[chat_index].chat_id, from, entries) {
            Ok(page) => page,
            Err(detail) => {
                self.mark_unavailable(chat_index, UnavailableReason::PageContract { detail });
                return Ok(());
            }
        };
        let chat = &mut self.chats[chat_index];
        chat.pages_served = chat.pages_served.saturating_add(1);
        chat.records_emitted = chat
            .records_emitted
            .saturating_add(page.records.len() as u64);
        let phase = chat.phase;
        let malformed = |detail: String| CrawlError::Malformed { detail };
        match phase {
            Phase::Anchor => match page.bounds {
                None => {
                    // A chat with no history at all: complete, windowless.
                    chat.history_complete = true;
                    chat.phase = Phase::Complete;
                }
                Some((oldest, newest)) => {
                    chat.window = Some(CrawlWindow {
                        oldest_message_id: oldest,
                        newest_message_id: newest,
                    });
                    // A non-empty anchor page proves history exists below
                    // it, so completeness resets — the plan may have carried
                    // `history_complete=true` in from a prior empty-chat
                    // commit (`window=None`), and it must be re-proven by an
                    // empty backward answer exactly as for a fresh chat.
                    // Leaving it stale would let a later catch-up conclude
                    // `Complete` and skip the backfill, orphaning the older
                    // ids (TASK-260715-26dnp6 review: anchor-gap).
                    chat.history_complete = false;
                    chat.phase = Phase::Backward;
                }
            },
            Phase::CatchUp { top, .. } => {
                let Some(window) = &mut chat.window else {
                    return Err(self.fail(malformed("catch-up phase without a window".to_owned())));
                };
                match page.bounds {
                    Some((page_oldest, page_newest)) if page_oldest > window.newest_message_id => {
                        // Not yet connected: records commit, the window
                        // holds (contiguity, module docs).
                        chat.phase = Phase::CatchUp {
                            top: Some(top.unwrap_or(page_newest)),
                            floor: Some(page_oldest),
                        };
                    }
                    bounds => {
                        // Connected: an id at or below the committed
                        // newest (or nothing newer exists at all).
                        let top = top
                            .or(bounds.map(|(_, page_newest)| page_newest))
                            .unwrap_or(window.newest_message_id);
                        window.newest_message_id = window.newest_message_id.max(top);
                        chat.phase = if chat.history_complete {
                            Phase::Complete
                        } else {
                            Phase::Backward
                        };
                    }
                }
            }
            Phase::Backward => {
                let Some(window) = &mut chat.window else {
                    return Err(self.fail(malformed("backward phase without a window".to_owned())));
                };
                match page.bounds {
                    None => {
                        chat.history_complete = true;
                        chat.phase = Phase::Complete;
                    }
                    Some((page_oldest, _)) => {
                        // parse_entries proved every id < from, and from
                        // was this window's oldest.
                        window.oldest_message_id = page_oldest;
                    }
                }
            }
            Phase::Pending | Phase::Complete | Phase::Unavailable => {
                return Err(self.fail(malformed("a page answered in a workless phase".to_owned())));
            }
        }
        let chat = &self.chats[chat_index];
        self.pending = Some(Emit::Commit(Box::new(HistoryCommit {
            chat_id: chat.chat_id,
            records: page.records,
            window: chat.window,
            history_complete: chat.history_complete,
            skipped_malformed: page.skipped_malformed,
        })));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Page parsing
// ---------------------------------------------------------------------------

/// One parsed page: records in wire order, the id bounds the cursor moves
/// by, and the count of objects that degraded.
#[derive(Debug)]
struct ParsedPage {
    records: Vec<MessageRecord>,
    /// `(oldest, newest)` over every parsed id, malformed objects
    /// included — the cursor must move past a broken object, not refetch
    /// it forever. `None` for an empty page.
    bounds: Option<(i64, i64)>,
    skipped_malformed: u32,
}

/// Validate one page against the paging contract and normalize its
/// records. A contract violation is an `Err` (the chat fails explicitly);
/// a malformed message object is counted and skipped.
fn parse_entries(chat_id: i64, from: i64, entries: &[Value]) -> Result<ParsedPage, String> {
    let mut records = Vec::with_capacity(entries.len());
    let mut skipped_malformed: u32 = 0;
    let mut previous: Option<i64> = None;
    let mut bounds: Option<(i64, i64)> = None;
    for entry in entries {
        let Some(id) = entry.get("id").and_then(Value::as_i64) else {
            skipped_malformed = skipped_malformed.saturating_add(1);
            continue;
        };
        if from > 0 && id >= from {
            return Err(format!(
                "chat {chat_id}: page from {from} answered id {id} at or above it"
            ));
        }
        if let Some(previous) = previous
            && id >= previous
        {
            return Err(format!(
                "chat {chat_id}: page ids not strictly descending ({previous} then {id})"
            ));
        }
        previous = Some(id);
        bounds = Some(match bounds {
            None => (id, id),
            Some((oldest, newest)) => (oldest.min(id), newest.max(id)),
        });
        match normalize_message(entry) {
            Ok(record) => {
                if record.chat_id != chat_id {
                    return Err(format!(
                        "chat {chat_id}: page carried message {id} of chat {}",
                        record.chat_id
                    ));
                }
                records.push(record);
            }
            Err(_) => skipped_malformed = skipped_malformed.saturating_add(1),
        }
    }
    if !entries.is_empty() && bounds.is_none() {
        return Err(format!(
            "chat {chat_id}: a non-empty page carried no usable message id"
        ));
    }
    Ok(ParsedPage {
        records,
        bounds,
        skipped_malformed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(chat_id: i64, id: i64) -> Value {
        json!({
            "@type": "message",
            "id": id,
            "chat_id": chat_id,
            "date": 1_700_000_000 + id,
            "sender_id": {"@type": "messageSenderUser", "user_id": 42},
            "can_be_saved": true,
            "content": {
                "@type": "messageText",
                "text": {"@type": "formattedText", "text": format!("m{id}"), "entities": []},
            },
        })
    }

    fn page(chat_id: i64, ids: &[i64]) -> Value {
        let messages: Vec<Value> = ids.iter().map(|id| message(chat_id, *id)).collect();
        json!({"@type": "messages", "total_count": ids.len(), "messages": messages})
    }

    fn submit(machine: &mut CrawlMachine) -> Value {
        match machine.next_step().expect("a step") {
            CrawlStep::Submit(request) => request,
            other => panic!("expected a submit, got {other:?}"),
        }
    }

    fn commit(machine: &mut CrawlMachine) -> HistoryCommit {
        match machine.next_step().expect("a step") {
            CrawlStep::Commit(commit) => *commit,
            other => panic!("expected a commit, got {other:?}"),
        }
    }

    #[test]
    fn plan_rejects_duplicates_and_inverted_windows() {
        let err = CrawlMachine::new(CrawlPlan::new([ChatCrawl::new(1), ChatCrawl::new(1)]))
            .expect_err("duplicate chats must be rejected");
        assert!(matches!(err, CrawlError::Plan { .. }), "{err}");
        let inverted = ChatCrawl {
            window: Some(CrawlWindow {
                oldest_message_id: 9,
                newest_message_id: 3,
            }),
            ..ChatCrawl::new(2)
        };
        let err = CrawlMachine::new(CrawlPlan::new([inverted]))
            .expect_err("an inverted window must be rejected");
        assert!(matches!(err, CrawlError::Plan { .. }), "{err}");
    }

    #[test]
    fn page_size_is_clamped_to_tdlib_bounds() {
        for (asked, granted) in [(0, 1), (50, 50), (100_000, 100)] {
            let mut machine = CrawlMachine::new(CrawlPlan {
                chats: vec![ChatCrawl::new(1)],
                page_size: asked,
            })
            .expect("plan is valid");
            let request = submit(&mut machine);
            assert_eq!(request["limit"].as_u64(), Some(granted), "asked {asked}");
        }
    }

    #[test]
    fn anchor_then_backward_records_boundaries_and_completion() {
        let mut machine =
            CrawlMachine::new(CrawlPlan::new([ChatCrawl::new(5)])).expect("plan is valid");
        let request = submit(&mut machine);
        assert_eq!(request["@type"].as_str(), Some("getChatHistory"));
        assert_eq!(request["from_message_id"].as_i64(), Some(0));
        assert_eq!(request["only_local"].as_bool(), Some(false));
        machine
            .on_response(Ok(page(5, &[30, 20])))
            .expect("anchor page folds");
        let first = commit(&mut machine);
        assert_eq!(
            first.window,
            Some(CrawlWindow {
                oldest_message_id: 20,
                newest_message_id: 30,
            })
        );
        assert!(!first.history_complete);
        assert_eq!(first.records.len(), 2);

        let request = submit(&mut machine);
        assert_eq!(request["from_message_id"].as_i64(), Some(20));
        machine
            .on_response(Ok(page(5, &[10])))
            .expect("backward page folds");
        let second = commit(&mut machine);
        assert_eq!(
            second.window,
            Some(CrawlWindow {
                oldest_message_id: 10,
                newest_message_id: 30,
            })
        );

        let request = submit(&mut machine);
        assert_eq!(request["from_message_id"].as_i64(), Some(10));
        machine
            .on_response(Ok(page(5, &[])))
            .expect("empty page folds");
        let last = commit(&mut machine);
        assert!(last.history_complete);
        assert!(last.records.is_empty());
        assert!(matches!(
            machine.next_step().expect("a step"),
            CrawlStep::Done
        ));
    }

    #[test]
    fn anchor_over_a_carried_complete_flag_resets_it() {
        // The plan may carry history_complete=true from a prior empty-chat
        // commit (window=None) — the machine's own durable output. A
        // non-empty anchor page proves history exists below it, so the flag
        // must reset; leaving it stale lets a later catch-up conclude
        // Complete and skip the backfill (TASK-260715-26dnp6 review:
        // anchor-gap).
        let resumed = ChatCrawl {
            window: None,
            history_complete: true,
            ..ChatCrawl::new(5)
        };
        let mut machine = CrawlMachine::new(CrawlPlan::new([resumed])).expect("plan is valid");
        submit(&mut machine);
        machine
            .on_response(Ok(page(5, &[30, 20])))
            .expect("anchor page folds");
        let first = commit(&mut machine);
        assert_eq!(
            first.window,
            Some(CrawlWindow {
                oldest_message_id: 20,
                newest_message_id: 30,
            })
        );
        assert!(
            !first.history_complete,
            "a fresh anchor window must re-prove completeness, not inherit it"
        );
        // The backfill actually runs rather than concluding Done.
        let request = submit(&mut machine);
        assert_eq!(request["from_message_id"].as_i64(), Some(20));
    }

    #[test]
    fn empty_chat_completes_without_a_window() {
        let mut machine =
            CrawlMachine::new(CrawlPlan::new([ChatCrawl::new(5)])).expect("plan is valid");
        submit(&mut machine);
        machine
            .on_response(Ok(json!({"@type": "messages", "total_count": 0})))
            .expect("an absent messages member is an empty page");
        let only = commit(&mut machine);
        assert_eq!(only.window, None);
        assert!(only.history_complete);
        assert!(matches!(
            machine.next_step().expect("a step"),
            CrawlStep::Done
        ));
    }

    #[test]
    fn catch_up_connects_across_pages_and_keeps_the_window_contiguous() {
        let resumed = ChatCrawl {
            window: Some(CrawlWindow {
                oldest_message_id: 10,
                newest_message_id: 20,
            }),
            history_complete: false,
            ..ChatCrawl::new(5)
        };
        let mut machine = CrawlMachine::new(CrawlPlan {
            chats: vec![resumed],
            page_size: 2,
        })
        .expect("plan is valid");

        // Catch-up page 1: strictly above the committed newest — the
        // committed window must not move yet.
        let request = submit(&mut machine);
        assert_eq!(request["from_message_id"].as_i64(), Some(0));
        machine
            .on_response(Ok(page(5, &[50, 40])))
            .expect("catch-up page folds");
        let unconnected = commit(&mut machine);
        assert_eq!(
            unconnected.window,
            Some(CrawlWindow {
                oldest_message_id: 10,
                newest_message_id: 20,
            }),
            "an unconnected catch-up page must not advance the window"
        );

        // Catch-up page 2 reaches the committed newest: connected, and
        // the newest boundary jumps to the top of the first page.
        let request = submit(&mut machine);
        assert_eq!(request["from_message_id"].as_i64(), Some(40));
        machine
            .on_response(Ok(page(5, &[30, 20])))
            .expect("connecting page folds");
        let connected = commit(&mut machine);
        assert_eq!(
            connected.window,
            Some(CrawlWindow {
                oldest_message_id: 10,
                newest_message_id: 50,
            })
        );

        // Catch-up done; the backfill resumes below the committed oldest.
        let request = submit(&mut machine);
        assert_eq!(request["from_message_id"].as_i64(), Some(10));
    }

    #[test]
    fn catch_up_on_a_complete_chat_ends_without_backfill() {
        let resumed = ChatCrawl {
            window: Some(CrawlWindow {
                oldest_message_id: 10,
                newest_message_id: 20,
            }),
            history_complete: true,
            ..ChatCrawl::new(5)
        };
        let mut machine = CrawlMachine::new(CrawlPlan::new([resumed])).expect("plan is valid");
        submit(&mut machine);
        machine
            .on_response(Ok(page(5, &[25, 20])))
            .expect("connecting page folds");
        let connected = commit(&mut machine);
        assert_eq!(
            connected.window,
            Some(CrawlWindow {
                oldest_message_id: 10,
                newest_message_id: 25,
            })
        );
        assert!(connected.history_complete);
        assert!(matches!(
            machine.next_step().expect("a step"),
            CrawlStep::Done
        ));
    }

    #[test]
    fn scheduling_round_robins_equals_and_priority_preempts() {
        let mut machine = CrawlMachine::new(CrawlPlan {
            chats: vec![ChatCrawl::new(1), ChatCrawl::new(2)],
            page_size: 1,
        })
        .expect("plan is valid");

        // Equal priority: page-by-page alternation, plan order first.
        let request = submit(&mut machine);
        assert_eq!(request["chat_id"].as_i64(), Some(1));
        machine.on_response(Ok(page(1, &[100]))).expect("folds");
        commit(&mut machine);
        let request = submit(&mut machine);
        assert_eq!(request["chat_id"].as_i64(), Some(2));
        machine.on_response(Ok(page(2, &[200]))).expect("folds");
        commit(&mut machine);

        // A visibility boost takes effect at the next pick and holds
        // until the boosted chat is done.
        assert!(machine.set_priority(2, CrawlPriority::Visible));
        assert!(!machine.set_priority(99, CrawlPriority::Visible));
        let request = submit(&mut machine);
        assert_eq!(request["chat_id"].as_i64(), Some(2));
        machine.on_response(Ok(page(2, &[199]))).expect("folds");
        commit(&mut machine);
        let request = submit(&mut machine);
        assert_eq!(request["chat_id"].as_i64(), Some(2));
    }

    #[test]
    fn flood_wait_arms_backoff_and_reissues_the_identical_request() {
        let mut machine =
            CrawlMachine::new(CrawlPlan::new([ChatCrawl::new(5)])).expect("plan is valid");
        let first = submit(&mut machine);
        machine
            .on_response(Err(TdError::Td {
                code: 429,
                message: "Too Many Requests: retry after 17".to_owned(),
            }))
            .expect("flood is retryable");
        match machine.next_step().expect("a step") {
            CrawlStep::Backoff(backoff) => {
                assert_eq!(backoff.retry_after_secs, Some(17));
                assert_eq!(backoff.attempt, 1);
            }
            other => panic!("expected a backoff, got {other:?}"),
        }
        let second = submit(&mut machine);
        assert_eq!(first, second, "the re-issued request must be identical");
    }

    #[test]
    fn a_rejected_chat_is_explicitly_unavailable_and_the_rest_continue() {
        let mut machine = CrawlMachine::new(CrawlPlan {
            chats: vec![ChatCrawl::new(1), ChatCrawl::new(2)],
            page_size: 1,
        })
        .expect("plan is valid");
        let request = submit(&mut machine);
        assert_eq!(request["chat_id"].as_i64(), Some(1));
        machine
            .on_response(Err(TdError::Td {
                code: 400,
                message: "CHANNEL_PRIVATE".to_owned(),
            }))
            .expect("a per-chat rejection is not fatal");
        match machine.next_step().expect("a step") {
            CrawlStep::Unavailable(unavailable) => {
                assert_eq!(unavailable.chat_id, 1);
                assert!(matches!(
                    unavailable.reason,
                    UnavailableReason::Rejected {
                        source: TdError::Td { code: 400, .. }
                    }
                ));
            }
            other => panic!("expected unavailability, got {other:?}"),
        }
        let request = submit(&mut machine);
        assert_eq!(request["chat_id"].as_i64(), Some(2), "the crawl continues");
    }

    #[test]
    fn page_contract_violations_fail_the_one_chat() {
        for (name, answer) in [
            ("ascending ids", page(5, &[10, 20])),
            ("duplicate ids", page(5, &[10, 10])),
            ("foreign chat", page(7, &[10])),
            (
                "no usable id",
                json!({"@type": "messages", "messages": [{"@type": "message"}]}),
            ),
        ] {
            let mut machine =
                CrawlMachine::new(CrawlPlan::new([ChatCrawl::new(5)])).expect("plan is valid");
            submit(&mut machine);
            machine.on_response(Ok(answer)).expect("folds");
            match machine.next_step().expect("a step") {
                CrawlStep::Unavailable(unavailable) => {
                    assert_eq!(unavailable.chat_id, 5, "{name}");
                    assert!(
                        matches!(unavailable.reason, UnavailableReason::PageContract { .. }),
                        "{name}"
                    );
                }
                other => panic!("{name}: expected unavailability, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_id_at_or_above_the_cursor_is_a_contract_violation() {
        let resumed = ChatCrawl {
            window: Some(CrawlWindow {
                oldest_message_id: 10,
                newest_message_id: 10,
            }),
            history_complete: false,
            ..ChatCrawl::new(5)
        };
        let mut machine = CrawlMachine::new(CrawlPlan::new([resumed])).expect("plan is valid");
        submit(&mut machine);
        machine
            .on_response(Ok(page(5, &[12, 10])))
            .expect("catch-up connects");
        commit(&mut machine);
        let request = submit(&mut machine);
        assert_eq!(request["from_message_id"].as_i64(), Some(10));
        machine
            .on_response(Ok(page(5, &[10])))
            .expect("the violation folds into unavailability");
        assert!(matches!(
            machine.next_step().expect("a step"),
            CrawlStep::Unavailable(_)
        ));
    }

    #[test]
    fn a_malformed_message_is_counted_and_the_cursor_still_advances() {
        let mut machine =
            CrawlMachine::new(CrawlPlan::new([ChatCrawl::new(5)])).expect("plan is valid");
        submit(&mut machine);
        // id 20 parses but has no content object — normalization fails.
        let broken = json!({"@type": "message", "id": 20, "chat_id": 5, "date": 1});
        machine
            .on_response(Ok(json!({
                "@type": "messages",
                "messages": [message(5, 30), broken, message(5, 10)],
            })))
            .expect("folds");
        let first = commit(&mut machine);
        assert_eq!(first.skipped_malformed, 1);
        assert_eq!(first.records.len(), 2);
        assert_eq!(
            first.window,
            Some(CrawlWindow {
                oldest_message_id: 10,
                newest_message_id: 30,
            }),
            "the broken object's id still bounds the window"
        );
        let request = submit(&mut machine);
        assert_eq!(
            request["from_message_id"].as_i64(),
            Some(10),
            "the cursor moves past the broken object"
        );
    }

    #[test]
    fn progress_is_observable_per_chat() {
        let mut machine = CrawlMachine::new(CrawlPlan {
            chats: vec![ChatCrawl::new(1), ChatCrawl::new(2)],
            page_size: 1,
        })
        .expect("plan is valid");
        submit(&mut machine);
        machine.on_response(Ok(page(1, &[100]))).expect("folds");
        commit(&mut machine);
        let progress = machine.progress();
        assert_eq!(progress.len(), 2);
        assert_eq!(progress[0].chat_id, 1);
        assert_eq!(progress[0].phase, CrawlPhase::Backfilling);
        assert_eq!(progress[0].pages_served, 1);
        assert_eq!(progress[0].records_emitted, 1);
        assert_eq!(progress[1].phase, CrawlPhase::Pending);
        assert_eq!(progress[1].pages_served, 0);
    }

    #[test]
    fn response_without_an_outstanding_request_is_a_typed_failure() {
        let mut machine =
            CrawlMachine::new(CrawlPlan::new([ChatCrawl::new(1)])).expect("plan is valid");
        let err = machine
            .on_response(Ok(json!({"@type": "messages", "messages": []})))
            .expect_err("nothing outstanding");
        assert_eq!(err, CrawlError::NoRequestOutstanding);
        let repeat = machine.next_step().expect_err("machine is poisoned");
        assert_eq!(repeat, CrawlError::NoRequestOutstanding);
    }

    #[test]
    fn a_runtime_level_failure_is_fatal_for_the_machine() {
        let mut machine =
            CrawlMachine::new(CrawlPlan::new([ChatCrawl::new(1)])).expect("plan is valid");
        submit(&mut machine);
        let err = machine
            .on_response(Err(TdError::ClientClosed))
            .expect_err("client closure is not chat-specific");
        assert!(matches!(
            err,
            CrawlError::Request {
                source: TdError::ClientClosed
            }
        ));
    }
}
