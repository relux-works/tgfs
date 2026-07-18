//! The ordered live message update loop: TDLib's message push updates —
//! new messages, edits, deletions — become normalized, transactional
//! commits with gap recovery before any durable cursor advances
//! (TASK-260715-10p5zp, SYNC-022/023).
//!
//! # Where it sits
//!
//! [`CrawlMachine`](crate::history::CrawlMachine) owns the past: it
//! backfills each chat's history and, at the start of every run,
//! reconnects the committed `[oldest, newest]` window to the present.
//! [`LiveMachine`] owns the present: from the moment the client's update
//! stream flows, it folds every message push into the same
//! [`normalize_message`] vocabulary the crawl commits in, so the state
//! layer sees one change stream regardless of which machine observed a
//! message first. The chat-metadata siblings
//! ([`UpdateMachine`](crate::updates::UpdateMachine),
//! [`FolderCatalogMachine`](crate::folders::FolderCatalogMachine)) run
//! beside it on the same stream; this machine consumes only message
//! updates.
//!
//! # The crawl/live boundary
//!
//! The durable cursor is the state layer's `chat_sync_state` window, and
//! its meaning is *contiguous coverage* (SYNC-021). The two machines
//! split it: the crawl moves `oldest` down (backfill) and reconnects
//! `newest` after downtime (catch-up); the live loop only ever *extends*
//! `newest`. Three rules keep that split safe under concurrency:
//!
//! - A [`LiveCommit`] states the boundary as
//!   [`advance_newest`](LiveCommit::advance_newest) — "raise the stored
//!   window's newest to at least this id". The caller merges it with the
//!   *stored* row inside the applying transaction, keeping the stored
//!   `oldest` and `history_complete` untouched, so an in-progress
//!   backfill commit is never clobbered and never lost.
//! - The machine never *establishes* a window. Anchoring is the crawl's
//!   job (with its completeness-reset discipline); a chat with no
//!   committed window gets boundary-free commits — records persist, the
//!   cursor stays absent — until a crawl anchors it.
//! - Records outside the window are always safe to persist: the state
//!   layer's replay is idempotent by message identity (SYNC-021), so only
//!   the cursor needs gating, never the observations.
//!
//! # Gaps and recovery (SYNC-023)
//!
//! A committed `newest` of `M` plus a live message `N > M` does not prove
//! the ids between them were observed — messages may have arrived while
//! this process was down. Advancing the cursor straight to `N` would
//! claim coverage of `(M, N)` it does not have, orphaning those messages
//! forever. So each planned chat carries a per-session boundary state:
//!
//! - **Unverified** — a committed window exists, contiguity with the live
//!   stream is unproven. The first live message above `newest` opens a
//!   **bridge**: `getChatHistory` pages descend from the present (the
//!   crawl's exact catch-up protocol, same page-contract validation)
//!   until one connects to `newest`. Pages before the connection commit
//!   their records under the *unchanged* cursor; the connecting page's
//!   commit carries the advance. The gap is recovered strictly before the
//!   cursor moves — a crash mid-bridge leaves the old cursor, and the
//!   next session simply re-bridges over idempotent replay.
//! - **Verified** — contiguity proven (the bridge connected, or nothing
//!   newer existed). Every subsequent live message above `newest`
//!   advances the cursor with its own commit.
//! - **Frozen** — the bridge failed: TDLib rejected the history request
//!   (left/inaccessible chat) or a page violated the paging contract.
//!   Reported once as [`LiveStep::Degraded`] with the typed
//!   [`UnavailableReason`]; records keep flowing boundary-free and the
//!   cursor never advances this session — the next crawl run owns
//!   recovery. Never a silent skip, never a lying cursor.
//!
//! An update naming a chat outside the plan is the chat-level gap: its
//! operations buffer in arrival order and the chat is reported once as
//! [`LiveStep::Unresolved`]. The caller resolves the chat through the
//! chat machinery (the canonical row must exist before message rows —
//! the state layer's foreign key), then calls
//! [`LiveMachine::track_chat`], which replays the buffer through the
//! normal paths.
//!
//! # Edits and deletions
//!
//! TDLib splits an edit across partial updates: `updateMessageContent`
//! carries new content without an edit time, `updateMessageEdited` the
//! edit time without content. Merging them would be a torn write, so both
//! are treated as one *edit signal*: the machine re-fetches the full
//! message with `getMessage` (targeted re-fetch, TGC-21), normalizes the
//! consistent snapshot, and commits it as an ordinary observation.
//! Signals for the same message coalesce into one fetch. The state
//! layer's projection decides what each observation *is* (first sight or
//! edit) and guards the pathological orders: a stale revision never
//! rewinds newer state, and a revision arriving after an observed
//! deletion never resurrects the message.
//!
//! `updateDeleteMessages` counts only when `is_permanent` and not
//! `from_cache` (cache eviction is not deletion). Deletions emit in
//! arrival order; deleting a never-observed message is skipped by the
//! state layer (POL-3: unobserved history is never implied).
//!
//! A message still carrying a `sending_state` is skipped outright — its
//! id is provisional; the final message arrives via
//! `updateMessageSendSucceeded` and ingests as an ordinary new message.
//!
//! `updateMessageInteractionInfo` (reactions/views tallies) is
//! deliberately not consumed: tallies change continuously, and folding
//! every tick into the append-only event log (POL-3) would grow it
//! pathologically. Reactions refresh whenever the message is re-observed
//! (an edit refresh, a history re-fetch).
//!
//! # Shape, pacing, and failure
//!
//! Sans-IO like its siblings: [`LiveMachine::on_update`] is push-fed from
//! the update stream, [`LiveMachine::next_step`] names the caller's
//! current obligation, and re-fetch outcomes return through
//! [`LiveMachine::on_response`]. One request is outstanding at a time
//! (SEC-031); flood control and transport failures arm a [`LiveBackoff`]
//! and the identical request is re-issued (SYNC-044, TGC-22). Commits
//! drain before new fetches start, so checkpoints land early and pending
//! memory stays bounded. Only runtime-level failures (client closed,
//! shutdown, protocol breakage) are fatal for the machine
//! ([`LiveError::Request`]); recovery is a fresh machine planned from the
//! durable cursors, exactly as for the crawl.
//!
//! The machine reads no clock: `observed_at_ms` is the caller's to stamp
//! at commit time (SYNC-073), and metadata-first discipline holds — a
//! commit carries descriptors, never media (SYNC-020).

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::error::{TdError, retryable_after};
use crate::history::UnavailableReason;
use crate::message::{MessageRecord, normalize_message};

/// TDLib's hard `getChatHistory` page-size ceiling.
const MAX_PAGE_SIZE: u32 = 100;

