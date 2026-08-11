//! Message change application: the append-only event log, its `messages`
//! projection, and per-chat sync windows (POL-3, DEC-015, SYNC-021/022).
//!
//! [`WriteTxn::apply_message_changes`] is idempotent by Telegram message
//! identity (SYNC-021): re-applying a batch that is already reflected —
//! a crashed run whose cursor never committed, an overlapping history page —
//! appends nothing and changes nothing. Combined with a cursor write under
//! the same transaction ([`WriteTxn::put_cursor`]), that gives SYNC-022 its
//! exactly-once *effect* from at-least-once delivery.
//!
//! # Retention mapping (POL-3, DEC-015)
//!
//! The event log is append-only in both retention modes — an event row is
//! never removed here, so watermarks (`event_seq`) never rewind and replay
//! stays recognizable. What the account's [`RetentionMode`] governs is
//! *content*, applied as the single sanctioned payload purge the schema
//! trigger allows:
//!
//! * **Audit** retains everything observed — every revision keeps its
//!   payload, and a deletion is a content-preserving tombstone (the deletion
//!   event itself never carries content, but the prior revisions do).
//! * **Mirror** keeps only what current Telegram state shows. An edit
//!   replaces prior revisions: the newest revision keeps its payload and
//!   every earlier revision of that message is purged to a marker. An
//!   observed deletion purges *all* of the message's revision content,
//!   leaving only the id/timestamp markers POL-3 keeps for sync correctness.
//!
//! The current revision of a live message always keeps its payload in both
//! modes — it is the message's current state, the projection's join target,
//! and what the replay check compares against. The mode only decides whether
//! *superseded* content (older revisions, deleted messages) survives.
//! Switching an account's mode mid-life is [`WriteTxn::set_retention_mode`],
//! which applies the Mirror invariant retroactively.

use gramdrive_model::identity::{
    AccountScope, ChatId, ChatKey, MessageId, MessageKey, SchemaFamily,
};
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{ReadTxn, RetentionMode, WriteTxn, scope_columns};

/// One full observed revision of a message: first sight or an edit — the
/// normalizer does not need to know which, [`WriteTxn::apply_message_changes`]
/// decides against the projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRevision {
    /// Telegram message identity within the batch's chat.
    pub message_id: MessageId,
    /// Sender, if known.
    pub sender_id: Option<i64>,
    /// When the message was sent (ms since the Unix epoch).
    pub sent_at_ms: i64,
    /// When this revision was edited, if it is an edit.
    pub edited_at_ms: Option<i64>,
    /// When this revision was observed by the source (SYNC-073 —
    /// timestamps are always source-explicit, never invented here).
    pub observed_at_ms: i64,
    /// Schema family of `payload` (DOM-023).
    pub payload_schema: SchemaFamily,
    /// The normalized message record — raw enough for lossless migration,
    /// never interpreted here.
    pub payload: Vec<u8>,
}

/// One observation from a source change feed or history page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageChange {
    /// A message revision was observed (first sight or edit).
    Observed(MessageRevision),
    /// The message's deletion was observed. Carries no content — a
    /// tombstone never implies history that was not observed (POL-3).
    Deleted {
        /// The deleted message.
        message_id: MessageId,
        /// When the deletion was observed (ms since the Unix epoch).
        observed_at_ms: i64,
    },
}

/// What one [`WriteTxn::apply_message_changes`] call actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AppliedChanges {
    /// Revisions recorded as first observations.
    pub observed: usize,
    /// Revisions recorded as edits of existing state.
    pub edited: usize,
    /// Deletions recorded as tombstones.
    pub deleted: usize,
    /// Changes skipped because the projection already reflects them —
    /// the replay half of SYNC-021.
    pub skipped: usize,
}