/// One chat in the live plan: identity plus the committed window's newest
/// bound read back from the state layer (`None` for a chat with no
/// committed window — its cursor is never advanced here; module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveChat {
    /// Telegram chat id (int53).
    pub chat_id: i64,
    /// `chat_sync_state`'s `newest_loaded_message_id`, when a window
    /// exists.
    pub newest_message_id: Option<i64>,
}

impl LiveChat {
    /// A tracked chat with no committed window.
    pub fn new(chat_id: i64) -> LiveChat {
        LiveChat {
            chat_id,
            newest_message_id: None,
        }
    }
}

/// Which chats the loop tracks, and how gap bridges page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivePlan {
    /// The tracked chats. Duplicate ids are rejected at machine
    /// construction ([`LiveError::Plan`]).
    pub chats: Vec<LiveChat>,
    /// `getChatHistory` limit per bridge page, clamped to `1..=100`.
    pub page_size: u32,
}

impl LivePlan {
    /// Default bridge page size: TDLib's maximum, minimizing request
    /// count — which is what flood control meters.
    pub const DEFAULT_PAGE_SIZE: u32 = 100;

    /// A plan over `chats` with the default page size.
    pub fn new(chats: impl Into<Vec<LiveChat>>) -> LivePlan {
        LivePlan {
            chats: chats.into(),
            page_size: Self::DEFAULT_PAGE_SIZE,
        }
    }
}

/// One observation in a commit, in application order.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveChange {
    /// A full message revision was observed (a new message, a bridge
    /// page record, or an edit refresh) — the state layer's projection
    /// decides what it *is*. Boxed: a record is two orders of magnitude
    /// larger than a deletion, and commits carry many of either.
    Observed(Box<MessageRecord>),
    /// The message's deletion was observed. Carries no content — a
    /// tombstone never implies history that was not observed (POL-3).
    Deleted {
        /// The deleted message.
        message_id: i64,
    },
}

/// One drained checkpoint for one chat, applied to the state layer in a
/// single transaction (SYNC-022): the ordered changes, and the cursor
/// advance they justify.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveCommit {
    /// The chat this commit covers.
    pub chat_id: i64,
    /// The observations, in application order.
    pub changes: Vec<LiveChange>,
    /// Raise the stored window's newest to at least this id, in the same
    /// transaction — keeping the stored `oldest` and `history_complete`
    /// (the caller merges against the stored row; module docs). `None`
    /// leaves the cursor untouched, and a chat with no stored window
    /// never sees `Some` (this machine never establishes a window).
    pub advance_newest: Option<i64>,
    /// Message objects that could not be normalized — counted, never
    /// silently dropped.
    pub skipped_malformed: u32,
    /// Edit refreshes TDLib rejected (the message is gone or
    /// inaccessible); its deletion, when real, arrives as its own update.
    pub refreshes_rejected: u32,
}

/// Flood-wait/transient-failure advice: wait, then call
/// [`LiveMachine::next_step`] again — it re-issues the identical request
/// (SYNC-044).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveBackoff {
    /// The wait Telegram stated, when its message carried one. `None`
    /// for a transport failure or an unstated flood wait; the caller's
    /// retry policy owns the delay then.
    pub retry_after_secs: Option<u64>,
    /// How many times this request has failed retryably, starting at 1.
    /// The machine never caps attempts; the caller's pacing policy can.
    pub attempt: u32,
}

/// One chat whose boundary cannot advance this session, and why — the
/// gap bridge failed (module docs). Records keep flowing boundary-free;
/// the next crawl run owns recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatDegraded {
    /// The chat.
    pub chat_id: i64,
    /// Why the bridge failed, in the crawl's shared vocabulary.
    pub reason: UnavailableReason,
}

/// The caller's current obligation, from [`LiveMachine::next_step`].
#[derive(Debug, Clone)]
pub enum LiveStep {
    /// Submit this request on the account's client and feed the outcome
    /// to [`LiveMachine::on_response`]. Submit it exactly once per
    /// returned step; `next_step` repeats the obligation until the
    /// response is fed.
    Submit(Value),
    /// The last request hit flood control or a transport failure: wait,
    /// then call `next_step` again to re-issue it.
    Backoff(LiveBackoff),
    /// A checkpoint is ready: persist the changes and the cursor advance
    /// atomically, then call `next_step` to continue.
    Commit(Box<LiveCommit>),
    /// Updates named a chat outside the plan (reported once per chat):
    /// resolve it through the chat machinery, then
    /// [`LiveMachine::track_chat`] it — its buffered operations replay.
    Unresolved {
        /// The unknown chat.
        chat_id: i64,
    },
    /// One chat's gap bridge failed (reported once): its cursor is
    /// frozen for this session, its records still commit.
    Degraded(Box<ChatDegraded>),
    /// Nothing to do until more updates or responses arrive.
    Idle,
}

/// Why the loop failed as a whole. Every variant is terminal for the
/// machine — [`LiveMachine::next_step`] keeps returning it — and recovery
/// is a fresh machine planned from the durable cursors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveError {
    /// The plan is invalid (duplicate chats).
    Plan {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// A re-fetch failed at the runtime level (client closed, shutdown,
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

impl std::fmt::Display for LiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveError::Plan { detail } => write!(f, "invalid live plan: {detail}"),
            LiveError::Request { source } => {
                write!(f, "live re-fetch failed at the runtime level: {source}")
            }
            LiveError::Malformed { detail } => write!(f, "malformed live data: {detail}"),
            LiveError::NoRequestOutstanding => {
                write!(
                    f,
                    "a response was fed while no live request was outstanding"
                )
            }
        }
    }
}

impl std::error::Error for LiveError {}

// ---------------------------------------------------------------------------
// The machine
// ---------------------------------------------------------------------------

/// Where one chat's boundary stands this session (module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Boundary {
    /// No committed window: records commit boundary-free, forever.
    Floating,
    /// A committed window's newest, contiguity with the stream unproven.
    Unverified { newest: i64 },
    /// A gap bridge is descending toward `newest`. `top` is the first
    /// page's maximum id; `floor` is where the descent stands (`None`:
    /// not started, page from `0`).
    Bridging {
        newest: i64,
        top: Option<i64>,
        floor: Option<i64>,
    },
    /// Contiguity proven; live messages above `newest` advance the
    /// cursor directly.
    Verified { newest: i64 },
    /// The bridge failed: the cursor never advances this session.
    Frozen,
}

/// One tracked chat's live state.
#[derive(Debug, Default)]
struct ChatLive {
    boundary: Option<Boundary>,
    /// Changes ready to commit, in application order.
    ready: Vec<LiveChange>,
    /// Cursor advance to publish with the next commit.
    advance: Option<i64>,
    /// Highest live-observed message id (bridge connect raises the
    /// cursor at least this far).
    live_top: i64,
    /// Message ids with a pending edit refresh, coalesced.
    refreshes: BTreeSet<i64>,
    skipped_malformed: u32,
    refreshes_rejected: u32,
    /// A bridge failure not yet reported to the caller.
    degraded: Option<UnavailableReason>,
}

impl ChatLive {
    fn boundary(&self) -> Boundary {
        // Tracked chats always carry a boundary; Option only spares a
        // Default impl. Treat absence as Floating defensively.
        self.boundary.unwrap_or(Boundary::Floating)
    }

    fn has_commit(&self) -> bool {
        !self.ready.is_empty()
            || self.advance.is_some()
            || self.skipped_malformed > 0
            || self.refreshes_rejected > 0
    }
}

/// One buffered operation of an untracked chat, replayed on
/// [`LiveMachine::track_chat`].
#[derive(Debug, Clone, PartialEq)]
enum UntrackedOp {
    Observed(Box<MessageRecord>),
    Deleted { message_id: i64 },
    EditSignal { message_id: i64 },
}

/// An untracked chat's buffer and its report-once flag.
#[derive(Debug, Default)]
struct Untracked {
    pending: Vec<UntrackedOp>,
    malformed: u32,
    reported: bool,
}

/// What the one outstanding request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FetchKind {
    /// A gap-bridge `getChatHistory` page.
    Bridge { chat_id: i64, from: i64 },
    /// An edit-refresh `getMessage`.
    Refresh { chat_id: i64, message_id: i64 },
}

/// One request in flight (or awaiting re-issue after a backoff).
#[derive(Debug)]
struct Outstanding {
    kind: FetchKind,
    payload: Value,
    attempt: u32,
    pending_backoff: Option<LiveBackoff>,
}

/// The deterministic sans-IO ordered live message update loop for one
/// authorized account's client (module docs).
#[derive(Debug)]
pub struct LiveMachine {
    chats: BTreeMap<i64, ChatLive>,
    untracked: BTreeMap<i64, Untracked>,
    page_size: u32,
    outstanding: Option<Outstanding>,
    failed: Option<Box<LiveError>>,
}

impl LiveMachine {
    /// A live loop over `plan`. Each chat's committed newest bound comes
    /// from the durable cursor (`chat_sync_state`); a chat without one is
    /// tracked boundary-free (module docs).
    pub fn new(plan: LivePlan) -> Result<LiveMachine, LiveError> {
        let mut chats: BTreeMap<i64, ChatLive> = BTreeMap::new();
        for chat in &plan.chats {
            let state = ChatLive {
                boundary: Some(match chat.newest_message_id {
                    None => Boundary::Floating,
                    Some(newest) => Boundary::Unverified { newest },
                }),
                ..ChatLive::default()
            };
            if chats.insert(chat.chat_id, state).is_some() {
                return Err(LiveError::Plan {
                    detail: format!("chat {} appears twice", chat.chat_id),
                });
            }
        }
        Ok(LiveMachine {
            chats,
            untracked: BTreeMap::new(),
            page_size: plan.page_size.clamp(1, MAX_PAGE_SIZE),
            outstanding: None,
            failed: None,
        })
    }

    /// Whether [`LiveMachine::next_step`] would return anything but
    /// [`LiveStep::Idle`] — a cheap check before driving the loop.
    pub fn has_pending(&self) -> bool {
        self.outstanding.is_some()
            || self.untracked.values().any(|chat| !chat.reported)
            || self.chats.values().any(|chat| {
                chat.has_commit()
                    || chat.degraded.is_some()
                    || !chat.refreshes.is_empty()
                    || matches!(chat.boundary(), Boundary::Bridging { .. })
            })
    }

    /// Track a chat mid-session — a chat the snapshot did not know, now
    /// resolved through the chat machinery (its canonical row exists).
    /// Buffered operations replay in arrival order. `newest_message_id`
    /// is the chat's durable cursor bound, as at plan time; `false` if
    /// the chat is already tracked (nothing changes then).
    pub fn track_chat(&mut self, chat_id: i64, newest_message_id: Option<i64>) -> bool {
        if self.chats.contains_key(&chat_id) {
            return false;
        }
        let buffered = self.untracked.remove(&chat_id).unwrap_or_default();
        let state = ChatLive {
            boundary: Some(match newest_message_id {
                None => Boundary::Floating,
                Some(newest) => Boundary::Unverified { newest },
            }),
            skipped_malformed: buffered.malformed,
            ..ChatLive::default()
        };
        self.chats.insert(chat_id, state);
        for op in buffered.pending {
            match op {
                UntrackedOp::Observed(record) => self.ingest_record(record),
                UntrackedOp::Deleted { message_id } => self.ingest_deletion(chat_id, message_id),
                UntrackedOp::EditSignal { message_id } => self.ingest_edit(chat_id, message_id),
            }
        }
        true
    }