/// Current observed state of one message — the projection row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageState {
    /// Telegram message identity.
    pub message_id: MessageId,
    /// Sender, if known.
    pub sender_id: Option<i64>,
    /// When the message was sent (ms since the Unix epoch).
    pub sent_at_ms: i64,
    /// When the current revision was edited, if ever.
    pub edited_at_ms: Option<i64>,
    /// POL-3 tombstone bit.
    pub is_deleted: bool,
    /// The event that produced this state.
    pub latest_event_seq: i64,
}

/// What kind of observation an event row records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageEventKind {
    /// First sight of a message.
    Observed,
    /// An edit: a full new revision.
    Edited,
    /// An observed deletion; never carries content.
    Deleted,
}

impl MessageEventKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Observed => "observed",
            Self::Edited => "edited",
            Self::Deleted => "deleted",
        }
    }

    pub(super) fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "observed" => Ok(Self::Observed),
            "edited" => Ok(Self::Edited),
            "deleted" => Ok(Self::Deleted),
            other => Err(StateError::CorruptRow {
                table: "message_events",
                detail: format!("unknown event_kind '{other}'"),
            }),
        }
    }
}

/// The normalized payload carried by an event, with its schema family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagePayload {
    /// Schema family of `bytes` (DOM-023).
    pub schema: SchemaFamily,
    /// The normalized record bytes.
    pub bytes: Vec<u8>,
}

/// One row of the append-only event log (POL-3, DEC-015).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEventRecord {
    /// Watermark-safe sequence number (never reused).
    pub event_seq: i64,
    /// The message the event is about.
    pub message_id: MessageId,
    /// What was observed.
    pub kind: MessageEventKind,
    /// When it was observed (ms since the Unix epoch).
    pub observed_at_ms: i64,
    /// The revision payload; absent for deletions and purged events.
    pub payload: Option<MessagePayload>,
}

/// The contiguous `[oldest, newest]` window of message ids already
/// normalized for a chat (SYNC-021).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncWindow {
    /// Oldest message id inside the window.
    pub oldest: MessageId,
    /// Newest message id inside the window.
    pub newest: MessageId,
}

/// Resumable history-traversal state of one chat (SYNC-021).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatSyncRecord {
    /// The loaded window, once anything is loaded.
    pub window: Option<SyncWindow>,
    /// Whether backfill reached the beginning of history.
    pub history_complete: bool,
    /// When the chat last made sync progress, if ever.
    pub last_sync_at_ms: Option<i64>,
}

fn payload_from_columns(
    schema: Option<i64>,
    bytes: Option<Vec<u8>>,
) -> Result<Option<MessagePayload>, StateError> {
    match (schema, bytes) {
        (None, None) => Ok(None),
        (Some(schema), Some(bytes)) => {
            let schema = u16::try_from(schema).map_err(|_| StateError::CorruptRow {
                table: "message_events",
                detail: format!("payload_schema {schema} does not fit the schema-family range"),
            })?;
            Ok(Some(MessagePayload {
                schema: SchemaFamily(schema),
                bytes,
            }))
        }
        _ => Err(StateError::CorruptRow {
            table: "message_events",
            detail: "payload and payload_schema must be present together".to_owned(),
        }),
    }
}

fn read_message_state(row: &Row<'_>) -> Result<MessageState, rusqlite::Error> {
    Ok(MessageState {
        message_id: MessageId(row.get(0)?),
        sender_id: row.get(1)?,
        sent_at_ms: row.get(2)?,
        edited_at_ms: row.get(3)?,
        is_deleted: row.get(4)?,
        latest_event_seq: row.get(5)?,
    })
}

const MESSAGE_COLUMNS: &str =
    "message_id, sender_id, sent_at_ms, edited_at_ms, is_deleted, latest_event_seq";