    /// Feed one update from the client's stream (module docs).
    /// Unrecognized and structurally malformed updates are ignored — the
    /// safety nets are the crawl's re-fetch and the state layer's
    /// idempotent replay, not a strict parse here (the
    /// [`UpdateMachine`](crate::updates::UpdateMachine) precedent).
    pub fn on_update(&mut self, update: &Value) {
        match update.get("@type").and_then(Value::as_str) {
            Some("updateNewMessage") | Some("updateMessageSendSucceeded") => {
                if let Some(message) = update.get("message") {
                    self.ingest_message(message);
                }
            }
            Some("updateMessageContent") | Some("updateMessageEdited") => {
                if let (Some(chat_id), Some(message_id)) = (
                    update.get("chat_id").and_then(Value::as_i64),
                    update.get("message_id").and_then(Value::as_i64),
                ) {
                    self.ingest_edit(chat_id, message_id);
                }
            }
            Some("updateDeleteMessages") => {
                let is_permanent = update
                    .get("is_permanent")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let from_cache = update
                    .get("from_cache")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // Cache eviction is not deletion; only permanent,
                // non-cache removals are observations (module docs).
                if !is_permanent || from_cache {
                    return;
                }
                if let (Some(chat_id), Some(ids)) = (
                    update.get("chat_id").and_then(Value::as_i64),
                    update.get("message_ids").and_then(Value::as_array),
                ) {
                    for id in ids {
                        if let Some(message_id) = id.as_i64() {
                            self.ingest_deletion(chat_id, message_id);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// The caller's current obligation. Idempotent: without an
    /// intervening [`LiveMachine::on_update`], [`LiveMachine::track_chat`],
    /// [`LiveMachine::on_response`], or step pickup, the same obligation
    /// is returned again (a [`LiveStep::Backoff`], [`LiveStep::Unresolved`],
    /// or [`LiveStep::Degraded`] is returned once, then the loop moves on).
    pub fn next_step(&mut self) -> Result<LiveStep, LiveError> {
        if let Some(error) = &self.failed {
            return Err((**error).clone());
        }
        // Reports first: they are cheap and the caller may act on them
        // (resolve a chat) while commits and fetches proceed.
        if let Some((&chat_id, chat)) = self
            .chats
            .iter_mut()
            .find(|(_, chat)| chat.degraded.is_some())
            && let Some(reason) = chat.degraded.take()
        {
            return Ok(LiveStep::Degraded(Box::new(ChatDegraded {
                chat_id,
                reason,
            })));
        }
        if let Some((&chat_id, chat)) = self.untracked.iter_mut().find(|(_, chat)| !chat.reported) {
            chat.reported = true;
            return Ok(LiveStep::Unresolved { chat_id });
        }
        // Commits before fetches: checkpoints land early, pending memory
        // stays bounded.
        if let Some((&chat_id, chat)) = self.chats.iter_mut().find(|(_, chat)| chat.has_commit()) {
            let commit = LiveCommit {
                chat_id,
                changes: std::mem::take(&mut chat.ready),
                advance_newest: chat.advance.take(),
                skipped_malformed: std::mem::take(&mut chat.skipped_malformed),
                refreshes_rejected: std::mem::take(&mut chat.refreshes_rejected),
            };
            return Ok(LiveStep::Commit(Box::new(commit)));
        }
        if let Some(outstanding) = &mut self.outstanding {
            if let Some(backoff) = outstanding.pending_backoff.take() {
                return Ok(LiveStep::Backoff(backoff));
            }
            return Ok(LiveStep::Submit(outstanding.payload.clone()));
        }
        // Bridges before refreshes: they gate cursors, refreshes only
        // enrich records. Both picks are ascending by chat id and
        // ascending by message id — deterministic.
        let mut bridge: Option<(i64, i64)> = None;
        let mut refresh: Option<(i64, i64)> = None;
        for (&chat_id, chat) in &self.chats {
            if let Boundary::Bridging { floor, .. } = chat.boundary() {
                bridge = Some((chat_id, floor.unwrap_or(0)));
                break;
            }
            if refresh.is_none()
                && let Some(&message_id) = chat.refreshes.first()
            {
                refresh = Some((chat_id, message_id));
            }
        }
        if let Some((chat_id, from)) = bridge {
            let payload = json!({
                "@type": "getChatHistory",
                "chat_id": chat_id,
                "from_message_id": from,
                "offset": 0,
                "limit": self.page_size,
                "only_local": false,
            });
            self.outstanding = Some(Outstanding {
                kind: FetchKind::Bridge { chat_id, from },
                payload: payload.clone(),
                attempt: 0,
                pending_backoff: None,
            });
            return Ok(LiveStep::Submit(payload));
        }
        if let Some((chat_id, message_id)) = refresh {
            let payload = json!({
                "@type": "getMessage",
                "chat_id": chat_id,
                "message_id": message_id,
            });
            self.outstanding = Some(Outstanding {
                kind: FetchKind::Refresh {
                    chat_id,
                    message_id,
                },
                payload: payload.clone(),
                attempt: 0,
                pending_backoff: None,
            });
            return Ok(LiveStep::Submit(payload));
        }
        Ok(LiveStep::Idle)
    }

    /// Feed the outcome of the request the last [`LiveStep::Submit`]
    /// named. Feeding a response with nothing outstanding is
    /// [`LiveError::NoRequestOutstanding`].
    pub fn on_response(&mut self, outcome: Result<Value, TdError>) -> Result<(), LiveError> {
        if let Some(error) = &self.failed {
            return Err((**error).clone());
        }
        let Some(mut outstanding) = self.outstanding.take() else {
            return Err(self.fail(LiveError::NoRequestOutstanding));
        };
        match outcome {
            Ok(value) => match outstanding.kind {
                FetchKind::Bridge { chat_id, from } => self.on_bridge_page(chat_id, from, &value),
                FetchKind::Refresh {
                    chat_id,
                    message_id,
                } => {
                    self.on_refresh(chat_id, message_id, &value);
                    Ok(())
                }
            },
            Err(error) => {
                if let Some(retry_after_secs) = retryable_after(&error) {
                    outstanding.attempt = outstanding.attempt.saturating_add(1);
                    outstanding.pending_backoff = Some(LiveBackoff {
                        retry_after_secs,
                        attempt: outstanding.attempt,
                    });
                    self.outstanding = Some(outstanding);
                    return Ok(());
                }
                match (outstanding.kind, error) {
                    // A non-retryable TDLib rejection of a bridge is a
                    // fact about the one chat: its cursor freezes.
                    (FetchKind::Bridge { chat_id, .. }, error @ TdError::Td { .. }) => {
                        self.freeze(chat_id, UnavailableReason::Rejected { source: error });
                        Ok(())
                    }
                    // A rejected refresh means the message is gone or
                    // inaccessible; its deletion, when real, arrives as
                    // its own update. Counted, never silent.
                    (
                        FetchKind::Refresh {
                            chat_id,
                            message_id,
                        },
                        TdError::Td { .. },
                    ) => {
                        if let Some(chat) = self.chats.get_mut(&chat_id) {
                            chat.refreshes.remove(&message_id);
                            chat.refreshes_rejected = chat.refreshes_rejected.saturating_add(1);
                        }
                        Ok(())
                    }
                    (_, other) => Err(self.fail(LiveError::Request { source: other })),
                }
            }
        }
    }

    // -- update ingestion ----------------------------------------------------

    /// Fold one full TDLib `message` object from a push update.
    fn ingest_message(&mut self, message: &Value) {
        // A message still being sent has a provisional id; the final
        // message arrives via updateMessageSendSucceeded (module docs).
        if message
            .get("sending_state")
            .is_some_and(|state| !state.is_null())
        {
            return;
        }
        match normalize_message(message) {
            Ok(record) => self.ingest_record(Box::new(record)),
            Err(_) => {
                // Malformed: count it on the owning chat when the chat id
                // is readable at all; an object without one is untraceable
                // and ignored (the UpdateMachine stance on malformed
                // updates).
                if let Some(chat_id) = message.get("chat_id").and_then(Value::as_i64) {
                    match self.chats.get_mut(&chat_id) {
                        Some(chat) => {
                            chat.skipped_malformed = chat.skipped_malformed.saturating_add(1);
                        }
                        None => {
                            let chat = self.untracked.entry(chat_id).or_default();
                            chat.malformed = chat.malformed.saturating_add(1);
                        }
                    }
                }
            }
        }
    }

    /// Fold one normalized live record into its chat's boundary state.
    fn ingest_record(&mut self, record: Box<MessageRecord>) {
        let chat_id = record.chat_id;
        let message_id = record.message_id;
        let Some(chat) = self.chats.get_mut(&chat_id) else {
            self.buffer_untracked(chat_id, UntrackedOp::Observed(record));
            return;
        };
        chat.live_top = chat.live_top.max(message_id);
        chat.ready.push(LiveChange::Observed(record));
        match chat.boundary() {
            Boundary::Verified { newest } if message_id > newest => {
                chat.boundary = Some(Boundary::Verified { newest: message_id });
                chat.advance = Some(message_id);
            }
            Boundary::Unverified { newest } if message_id > newest => {
                // The gap: ids in (newest, message_id) may have landed
                // while offline. Bridge before the cursor moves
                // (SYNC-023).
                chat.boundary = Some(Boundary::Bridging {
                    newest,
                    top: None,
                    floor: None,
                });
            }
            Boundary::Floating
            | Boundary::Frozen
            | Boundary::Bridging { .. }
            | Boundary::Verified { .. }
            | Boundary::Unverified { .. } => {}
        }
    }

    /// Fold one edit signal (`updateMessageContent`/`updateMessageEdited`).
    fn ingest_edit(&mut self, chat_id: i64, message_id: i64) {
        match self.chats.get_mut(&chat_id) {
            Some(chat) => {
                chat.refreshes.insert(message_id);
            }
            None => self.buffer_untracked(chat_id, UntrackedOp::EditSignal { message_id }),
        }
    }

    /// Fold one observed deletion.
    fn ingest_deletion(&mut self, chat_id: i64, message_id: i64) {
        match self.chats.get_mut(&chat_id) {
            Some(chat) => {
                // A deletion supersedes a pending, not-yet-submitted
                // refresh: getMessage would only reject.
                chat.refreshes.remove(&message_id);
                chat.ready.push(LiveChange::Deleted { message_id });
            }
            None => self.buffer_untracked(chat_id, UntrackedOp::Deleted { message_id }),
        }
    }

    fn buffer_untracked(&mut self, chat_id: i64, op: UntrackedOp) {
        match self.untracked.entry(chat_id) {
            Entry::Occupied(mut entry) => entry.get_mut().pending.push(op),
            Entry::Vacant(entry) => {
                entry.insert(Untracked {
                    pending: vec![op],
                    malformed: 0,
                    reported: false,
                });
            }
        }
    }

    // -- re-fetch outcomes ---------------------------------------------------

    /// Fold one bridge page: the crawl's catch-up protocol (module docs).
    fn on_bridge_page(&mut self, chat_id: i64, from: i64, value: &Value) -> Result<(), LiveError> {
        let entries = match value.get("messages") {
            None | Some(Value::Null) => &[][..],
            Some(Value::Array(items)) => items.as_slice(),
            Some(other) => {
                return Err(self.fail(LiveError::Malformed {
                    detail: format!("messages member is not an array: {other}"),
                }));
            }
        };
        let page = match parse_bridge_page(chat_id, from, entries) {
            Ok(page) => page,
            Err(detail) => {
                self.freeze(chat_id, UnavailableReason::PageContract { detail });
                return Ok(());
            }
        };
        let Some(chat) = self.chats.get_mut(&chat_id) else {
            return Err(self.fail(LiveError::Malformed {
                detail: format!("a bridge page answered for untracked chat {chat_id}"),
            }));
        };
        let Boundary::Bridging { newest, top, .. } = chat.boundary() else {
            return Err(self.fail(LiveError::Malformed {
                detail: format!("a bridge page answered for chat {chat_id} not bridging"),
            }));
        };
        chat.skipped_malformed = chat
            .skipped_malformed
            .saturating_add(page.skipped_malformed);
        chat.ready.extend(
            page.records
                .into_iter()
                .map(|record| LiveChange::Observed(Box::new(record))),
        );
        match page.bounds {
            Some((page_oldest, page_newest)) if page_oldest > newest => {
                // Not yet connected: records commit, the cursor holds
                // (contiguity; module docs).
                chat.boundary = Some(Boundary::Bridging {
                    newest,
                    top: Some(top.unwrap_or(page_newest)),
                    floor: Some(page_oldest),
                });
            }
            bounds => {
                // Connected: an id at or below the committed newest, or
                // nothing newer exists at all. The cursor rises to the
                // top of everything the bridge and the live stream saw.
                let top = top
                    .or(bounds.map(|(_, page_newest)| page_newest))
                    .unwrap_or(newest);
                let advanced = newest.max(top).max(chat.live_top);
                chat.boundary = Some(Boundary::Verified { newest: advanced });
                if advanced > newest {
                    chat.advance = Some(advanced);
                }
            }
        }
        Ok(())
    }

    /// Fold one edit-refresh answer: the full message, normalized like
    /// any other observation.
    fn on_refresh(&mut self, chat_id: i64, message_id: i64, value: &Value) {
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.refreshes.remove(&message_id);
        }
        match normalize_message(value) {
            Ok(record) if record.chat_id == chat_id && record.message_id == message_id => {
                self.ingest_record(Box::new(record));
            }
            // An answer for another message, or an unnormalizable one, is
            // an unusable observation — counted, never applied under the
            // wrong identity.
            Ok(_) | Err(_) => {
                if let Some(chat) = self.chats.get_mut(&chat_id) {
                    chat.skipped_malformed = chat.skipped_malformed.saturating_add(1);
                }
            }
        }
    }

    // -- internals -----------------------------------------------------------

    fn fail(&mut self, error: LiveError) -> LiveError {
        self.failed = Some(Box::new(error.clone()));
        error
    }

    /// Freeze one chat's cursor after a failed bridge and arm the
    /// one-time report.
    fn freeze(&mut self, chat_id: i64, reason: UnavailableReason) {
        if let Some(chat) = self.chats.get_mut(&chat_id) {
            chat.boundary = Some(Boundary::Frozen);
            chat.advance = None;
            chat.degraded = Some(reason);
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge page parsing
// ---------------------------------------------------------------------------

/// One parsed bridge page: records in wire order, the id bounds the
/// descent moves by (malformed objects included — the descent must move
/// past a broken object, not refetch it forever), and the degraded count.
#[derive(Debug)]
struct BridgePage {
    records: Vec<MessageRecord>,
    bounds: Option<(i64, i64)>,
    skipped_malformed: u32,
}

/// Validate one bridge page against the paging contract and normalize
/// its records — the crawl's rules verbatim (SYNC-003 at message
/// granularity): a contract violation freezes the chat's cursor, a
/// malformed message object is counted and skipped.
fn parse_bridge_page(chat_id: i64, from: i64, entries: &[Value]) -> Result<BridgePage, String> {
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
                "chat {chat_id}: bridge page from {from} answered id {id} at or above it"
            ));
        }
        if let Some(previous) = previous
            && id >= previous
        {
            return Err(format!(
                "chat {chat_id}: bridge page ids not strictly descending ({previous} then {id})"
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
                        "chat {chat_id}: bridge page carried message {id} of chat {}",
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
            "chat {chat_id}: a non-empty bridge page carried no usable message id"
        ));
    }
    Ok(BridgePage {
        records,
        bounds,
        skipped_malformed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(chat_id: i64, id: i64, text: &str) -> Value {
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

    fn new_message(chat_id: i64, id: i64, text: &str) -> Value {
        json!({"@type": "updateNewMessage", "message": message(chat_id, id, text)})
    }

    fn page(chat_id: i64, ids: &[i64]) -> Value {
        let messages: Vec<Value> = ids.iter().map(|id| message(chat_id, *id, "m")).collect();
        json!({"@type": "messages", "total_count": ids.len(), "messages": messages})
    }

    fn delete(chat_id: i64, ids: &[i64]) -> Value {
        json!({
            "@type": "updateDeleteMessages",
            "chat_id": chat_id,
            "message_ids": ids,
            "is_permanent": true,
            "from_cache": false,
        })
    }

    fn tracked(chat_id: i64, newest: i64) -> LiveChat {
        LiveChat {
            chat_id,
            newest_message_id: Some(newest),
        }
    }

    fn machine(chats: impl Into<Vec<LiveChat>>) -> LiveMachine {
        LiveMachine::new(LivePlan::new(chats)).expect("plan is valid")
    }

    fn submit(machine: &mut LiveMachine) -> Value {
        match machine.next_step().expect("a step") {
            LiveStep::Submit(request) => request,
            other => panic!("expected a submit, got {other:?}"),
        }
    }

    fn commit(machine: &mut LiveMachine) -> LiveCommit {
        match machine.next_step().expect("a step") {
            LiveStep::Commit(commit) => *commit,
            other => panic!("expected a commit, got {other:?}"),
        }
    }

    fn idle(machine: &mut LiveMachine) {
        assert!(!machine.has_pending(), "machine still has pending work");
        assert!(matches!(
            machine.next_step().expect("a step"),
            LiveStep::Idle
        ));
    }

    fn observed_ids(commit: &LiveCommit) -> Vec<i64> {
        commit
            .changes
            .iter()
            .filter_map(|change| match change {
                LiveChange::Observed(record) => Some(record.message_id),
                LiveChange::Deleted { .. } => None,
            })
            .collect()
    }

    #[test]
    fn plan_rejects_duplicate_chats() {
        let err = LiveMachine::new(LivePlan::new([LiveChat::new(1), LiveChat::new(1)]))
            .expect_err("duplicate chats must be rejected");
        assert!(matches!(err, LiveError::Plan { .. }), "{err}");
    }

    #[test]
    fn verified_chat_extends_the_cursor_per_message() {
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&new_message(5, 11, "a"));
        // The live record commits boundary-free, then the bridge's one
        // page connects immediately (its oldest id reaches newest 10).
        let live = commit(&mut machine);
        assert_eq!(live.advance_newest, None);
        submit(&mut machine);
        machine
            .on_response(Ok(page(5, &[11, 10])))
            .expect("bridge folds");
        let first = commit(&mut machine);
        assert_eq!(observed_ids(&first), vec![11, 10]);
        assert_eq!(first.advance_newest, Some(11));
        idle(&mut machine);

        // Now verified: each newer live message advances alone.
        machine.on_update(&new_message(5, 12, "b"));
        let second = commit(&mut machine);
        assert_eq!(observed_ids(&second), vec![12]);
        assert_eq!(second.advance_newest, Some(12));
        idle(&mut machine);

        // An older re-push is a re-observation, never a cursor move.
        machine.on_update(&new_message(5, 11, "a"));
        let third = commit(&mut machine);
        assert_eq!(third.advance_newest, None);
        idle(&mut machine);
    }

    #[test]
    fn floating_chat_never_advances_a_cursor() {
        let mut machine = machine([LiveChat::new(5)]);
        machine.on_update(&new_message(5, 7, "hi"));
        let commit = commit(&mut machine);
        assert_eq!(observed_ids(&commit), vec![7]);
        assert_eq!(
            commit.advance_newest, None,
            "no committed window: the live loop must not establish one"
        );
        idle(&mut machine);
    }

    #[test]
    fn gap_bridges_before_the_cursor_advances() {
        // Committed newest 10; messages 11..=14 landed while offline;
        // message 15 arrives live. Bridge pages (page size 2) must cover
        // the gap before any advance is published.
        let mut machine = LiveMachine::new(LivePlan {
            chats: vec![tracked(5, 10)],
            page_size: 2,
        })
        .expect("plan is valid");
        machine.on_update(&new_message(5, 15, "live"));

        // The live record commits boundary-free first.
        let live = commit(&mut machine);
        assert_eq!(observed_ids(&live), vec![15]);
        assert_eq!(live.advance_newest, None, "the gap is not yet covered");

        // Bridge page 1: from 0, strictly above newest — no advance.
        let request = submit(&mut machine);
        assert_eq!(request["@type"].as_str(), Some("getChatHistory"));
        assert_eq!(request["from_message_id"].as_i64(), Some(0));
        machine
            .on_response(Ok(page(5, &[15, 14])))
            .expect("bridge folds");
        let first = commit(&mut machine);
        assert_eq!(observed_ids(&first), vec![15, 14]);
        assert_eq!(first.advance_newest, None);

        // Bridge page 2: descends below the floor.
        let request = submit(&mut machine);
        assert_eq!(request["from_message_id"].as_i64(), Some(14));
        machine
            .on_response(Ok(page(5, &[13, 12])))
            .expect("bridge folds");
        let second = commit(&mut machine);
        assert_eq!(second.advance_newest, None);

        // Bridge page 3 reaches the committed newest: connected, and the
        // advance rides the same commit as the connecting records.
        let request = submit(&mut machine);
        assert_eq!(request["from_message_id"].as_i64(), Some(12));
        machine
            .on_response(Ok(page(5, &[11, 10])))
            .expect("bridge folds");
        let third = commit(&mut machine);
        assert_eq!(observed_ids(&third), vec![11, 10]);
        assert_eq!(third.advance_newest, Some(15));
        idle(&mut machine);
    }

    #[test]
    fn an_empty_bridge_page_connects_and_covers_the_live_top() {
        // History vanished under the bridge (everything deleted): the
        // empty answer still verifies the boundary, and the cursor rises
        // to the live stream's top.
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&new_message(5, 20, "live"));
        let live = commit(&mut machine);
        assert_eq!(live.advance_newest, None);
        submit(&mut machine);
        machine
            .on_response(Ok(json!({"@type": "messages", "total_count": 0})))
            .expect("empty page folds");
        let connect = commit(&mut machine);
        assert!(connect.changes.is_empty());
        assert_eq!(connect.advance_newest, Some(20));
        idle(&mut machine);
    }

    #[test]
    fn messages_arriving_mid_bridge_ride_the_connect_advance() {
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&new_message(5, 12, "a"));
        let _ = commit(&mut machine);
        submit(&mut machine);
        // While the page is in flight, a newer message lands.
        machine.on_update(&new_message(5, 13, "b"));
        machine
            .on_response(Ok(page(5, &[12, 11, 10])))
            .expect("bridge folds");
        // One commit: the mid-flight record, the page records, and the
        // advance covering the live top.
        let connected = commit(&mut machine);
        assert_eq!(observed_ids(&connected), vec![13, 12, 11, 10]);
        assert_eq!(connected.advance_newest, Some(13));
        idle(&mut machine);
    }

    #[test]
    fn a_rejected_bridge_freezes_the_cursor_and_reports_once() {
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&new_message(5, 12, "a"));
        let _ = commit(&mut machine);
        submit(&mut machine);
        machine
            .on_response(Err(TdError::Td {
                code: 400,
                message: "CHANNEL_PRIVATE".to_owned(),
            }))
            .expect("a per-chat rejection is not fatal");
        match machine.next_step().expect("a step") {
            LiveStep::Degraded(degraded) => {
                assert_eq!(degraded.chat_id, 5);
                assert!(matches!(
                    degraded.reason,
                    UnavailableReason::Rejected {
                        source: TdError::Td { code: 400, .. }
                    }
                ));
            }
            other => panic!("expected degradation, got {other:?}"),
        }
        idle(&mut machine);

        // Records keep flowing boundary-free; the cursor never moves.
        machine.on_update(&new_message(5, 13, "b"));
        let after = commit(&mut machine);
        assert_eq!(observed_ids(&after), vec![13]);
        assert_eq!(after.advance_newest, None);
        idle(&mut machine);
    }

    #[test]
    fn a_contract_violating_bridge_page_freezes_the_cursor() {
        for (name, answer) in [
            ("ascending ids", page(5, &[10, 20])),
            ("duplicate ids", page(5, &[12, 12])),
            ("foreign chat", page(7, &[12])),
            (
                "no usable id",
                json!({"@type": "messages", "messages": [{"@type": "message"}]}),
            ),
        ] {
            let mut machine = machine([tracked(5, 10)]);
            machine.on_update(&new_message(5, 15, "live"));
            let _ = commit(&mut machine);
            submit(&mut machine);
            machine.on_response(Ok(answer)).expect("violation folds");
            match machine.next_step().expect("a step") {
                LiveStep::Degraded(degraded) => {
                    assert_eq!(degraded.chat_id, 5, "{name}");
                    assert!(
                        matches!(degraded.reason, UnavailableReason::PageContract { .. }),
                        "{name}"
                    );
                }
                other => panic!("{name}: expected degradation, got {other:?}"),
            }
            idle(&mut machine);
        }
    }

    #[test]
    fn an_id_at_or_above_the_bridge_floor_is_a_contract_violation() {
        let mut machine = LiveMachine::new(LivePlan {
            chats: vec![tracked(5, 10)],
            page_size: 2,
        })
        .expect("plan is valid");
        machine.on_update(&new_message(5, 15, "live"));
        let _ = commit(&mut machine);
        submit(&mut machine);
        machine
            .on_response(Ok(page(5, &[15, 14])))
            .expect("bridge folds");
        let _ = commit(&mut machine);
        let request = submit(&mut machine);
        assert_eq!(request["from_message_id"].as_i64(), Some(14));
        machine
            .on_response(Ok(page(5, &[14])))
            .expect("the violation folds into degradation");
        assert!(matches!(
            machine.next_step().expect("a step"),
            LiveStep::Degraded(_)
        ));
    }

    #[test]
    fn flood_wait_arms_backoff_and_reissues_the_identical_request() {
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&new_message(5, 12, "a"));
        let _ = commit(&mut machine);
        let first = submit(&mut machine);
        machine
            .on_response(Err(TdError::Td {
                code: 429,
                message: "Too Many Requests: retry after 23".to_owned(),
            }))
            .expect("flood is retryable");
        match machine.next_step().expect("a step") {
            LiveStep::Backoff(backoff) => {
                assert_eq!(backoff.retry_after_secs, Some(23));
                assert_eq!(backoff.attempt, 1);
            }
            other => panic!("expected a backoff, got {other:?}"),
        }
        let second = submit(&mut machine);
        assert_eq!(first, second, "the re-issued request must be identical");
    }

    #[test]
    fn edit_signals_coalesce_into_one_refresh() {
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&json!({
            "@type": "updateMessageContent", "chat_id": 5, "message_id": 8,
        }));
        machine.on_update(&json!({
            "@type": "updateMessageEdited", "chat_id": 5, "message_id": 8, "edit_date": 9,
        }));
        let request = submit(&mut machine);
        assert_eq!(request["@type"].as_str(), Some("getMessage"));
        assert_eq!(request["chat_id"].as_i64(), Some(5));
        assert_eq!(request["message_id"].as_i64(), Some(8));
        let mut edited = message(5, 8, "edited");
        edited["edit_date"] = json!(1_700_000_100);
        machine.on_response(Ok(edited)).expect("refresh folds");
        let commit = commit(&mut machine);
        assert_eq!(observed_ids(&commit), vec![8]);
        assert_eq!(
            commit.advance_newest, None,
            "an edit never moves the cursor"
        );
        idle(&mut machine);
    }

    #[test]
    fn a_rejected_refresh_is_counted_never_silent() {
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&json!({
            "@type": "updateMessageContent", "chat_id": 5, "message_id": 8,
        }));
        submit(&mut machine);
        machine
            .on_response(Err(TdError::Td {
                code: 404,
                message: "Not Found".to_owned(),
            }))
            .expect("a rejected refresh is not fatal");
        let commit = commit(&mut machine);
        assert!(commit.changes.is_empty());
        assert_eq!(commit.refreshes_rejected, 1);
        idle(&mut machine);
    }

    #[test]
    fn a_refresh_answering_the_wrong_message_is_degraded_not_applied() {
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&json!({
            "@type": "updateMessageContent", "chat_id": 5, "message_id": 8,
        }));
        submit(&mut machine);
        machine
            .on_response(Ok(message(5, 9, "wrong message")))
            .expect("refresh folds");
        let commit = commit(&mut machine);
        assert!(commit.changes.is_empty(), "wrong identity is never applied");
        assert_eq!(commit.skipped_malformed, 1);
        idle(&mut machine);
    }

    #[test]
    fn a_deletion_supersedes_a_pending_refresh() {
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&json!({
            "@type": "updateMessageContent", "chat_id": 5, "message_id": 8,
        }));
        machine.on_update(&delete(5, &[8]));
        // The refresh was cancelled: the only step is the deletion commit.
        let commit = commit(&mut machine);
        assert_eq!(commit.changes, vec![LiveChange::Deleted { message_id: 8 }]);
        idle(&mut machine);
    }

    #[test]
    fn deletions_preserve_arrival_order_with_observations() {
        let mut machine = machine([LiveChat::new(5)]);
        machine.on_update(&new_message(5, 7, "hi"));
        machine.on_update(&delete(5, &[7]));
        let commit = commit(&mut machine);
        assert_eq!(commit.changes.len(), 2);
        assert!(matches!(commit.changes[0], LiveChange::Observed(_)));
        assert!(matches!(
            commit.changes[1],
            LiveChange::Deleted { message_id: 7 }
        ));
        idle(&mut machine);
    }

    #[test]
    fn cache_evictions_and_impermanent_deletes_are_ignored() {
        let mut machine = machine([LiveChat::new(5)]);
        machine.on_update(&json!({
            "@type": "updateDeleteMessages",
            "chat_id": 5,
            "message_ids": [7],
            "is_permanent": true,
            "from_cache": true,
        }));
        machine.on_update(&json!({
            "@type": "updateDeleteMessages",
            "chat_id": 5,
            "message_ids": [7],
            "is_permanent": false,
            "from_cache": false,
        }));
        idle(&mut machine);
    }

    #[test]
    fn sending_state_messages_wait_for_send_succeeded() {
        let mut machine = machine([tracked(5, 10)]);
        // The provisional message never ingests.
        let mut pending = message(5, 9_007_000_000, "outgoing");
        pending["sending_state"] = json!({"@type": "messageSendingStatePending"});
        machine.on_update(&json!({"@type": "updateNewMessage", "message": pending}));
        idle(&mut machine);

        // The final message arrives with its server id — via the
        // send-succeeded remap, bridging exactly like any live message.
        machine.on_update(&json!({
            "@type": "updateMessageSendSucceeded",
            "message": message(5, 11, "outgoing"),
            "old_message_id": 9_007_000_000i64,
        }));
        let live = commit(&mut machine);
        assert_eq!(observed_ids(&live), vec![11]);
        submit(&mut machine);
        machine
            .on_response(Ok(page(5, &[11, 10])))
            .expect("bridge folds");
        let connect = commit(&mut machine);
        assert_eq!(connect.advance_newest, Some(11));
        idle(&mut machine);
    }

    #[test]
    fn an_untracked_chat_buffers_reports_once_and_replays_on_track() {
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&new_message(999, 3, "ghost"));
        machine.on_update(&json!({
            "@type": "updateMessageContent", "chat_id": 999, "message_id": 3,
        }));
        machine.on_update(&delete(999, &[2]));
        match machine.next_step().expect("a step") {
            LiveStep::Unresolved { chat_id } => assert_eq!(chat_id, 999),
            other => panic!("expected unresolved, got {other:?}"),
        }
        // Reported once; nothing else pends while unresolved.
        idle(&mut machine);

        assert!(machine.track_chat(999, None));
        assert!(!machine.track_chat(999, None), "already tracked");
        // The buffer replays in arrival order: the record, then the
        // deletion; the edit signal becomes a refresh.
        let commit = commit(&mut machine);
        assert_eq!(commit.chat_id, 999);
        assert_eq!(commit.changes.len(), 2);
        assert!(matches!(commit.changes[0], LiveChange::Observed(_)));
        assert!(matches!(
            commit.changes[1],
            LiveChange::Deleted { message_id: 2 }
        ));
        assert_eq!(commit.advance_newest, None);
        let request = submit(&mut machine);
        assert_eq!(request["@type"].as_str(), Some("getMessage"));
        assert_eq!(request["message_id"].as_i64(), Some(3));
    }

    #[test]
    fn duplicate_updates_re_emit_but_never_re_advance() {
        let mut machine = machine([LiveChat::new(5)]);
        machine.on_update(&new_message(5, 7, "hi"));
        machine.on_update(&new_message(5, 7, "hi"));
        let commit = commit(&mut machine);
        // Both observations emit — the state layer's idempotent replay
        // decides they are one (SYNC-021); the cursor stays untouched.
        assert_eq!(observed_ids(&commit), vec![7, 7]);
        assert_eq!(commit.advance_newest, None);
        idle(&mut machine);
    }

    #[test]
    fn drains_are_deterministic_and_ascending_by_chat() {
        let mut machine = machine([LiveChat::new(3), LiveChat::new(1), LiveChat::new(2)]);
        machine.on_update(&new_message(2, 20, "b"));
        machine.on_update(&new_message(1, 10, "a"));
        machine.on_update(&new_message(3, 30, "c"));
        let order: Vec<i64> = (0..3).map(|_| commit(&mut machine).chat_id).collect();
        assert_eq!(order, vec![1, 2, 3]);
        idle(&mut machine);
    }

    #[test]
    fn a_malformed_live_message_is_counted_on_its_chat() {
        let mut machine = machine([LiveChat::new(5)]);
        machine.on_update(&json!({
            "@type": "updateNewMessage",
            "message": {"@type": "message", "id": 7, "chat_id": 5, "date": 1},
        }));
        let commit = commit(&mut machine);
        assert!(commit.changes.is_empty());
        assert_eq!(commit.skipped_malformed, 1);
        idle(&mut machine);
    }

    #[test]
    fn response_without_an_outstanding_request_is_a_typed_failure() {
        let mut machine = machine([LiveChat::new(5)]);
        let err = machine
            .on_response(Ok(json!({"@type": "messages", "messages": []})))
            .expect_err("nothing outstanding");
        assert_eq!(err, LiveError::NoRequestOutstanding);
        let repeat = machine.next_step().expect_err("machine is poisoned");
        assert_eq!(repeat, LiveError::NoRequestOutstanding);
    }

    #[test]
    fn a_runtime_level_failure_is_fatal_for_the_machine() {
        let mut machine = machine([tracked(5, 10)]);
        machine.on_update(&new_message(5, 12, "a"));
        let _ = commit(&mut machine);
        submit(&mut machine);
        let err = machine
            .on_response(Err(TdError::ClientClosed))
            .expect_err("client closure is not chat-specific");
        assert!(matches!(
            err,
            LiveError::Request {
                source: TdError::ClientClosed
            }
        ));
    }

    #[test]
    fn commits_drain_before_new_fetches_start() {
        let mut machine = machine([tracked(1, 10), tracked(2, 20)]);
        machine.on_update(&new_message(1, 12, "a"));
        machine.on_update(&new_message(2, 22, "b"));
        // Both chats have ready records and both need bridges; every
        // ready commit drains before the first submit.
        assert!(matches!(
            machine.next_step().expect("a step"),
            LiveStep::Commit(_)
        ));
        assert!(matches!(
            machine.next_step().expect("a step"),
            LiveStep::Commit(_)
        ));
        let request = submit(&mut machine);
        assert_eq!(request["chat_id"].as_i64(), Some(1), "ascending chat order");
    }
}