impl ReadTxn<'_> {
    /// Current retained normalized payload of one live message.
    ///
    /// The join follows `messages.latest_event_seq`, so stale history pages
    /// cannot overwrite a newer attachment projection after change
    /// application has rejected them.
    pub fn current_message_payload(
        &self,
        message: &MessageKey,
    ) -> Result<Option<MessagePayload>, StateError> {
        let (account_id, namespace) = scope_columns(&message.chat.scope);
        let raw: Option<(Option<i64>, Option<Vec<u8>>)> = self
            .conn()
            .prepare_cached(
                "SELECT e.payload_schema, e.payload
                 FROM messages m
                 JOIN message_events e ON e.event_seq = m.latest_event_seq
                 WHERE m.account_id = ?1 AND m.namespace_version = ?2 AND m.chat_id = ?3
                   AND m.message_id = ?4 AND m.is_deleted = 0",
            )?
            .query_row(
                params![
                    account_id,
                    namespace,
                    message.chat.chat_id.0,
                    message.message_id.0
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        raw.map(|(schema, bytes)| payload_from_columns(schema, bytes))
            .transpose()
            .map(Option::flatten)
    }

    /// The current observed state of one message.
    pub fn message(&self, key: &MessageKey) -> Result<Option<MessageState>, StateError> {
        let (account_id, namespace) = scope_columns(&key.chat.scope);
        Ok(self
            .conn()
            .prepare_cached(&format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                   AND message_id = ?4"
            ))?
            .query_row(
                params![account_id, namespace, key.chat.chat_id.0, key.message_id.0],
                read_message_state,
            )
            .optional()?)
    }

    /// One id-ordered page of a chat's messages after `after`, for
    /// resumable, idempotent history traversal (SYNC-021).
    pub fn messages_after(
        &self,
        chat: &ChatKey,
        after: MessageId,
        limit: u32,
    ) -> Result<Vec<MessageState>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let mut statement = self.conn().prepare_cached(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages
             WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
               AND message_id > ?4
             ORDER BY message_id LIMIT ?5"
        ))?;
        let rows = statement.query_map(
            params![
                account_id,
                namespace,
                chat.chat_id.0,
                after.0,
                i64::from(limit)
            ],
            read_message_state,
        )?;
        let mut messages = Vec::new();
        for row in rows {
            messages.push(row?);
        }
        Ok(messages)
    }

    /// A chat's messages inside the half-open send-time window
    /// `[start_ms, end_ms)`, for month/year partition rendering (SYNC-031).
    pub fn messages_in_window(
        &self,
        chat: &ChatKey,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<MessageState>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let mut statement = self.conn().prepare_cached(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages
             WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
               AND sent_at_ms >= ?4 AND sent_at_ms < ?5"
        ))?;
        let rows = statement.query_map(
            params![account_id, namespace, chat.chat_id.0, start_ms, end_ms],
            read_message_state,
        )?;
        let mut messages = Vec::new();
        for row in rows {
            messages.push(row?);
        }
        Ok(messages)
    }

    /// A chat's events after the given watermark, in order — render
    /// catch-up (SYNC-022, SYNC-024).
    pub fn events_after(
        &self,
        chat: &ChatKey,
        after_seq: i64,
        limit: u32,
    ) -> Result<Vec<MessageEventRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT event_seq, message_id, event_kind, observed_at_ms, payload_schema, payload
             FROM message_events
             WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
               AND event_seq > ?4
             ORDER BY event_seq LIMIT ?5",
        )?;
        let rows = statement.query_map(
            params![
                account_id,
                namespace,
                chat.chat_id.0,
                after_seq,
                i64::from(limit)
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                ))
            },
        )?;
        let mut events = Vec::new();
        for row in rows {
            let (event_seq, message_id, kind, observed_at_ms, schema, bytes) = row?;
            events.push(MessageEventRecord {
                event_seq,
                message_id: MessageId(message_id),
                kind: MessageEventKind::parse(&kind)?,
                observed_at_ms,
                payload: payload_from_columns(schema, bytes)?,
            });
        }
        Ok(events)
    }

    /// The highest event sequence recorded for a chat — the natural render
    /// watermark for its documents (SYNC-024). Zero for a chat with no
    /// events: sequences are AUTOINCREMENT and start at one.
    pub fn latest_event_seq(&self, chat: &ChatKey) -> Result<i64, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let max: Option<i64> = self
            .conn()
            .prepare_cached(
                "SELECT max(event_seq) FROM message_events
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3",
            )?
            .query_row(params![account_id, namespace, chat.chat_id.0], |row| {
                row.get(0)
            })?;
        Ok(max.unwrap_or(0))
    }

    /// The resumable history-traversal state of one chat (SYNC-021).
    pub fn chat_sync_state(&self, chat: &ChatKey) -> Result<Option<ChatSyncRecord>, StateError> {
        type RawSyncState = (Option<i64>, Option<i64>, bool, Option<i64>);
        let (account_id, namespace) = scope_columns(&chat.scope);
        let raw: Option<RawSyncState> = self
            .conn()
            .prepare_cached(
                "SELECT oldest_loaded_message_id, newest_loaded_message_id,
                        history_complete, last_sync_at_ms
                 FROM chat_sync_state
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3",
            )?
            .query_row(params![account_id, namespace, chat.chat_id.0], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .optional()?;
        let Some((oldest, newest, history_complete, last_sync_at_ms)) = raw else {
            return Ok(None);
        };
        let window = match (oldest, newest) {
            (None, None) => None,
            (Some(oldest), Some(newest)) => Some(SyncWindow {
                oldest: MessageId(oldest),
                newest: MessageId(newest),
            }),
            _ => {
                return Err(StateError::CorruptRow {
                    table: "chat_sync_state",
                    detail: "window bounds must be present together".to_owned(),
                });
            }
        };
        Ok(Some(ChatSyncRecord {
            window,
            history_complete,
            last_sync_at_ms,
        }))
    }

    /// Runnable listed chats of one scope that still need history, least
    /// recently *given a turn* first — the bounded backfill backlog
    /// (SYNC-021).
    ///
    /// Cursor rows may outlive list membership so a chat can resume if it
    /// reappears. Current eligibility therefore comes from
    /// `chat_list_entries`; canonical metadata outside every Telegram list is
    /// never repeatedly scanned. Terminal retry-on-demand rows — a chat
    /// Telegram refused or a request that failed — stay out of background
    /// scheduling; an explicit eligible foreground request can still retry
    /// them through a point read.
    ///
    /// A `degraded` chat is *not* one of those. Degradation is this engine's
    /// own fence — a live gap, a buffer overflow, an edit whose individual
    /// ids could not be retained — and it means "re-crawl me from the top",
    /// not "the source said no". Excluding it starved every chat that ever
    /// hit one: on a real preserved profile, 59 of 410 incomplete listed
    /// chats sat fenced indefinitely, reachable only if the user happened to
    /// open them in Finder (BUG-260728-2qfzbd). A degradation that names a
    /// retry deadline waits for it; one that does not is runnable now.
    ///
    /// The order is keyed on `last_backfill_at_ms` — when this chat was last
    /// handed a history turn — and deliberately *not* on `last_sync_at_ms`,
    /// which the live-update path stamps too. Ordering by the latter let
    /// ordinary incoming messages reset a chat's place in the queue, so the
    /// busiest correspondences were the ones that never crawled backward: on
    /// a preserved profile the reported chat held an unmoved backward
    /// frontier for over an hour while the account indexed 123k messages from
    /// quieter chats (BUG-260728-2qfzbd). A never-turned chat sorts first
    /// (NULL leads in SQLite ASC), so every incomplete chat gets a turn
    /// before any chat gets a second one.
    pub fn backfill_backlog(
        &self,
        scope: &AccountScope,
        limit: u32,
        now_ms: i64,
    ) -> Result<Vec<ChatId>, StateError> {
        let (account_id, namespace) = scope_columns(scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT s.chat_id
             FROM chat_sync_state s
             JOIN chats c
               ON c.account_id = s.account_id
              AND c.namespace_version = s.namespace_version
              AND c.chat_id = s.chat_id
             LEFT JOIN chat_content_progress p
               ON p.account_id = s.account_id
              AND p.namespace_version = s.namespace_version
              AND p.chat_id = s.chat_id
             WHERE s.account_id = ?1 AND s.namespace_version = ?2
               AND s.history_complete = 0
               AND c.deleted_at_ms IS NULL
               AND c.is_protected = 0
               AND EXISTS (
                   SELECT 1 FROM chat_list_entries e
                   WHERE e.account_id = s.account_id
                     AND e.namespace_version = s.namespace_version
                     AND e.chat_id = s.chat_id
               )
               AND (p.chat_id IS NULL
                    OR p.phase IN ('pending', 'syncing', 'cancelled')
                    OR (p.phase = 'degraded'
                        AND (p.retry_at_ms IS NULL OR p.retry_at_ms <= ?4)))
             ORDER BY s.last_backfill_at_ms, s.chat_id LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![account_id, namespace, i64::from(limit), now_ms],
            |row| Ok(ChatId(row.get(0)?)),
        )?;
        let mut chats = Vec::new();
        for row in rows {
            chats.push(row?);
        }
        Ok(chats)
    }
}

impl WriteTxn<'_> {
    /// Applies one batch of message observations to a chat, idempotently by
    /// Telegram message identity (SYNC-021).
    ///
    /// For every change, the projection decides what the observation *is*:
    /// a new message appends an `observed` event, a differing revision of a
    /// known message appends an `edited` event, an unseen deletion appends a
    /// `deleted` tombstone — and anything the projection already reflects is
    /// skipped whole, so replaying a batch after a crash whose cursor never
    /// committed appends nothing.
    ///
    /// Three deliberate skips beyond exact replay:
    /// a deletion of a message that was never observed is skipped (POL-3 —
    /// history never observed is never implied); a revision arriving after
    /// a deletion is skipped (a replayed history page must not resurrect a
    /// message whose deletion was already witnessed); and a revision whose
    /// edit time is older than the projected one is skipped (a history page
    /// fetched before an edit, replayed after it, must not rewind state).
    ///
    /// The account's [`RetentionMode`] is read once for the batch and governs
    /// content purging — Mirror replaces prior revisions and purges deleted
    /// messages' content, Audit retains everything (module docs). The
    /// deletion of a delete-for-everyone and a delete-for-me both reach this
    /// method as one [`MessageChange::Deleted`]: TDLib exposes no reliable
    /// signal distinguishing them (the archive mirrors this account's own
    /// view, in which both are permanent removals), so they map identically
    /// and the archive claims nothing about which it was.
    ///
    /// The chat's canonical row must exist ([`WriteTxn::upsert_chat`]), which
    /// implies its account row exists; a batch for a chat whose account is
    /// gone is [`StateError::RowNotFound`]. Atomicity with the cursor is the
    /// transaction's job: call [`WriteTxn::put_cursor`] under the same
    /// transaction (SYNC-022).
    pub fn apply_message_changes(
        &self,
        chat: &ChatKey,
        changes: &[MessageChange],
    ) -> Result<AppliedChanges, StateError> {
        let retention = self
            .read()
            .retention_mode(chat.scope.account)?
            .ok_or(StateError::RowNotFound { entity: "account" })?;
        let mut applied = AppliedChanges::default();
        for change in changes {
            match change {
                MessageChange::Observed(revision) => {
                    self.apply_revision(chat, revision, retention, &mut applied)?;
                }
                MessageChange::Deleted {
                    message_id,
                    observed_at_ms,
                } => {
                    self.apply_deletion(
                        chat,
                        *message_id,
                        *observed_at_ms,
                        retention,
                        &mut applied,
                    )?;
                }
            }
        }
        Ok(applied)
    }

    fn apply_revision(
        &self,
        chat: &ChatKey,
        revision: &MessageRevision,
        retention: RetentionMode,
        applied: &mut AppliedChanges,
    ) -> Result<(), StateError> {
        type CurrentRevision = (i64, Option<i64>, bool, Option<i64>, Option<Vec<u8>>);
        let (account_id, namespace) = scope_columns(&chat.scope);
        // The projection row plus the payload of the event that produced it:
        // everything needed to recognize a replay without trusting the feed.
        let current: Option<CurrentRevision> = self
            .conn()
            .prepare_cached(
                "SELECT m.sent_at_ms, m.edited_at_ms, m.is_deleted,
                        e.payload_schema, e.payload
                 FROM messages m JOIN message_events e ON e.event_seq = m.latest_event_seq
                 WHERE m.account_id = ?1 AND m.namespace_version = ?2 AND m.chat_id = ?3
                   AND m.message_id = ?4",
            )?
            .query_row(
                params![account_id, namespace, chat.chat_id.0, revision.message_id.0],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let event_kind = match &current {
            None => MessageEventKind::Observed,
            Some((_, _, true, _, _)) => {
                // Deletion already witnessed; a replayed revision must not
                // resurrect the message (POL-3).
                applied.skipped += 1;
                return Ok(());
            }
            Some((sent_at_ms, edited_at_ms, false, schema, payload)) => {
                let same_payload = *schema == Some(i64::from(revision.payload_schema.0))
                    && payload.as_deref() == Some(revision.payload.as_slice());
                if *sent_at_ms == revision.sent_at_ms
                    && *edited_at_ms == revision.edited_at_ms
                    && same_payload
                {
                    applied.skipped += 1;
                    return Ok(());
                }
                // A revision older than the projected one — a history page
                // fetched before an edit, replayed after it — is a stale
                // replay, not an edit: applying it would rewind the current
                // state to content Telegram no longer shows (SYNC-021).
                // Edit times are per-message monotonic at the source.
                let stored_revised_at = edited_at_ms.unwrap_or(*sent_at_ms);
                let incoming_revised_at = revision.edited_at_ms.unwrap_or(revision.sent_at_ms);
                if incoming_revised_at < stored_revised_at {
                    applied.skipped += 1;
                    return Ok(());
                }
                MessageEventKind::Edited
            }
        };
        self.conn()
            .prepare_cached(
                "INSERT INTO message_events (account_id, namespace_version, chat_id,
                                             message_id, event_kind, observed_at_ms,
                                             payload_schema, payload)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?
            .execute(params![
                account_id,
                namespace,
                chat.chat_id.0,
                revision.message_id.0,
                event_kind.as_str(),
                revision.observed_at_ms,
                i64::from(revision.payload_schema.0),
                revision.payload,
            ])?;
        let event_seq = self.conn().last_insert_rowid();
        self.conn()
            .prepare_cached(
                "INSERT INTO messages (account_id, namespace_version, chat_id, message_id,
                                       sender_id, sent_at_ms, edited_at_ms, is_deleted,
                                       latest_event_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)
                 ON CONFLICT (account_id, namespace_version, chat_id, message_id)
                 DO UPDATE SET
                     sender_id = excluded.sender_id,
                     sent_at_ms = excluded.sent_at_ms,
                     edited_at_ms = excluded.edited_at_ms,
                     latest_event_seq = excluded.latest_event_seq",
            )?
            .execute(params![
                account_id,
                namespace,
                chat.chat_id.0,
                revision.message_id.0,
                revision.sender_id,
                revision.sent_at_ms,
                revision.edited_at_ms,
                event_seq,
            ])?;
        match event_kind {
            MessageEventKind::Observed => applied.observed += 1,
            MessageEventKind::Edited => {
                applied.edited += 1;
                // Mirror replaces prior revisions: the message keeps only its
                // current content (the event just appended), every earlier
                // revision purged to a marker (POL-3). A first observation has
                // no prior revision, so only an edit triggers this.
                if retention == RetentionMode::Mirror {
                    self.purge_message_content(chat, revision.message_id, Some(event_seq))?;
                }
            }
            MessageEventKind::Deleted => {}
        }
        Ok(())
    }

    fn apply_deletion(
        &self,
        chat: &ChatKey,
        message_id: MessageId,
        observed_at_ms: i64,
        retention: RetentionMode,
        applied: &mut AppliedChanges,
    ) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let current: Option<bool> = self
            .conn()
            .prepare_cached(
                "SELECT is_deleted FROM messages
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                   AND message_id = ?4",
            )?
            .query_row(
                params![account_id, namespace, chat.chat_id.0, message_id.0],
                |row| row.get(0),
            )
            .optional()?;
        match current {
            // Never observed: nothing to delete, and POL-3 forbids implying
            // the message ever existed.
            None => applied.skipped += 1,
            // Already a tombstone: replay.
            Some(true) => applied.skipped += 1,
            Some(false) => {
                self.conn()
                    .prepare_cached(
                        "INSERT INTO message_events (account_id, namespace_version, chat_id,
                                                     message_id, event_kind, observed_at_ms,
                                                     payload_schema, payload)
                         VALUES (?1, ?2, ?3, ?4, 'deleted', ?5, NULL, NULL)",
                    )?
                    .execute(params![
                        account_id,
                        namespace,
                        chat.chat_id.0,
                        message_id.0,
                        observed_at_ms,
                    ])?;
                let event_seq = self.conn().last_insert_rowid();
                self.conn()
                    .prepare_cached(
                        "UPDATE messages SET is_deleted = 1, latest_event_seq = ?5
                         WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                           AND message_id = ?4",
                    )?
                    .execute(params![
                        account_id,
                        namespace,
                        chat.chat_id.0,
                        message_id.0,
                        event_seq,
                    ])?;
                // Mirror purges a deleted message's content entirely: the
                // tombstone just appended carries none, and every revision it
                // supersedes is purged to a marker (POL-3). Audit keeps those
                // revisions and the attachment metadata/verified byte links
                // observed with them. Mirror removes attachment ownership in
                // this same transaction; it never leaves content rows behind
                // for a deleted message.
                if retention == RetentionMode::Mirror {
                    self.purge_message_content(chat, message_id, None)?;
                    let message = MessageKey {
                        chat: *chat,
                        message_id,
                    };
                    for attachment in self.read().attachments_of_message(&message)? {
                        self.purge_attachment_materialization(
                            &attachment.facts.key,
                            observed_at_ms,
                        )?;
                    }
                    self.conn()
                        .prepare_cached(
                            "DELETE FROM attachments
                             WHERE account_id = ?1 AND namespace_version = ?2
                               AND chat_id = ?3 AND message_id = ?4",
                        )?
                        .execute(params![account_id, namespace, chat.chat_id.0, message_id.0,])?;
                }
                applied.deleted += 1;
            }
        }
        Ok(())
    }

    /// Purges every retained payload for a chat after an authoritative
    /// chat-level content restriction becomes active.
    ///
    /// Message projection rows and event identity/timestamps remain as minimal
    /// sync tombstones. The update is idempotent and intentionally ignores the
    /// account retention mode: Telegram restrictions override Mirror and
    /// Audit.
    pub fn purge_restricted_chat_message_content(
        &self,
        chat: &ChatKey,
    ) -> Result<usize, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        Ok(self
            .conn()
            .prepare_cached(
                "UPDATE message_events SET payload = NULL, payload_schema = NULL
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                   AND payload IS NOT NULL",
            )?
            .execute(params![account_id, namespace, chat.chat_id.0])?)
    }

    /// Applies a per-message authoritative restriction prospectively while
    /// preserving only the latest body-free placeholder payload.
    ///
    /// In Audit this removes any earlier allowed revision immediately; a later
    /// removal of the restriction cannot resurrect those payloads.
    pub fn purge_restricted_message_history(
        &self,
        message: &MessageKey,
    ) -> Result<usize, StateError> {
        let state = self
            .read()
            .message(message)?
            .ok_or(StateError::RowNotFound { entity: "message" })?;
        self.purge_message_content(
            &message.chat,
            message.message_id,
            Some(state.latest_event_seq),
        )
    }

    /// Purges the payload of a message's event rows to a marker (the schema's
    /// single sanctioned `message_events` update) — the Mirror content purge.
    ///
    /// `keep` is the one event whose payload survives (the current revision of
    /// a live message); `None` purges every revision (a deleted message keeps
    /// no content). Only rows that still carry a payload are touched, so the
    /// call is idempotent and its changed-count is the content actually
    /// removed. Deletion-kind rows already hold no payload and are skipped by
    /// that guard.
    fn purge_message_content(
        &self,
        chat: &ChatKey,
        message_id: MessageId,
        keep: Option<i64>,
    ) -> Result<usize, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let changed = match keep {
            Some(keep_seq) => self
                .conn()
                .prepare_cached(
                    "UPDATE message_events SET payload = NULL, payload_schema = NULL
                     WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                       AND message_id = ?4 AND payload IS NOT NULL AND event_seq <> ?5",
                )?
                .execute(params![
                    account_id,
                    namespace,
                    chat.chat_id.0,
                    message_id.0,
                    keep_seq,
                ])?,
            None => self
                .conn()
                .prepare_cached(
                    "UPDATE message_events SET payload = NULL, payload_schema = NULL
                     WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                       AND message_id = ?4 AND payload IS NOT NULL",
                )?
                .execute(params![account_id, namespace, chat.chat_id.0, message_id.0])?,
        };
        Ok(changed)
    }

    /// Records a chat's history-traversal state (SYNC-021).
    ///
    /// Call it in the same transaction as the
    /// [`WriteTxn::apply_message_changes`] that loaded the window — the
    /// bounds move only with normalized state (SYNC-022).
    pub fn record_chat_sync(
        &self,
        chat: &ChatKey,
        record: &ChatSyncRecord,
    ) -> Result<(), StateError> {
        if let Some(window) = &record.window
            && window.oldest.0 > window.newest.0
        {
            return Err(StateError::InvalidArgument {
                what: "sync window oldest must not exceed newest",
            });
        }
        let (account_id, namespace) = scope_columns(&chat.scope);
        self.conn()
            .prepare_cached(
                "INSERT INTO chat_sync_state (account_id, namespace_version, chat_id,
                                              oldest_loaded_message_id,
                                              newest_loaded_message_id,
                                              history_complete, last_sync_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (account_id, namespace_version, chat_id) DO UPDATE SET
                     oldest_loaded_message_id = excluded.oldest_loaded_message_id,
                     newest_loaded_message_id = excluded.newest_loaded_message_id,
                     history_complete = excluded.history_complete,
                     last_sync_at_ms = excluded.last_sync_at_ms",
            )?
            .execute(params![
                account_id,
                namespace,
                chat.chat_id.0,
                record.window.map(|window| window.oldest.0),
                record.window.map(|window| window.newest.0),
                record.history_complete,
                record.last_sync_at_ms,
            ])?;
        Ok(())
    }

    /// Records that this chat was handed a backward-history turn (SYNC-021).
    ///
    /// The backlog's rotation key, and the only writer of it. Call it where
    /// the scheduler *selects* a chat, not where a crawl succeeds: a turn
    /// that ends in a spacing wait, a source error, or an empty page is
    /// still a turn, and a key advanced only by success is a key a
    /// repeatedly failing chat holds at the head of the queue forever.
    ///
    /// Deliberately separate from [`WriteTxn::record_chat_sync`], which
    /// carries the cursor the crawl moved. Merging them would put the
    /// rotation key back on the live-update path that stamps that record,
    /// which is the starvation this exists to prevent (BUG-260728-2qfzbd).
    ///
    /// Silently does nothing when the chat has no cursor row yet: rows are
    /// created by the chat trigger, and a chat with no row is not in the
    /// backlog to be scheduled.
    pub fn record_backfill_turn(&self, chat: &ChatKey, at_ms: i64) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        self.conn()
            .prepare_cached(
                "UPDATE chat_sync_state SET last_backfill_at_ms = ?4
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3",
            )?
            .execute(params![account_id, namespace, chat.chat_id.0, at_ms])?;
        Ok(())
    }
}
