//! Render state per generated document: versions, the event-sequence input
//! watermark, and the dirty worklist (SYNC-024, SYNC-030..033).
//!
//! The watermark protocol closes the render/append race without locks. A
//! renderer reads a document's inputs up to watermark `W`
//! ([`ReadTxn::latest_event_seq`], [`ReadTxn::events_after`]), renders
//! outside any transaction, then publishes with
//! [`WriteTxn::publish_render`], which re-checks the chat's event log
//! *inside the publishing transaction*: if events beyond `W` arrived while
//! rendering, the document stays dirty and the worklist re-renders it —
//! published bytes never silently claim to reflect events they predate. A
//! publication whose watermark is *below* the recorded one is refused
//! outright ([`StateError::WatermarkRegression`]): watermarks only advance.

use std::collections::BTreeSet;

use gramdrive_model::identity::{
    AppearanceKey, CanonicalKey, ChatKey, ChatListKind, ContentHash, DocFormat, DocPartition,
    ItemId, ItemKey, MessageId, MonthDirKey, SchemaFamily,
};
use gramdrive_model::version::ContentVersion;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{
    MessageEventKind, MessagePayload, ReadTxn, WriteTxn, hash_columns, hash_from_columns,
    item_id_from_column, scope_columns, size_from_column, size_to_column,
};

/// One persisted revision/deletion event supplied to the render coordinator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderEventInput {
    /// Watermark-safe event sequence.
    pub event_seq: i64,
    /// Observation kind.
    pub kind: MessageEventKind,
    /// Source observation time in absolute UTC milliseconds.
    pub observed_at_ms: i64,
    /// Normalized payload for observations/edits; absent for deletions and
    /// Mirror-purged history.
    pub payload: Option<MessagePayload>,
}

/// One message and all retained events in a bounded monthly snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderMessageInput {
    /// Telegram message identity within the chat.
    pub message_id: MessageId,
    /// Sender id, when known.
    pub sender_id: Option<i64>,
    /// Absolute Telegram send time in UTC milliseconds.
    pub sent_at_ms: i64,
    /// Retained events in ascending sequence order.
    pub events: Vec<RenderEventInput>,
}

/// One consistent monthly render input snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonthRenderSnapshot {
    /// Chat being rendered.
    pub chat: ChatKey,
    /// Chat-global event watermark pinned by the read transaction.
    pub input_watermark_seq: i64,
    /// Monotonic byte-shaping account policy generation pinned by the same
    /// read transaction as the messages and watermark.
    pub render_generation: i64,
    /// Persisted timezone used for civil partitioning and rendered provenance.
    pub display_timezone: String,
    /// Retention projection used to select visible revisions.
    pub retention_mode: crate::repo::RetentionMode,
    /// UTC-inclusive lower bound of the local civil month.
    pub start_ms: i64,
    /// UTC-exclusive upper bound of the local civil month.
    pub end_ms: i64,
    /// Messages in canonical `(sent_at_ms, message_id)` order.
    pub messages: Vec<RenderMessageInput>,
}

/// One provider-visible appearance of a generated document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCatalogEntry {
    /// Appearance item carrying provider-visible content facts.
    pub item: ItemId,
    /// Chat-list view containing the appearance.
    pub view: ChatListKind,
    /// Monthly document format.
    pub format: DocFormat,
    /// Schema family embedded in stable identity.
    pub schema_family: SchemaFamily,
}

/// Durable render state of one generated document (domain-model
/// § Generated document).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderStateRecord {
    /// The generated-document item.
    pub item: ItemId,
    /// Renderer implementation version that produced the published bytes
    /// (SYNC-030).
    pub renderer_version: u32,
    /// Record-schema revision of the published bytes (DOM-023).
    pub schema_version: u32,
    /// The published bytes reflect every event at or below this sequence
    /// (SYNC-024).
    pub input_watermark_seq: i64,
    /// Content version providers see (DOM-006), once published.
    pub content_version: Option<ContentVersion>,
    /// Hash of the published bytes, once materialized.
    pub content_hash: Option<ContentHash>,
    /// Size of the published bytes, once materialized.
    pub logical_size: Option<u64>,
    /// Whether the document needs re-rendering.
    pub dirty: bool,
    /// Why this document was deliberately removed from the dirty worklist.
    ///
    /// A skipped row remains durable and countable; publishing or explicitly
    /// re-queueing it clears this marker.
    pub skip_reason: Option<RenderSkipReason>,
    /// When policy last excluded this document from rendering (ms since the
    /// Unix epoch).
    pub skipped_at_ms: Option<i64>,
    /// When the document was last rendered (ms since the Unix epoch).
    pub rendered_at_ms: Option<i64>,
}

/// A durable reason a generated document was not rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderSkipReason {
    /// Current chat policy forbids publishing this document.
    PolicyExcluded,
}

impl RenderSkipReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::PolicyExcluded => "policy-excluded",
        }
    }

    fn from_column(value: String) -> Result<Self, StateError> {
        match value.as_str() {
            "policy-excluded" => Ok(Self::PolicyExcluded),
            _ => Err(StateError::CorruptRow {
                table: "render_state",
                detail: format!("unknown render skip reason {value:?}"),
            }),
        }
    }
}

/// What a renderer publishes (SYNC-024).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderOutput {
    /// The content version the published bytes carry (DOM-006).
    pub content_version: ContentVersion,
    /// Hash of the published bytes, if materialized.
    pub content_hash: Option<ContentHash>,
    /// Size of the published bytes.
    pub logical_size: u64,
}

/// The outcome of [`WriteTxn::publish_render`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderPublish {
    /// Whether the document is clean after publication. `false` means
    /// events beyond the published watermark arrived while rendering; the
    /// state is recorded but the document stays on the dirty worklist.
    pub clean: bool,
}

struct RawRenderState {
    item_id: Vec<u8>,
    renderer_version: i64,
    schema_version: i64,
    input_watermark_seq: i64,
    content_version: Option<String>,
    content_hash_algo: Option<String>,
    content_hash: Option<Vec<u8>>,
    logical_size: Option<i64>,
    dirty: bool,
    skip_reason: Option<String>,
    skipped_at_ms: Option<i64>,
    rendered_at_ms: Option<i64>,
}

fn read_render_state(row: &Row<'_>) -> Result<RawRenderState, rusqlite::Error> {
    Ok(RawRenderState {
        item_id: row.get(0)?,
        renderer_version: row.get(1)?,
        schema_version: row.get(2)?,
        input_watermark_seq: row.get(3)?,
        content_version: row.get(4)?,
        content_hash_algo: row.get(5)?,
        content_hash: row.get(6)?,
        logical_size: row.get(7)?,
        dirty: row.get(8)?,
        skip_reason: row.get(9)?,
        skipped_at_ms: row.get(10)?,
        rendered_at_ms: row.get(11)?,
    })
}

fn version_from_column(table: &'static str, value: i64) -> Result<u32, StateError> {
    u32::try_from(value).map_err(|_| StateError::CorruptRow {
        table,
        detail: format!("version {value} does not fit u32"),
    })
}

fn finish_render_state(raw: RawRenderState) -> Result<RenderStateRecord, StateError> {
    Ok(RenderStateRecord {
        item: item_id_from_column("render_state", &raw.item_id)?,
        renderer_version: version_from_column("render_state", raw.renderer_version)?,
        schema_version: version_from_column("render_state", raw.schema_version)?,
        input_watermark_seq: raw.input_watermark_seq,
        content_version: raw
            .content_version
            .map(|text| {
                ContentVersion::new(text).map_err(|error| StateError::CorruptRow {
                    table: "render_state",
                    detail: format!("content_version does not parse: {error}"),
                })
            })
            .transpose()?,
        content_hash: hash_from_columns("render_state", raw.content_hash_algo, raw.content_hash)?,
        logical_size: raw
            .logical_size
            .map(|size| size_from_column("render_state", size))
            .transpose()?,
        dirty: raw.dirty,
        skip_reason: raw
            .skip_reason
            .map(RenderSkipReason::from_column)
            .transpose()?,
        skipped_at_ms: raw.skipped_at_ms,
        rendered_at_ms: raw.rendered_at_ms,
    })
}

const RENDER_COLUMNS: &str = "item_id, renderer_version, schema_version, input_watermark_seq,
     content_version, content_hash_algo, content_hash, logical_size, dirty, skip_reason,
     skipped_at_ms, rendered_at_ms";

impl ReadTxn<'_> {
    /// The render state of one generated document.
    pub fn render_state(&self, item: &ItemId) -> Result<Option<RenderStateRecord>, StateError> {
        let raw = self
            .conn()
            .prepare_cached(&format!(
                "SELECT {RENDER_COLUMNS} FROM render_state WHERE item_id = ?1"
            ))?
            .query_row(params![item.as_bytes()], read_render_state)
            .optional()?;
        raw.map(finish_render_state).transpose()
    }

    /// The re-render worklist: every dirty document, least advanced first.
    ///
    /// A freshly dirtied low-sorting chat must rotate behind documents that
    /// have never been published or were published earlier. Otherwise a deep
    /// active crawl can monopolize a small bounded work quantum indefinitely.
    pub fn dirty_render_items(&self, limit: u32) -> Result<Vec<ItemId>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT r.item_id
             FROM render_state r
             JOIN items i ON i.item_id = r.item_id
             WHERE r.dirty = 1 AND i.deleted_at_ms IS NULL
             ORDER BY r.input_watermark_seq, r.rendered_at_ms, r.item_id
             LIMIT ?1",
        )?;
        let rows =
            statement.query_map(params![i64::from(limit)], |row| row.get::<_, Vec<u8>>(0))?;
        let mut items = Vec::new();
        for row in rows {
            items.push(item_id_from_column("render_state", &row?)?);
        }
        Ok(items)
    }

    /// Absolute send instants of messages touched by events in
    /// `(after_seq, through_seq]`, de-duplicated and ordered.
    pub fn affected_message_instants(
        &self,
        chat: &ChatKey,
        after_seq: i64,
        through_seq: i64,
    ) -> Result<Vec<i64>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT m.sent_at_ms
             FROM message_events e
             JOIN messages m
               ON m.account_id = e.account_id
              AND m.namespace_version = e.namespace_version
              AND m.chat_id = e.chat_id
              AND m.message_id = e.message_id
             WHERE e.account_id = ?1 AND e.namespace_version = ?2 AND e.chat_id = ?3
               AND e.event_seq > ?4 AND e.event_seq <= ?5",
        )?;
        let rows = statement.query_map(
            params![
                account_id,
                namespace,
                chat.chat_id.0,
                after_seq,
                through_seq
            ],
            |row| row.get(0),
        )?;
        let instants = rows.collect::<Result<BTreeSet<_>, _>>()?;
        Ok(instants.into_iter().collect())
    }

    /// All known message send instants for a chat, ordered and de-duplicated.
    pub fn message_instants(&self, chat: &ChatKey) -> Result<Vec<i64>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT DISTINCT sent_at_ms FROM messages
             WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
             ORDER BY sent_at_ms",
        )?;
        let rows = statement.query_map(params![account_id, namespace, chat.chat_id.0], |row| {
            row.get(0)
        })?;
        rows.collect::<Result<_, _>>().map_err(StateError::from)
    }

    /// Pins one read snapshot and returns the retained normalized event input
    /// for a single UTC range corresponding to a local civil month.
    pub fn month_render_snapshot(
        &self,
        chat: ChatKey,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<MonthRenderSnapshot, StateError> {
        if start_ms >= end_ms {
            return Err(StateError::InvalidArgument {
                what: "render month start must precede end",
            });
        }
        let account = self
            .account(chat.scope.account)?
            .ok_or(StateError::RowNotFound { entity: "account" })?;
        let render_generation = self
            .render_generation(chat.scope.account)?
            .ok_or(StateError::RowNotFound { entity: "account" })?;
        let input_watermark_seq = self.latest_event_seq(&chat)?;
        let (account_id, namespace) = scope_columns(&chat.scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT m.message_id, m.sender_id, m.sent_at_ms,
                    e.event_seq, e.event_kind, e.observed_at_ms,
                    e.payload_schema, e.payload
             FROM messages m
             JOIN message_events e
               ON e.account_id = m.account_id
              AND e.namespace_version = m.namespace_version
              AND e.chat_id = m.chat_id
              AND e.message_id = m.message_id
             WHERE m.account_id = ?1 AND m.namespace_version = ?2 AND m.chat_id = ?3
               AND m.sent_at_ms >= ?4 AND m.sent_at_ms < ?5
               AND e.event_seq <= ?6
             ORDER BY m.sent_at_ms, m.message_id, e.event_seq",
        )?;
        type RawRow = (
            i64,
            Option<i64>,
            i64,
            i64,
            String,
            i64,
            Option<i64>,
            Option<Vec<u8>>,
        );
        let rows = statement.query_map(
            params![
                account_id,
                namespace,
                chat.chat_id.0,
                start_ms,
                end_ms,
                input_watermark_seq
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;
        let mut messages: Vec<RenderMessageInput> = Vec::new();
        for row in rows {
            let (
                message_id,
                sender_id,
                sent_at_ms,
                event_seq,
                event_kind,
                observed_at_ms,
                payload_schema,
                payload,
            ): RawRow = row?;
            if messages
                .last()
                .is_none_or(|message| message.message_id.0 != message_id)
            {
                messages.push(RenderMessageInput {
                    message_id: MessageId(message_id),
                    sender_id,
                    sent_at_ms,
                    events: Vec::new(),
                });
            }
            let payload = match (payload_schema, payload) {
                (None, None) => None,
                (Some(schema), Some(bytes)) => {
                    let schema = u16::try_from(schema).map_err(|_| StateError::CorruptRow {
                        table: "message_events",
                        detail: format!("payload_schema {schema} does not fit u16"),
                    })?;
                    Some(MessagePayload {
                        schema: SchemaFamily(schema),
                        bytes,
                    })
                }
                _ => {
                    return Err(StateError::CorruptRow {
                        table: "message_events",
                        detail: "payload and payload_schema must be present together".to_owned(),
                    });
                }
            };
            let message = messages.last_mut().ok_or(StateError::CorruptRow {
                table: "message_events",
                detail: "event row did not create a message group".to_owned(),
            })?;
            message.events.push(RenderEventInput {
                event_seq,
                kind: MessageEventKind::parse(&event_kind)?,
                observed_at_ms,
                payload,
            });
        }
        Ok(MonthRenderSnapshot {
            chat,
            input_watermark_seq,
            render_generation,
            display_timezone: account.display_timezone,
            retention_mode: account.retention_mode,
            start_ms,
            end_ms,
            messages,
        })
    }

    /// Live generated-document appearances for one direct monthly partition.
    pub fn month_render_catalog(
        &self,
        chat: ChatKey,
        year: u16,
        month: u8,
    ) -> Result<Vec<RenderCatalogEntry>, StateError> {
        let month_item =
            ItemKey::Canonical(CanonicalKey::MonthDir(MonthDirKey { chat, year, month })).id();
        let mut statement = self.conn().prepare_cached(
            "SELECT child.item_id
             FROM items AS month
             JOIN items AS child ON child.parent_item_id = month.item_id
             WHERE month.canonical_item_id = ?1
               AND month.kind = 'month_dir' AND month.deleted_at_ms IS NULL
               AND child.kind = 'generated_doc' AND child.deleted_at_ms IS NULL",
        )?;
        let rows = statement.query_map(params![month_item.as_bytes()], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        let mut catalog = Vec::new();
        for bytes in rows {
            let item = item_id_from_column("items", &bytes?)?;
            let ItemKey::Appearance(AppearanceKey {
                view,
                item: CanonicalKey::GeneratedDoc(document),
            }) = item.key()
            else {
                continue;
            };
            if document.chat != chat
                || document.partition != (DocPartition::Month { year, month })
                || !matches!(document.format, DocFormat::Markdown | DocFormat::Ndjson)
            {
                continue;
            }
            catalog.push(RenderCatalogEntry {
                item,
                view,
                format: document.format,
                schema_family: document.schema_family,
            });
        }
        catalog.sort_by(|left, right| left.item.as_bytes().cmp(right.item.as_bytes()));
        Ok(catalog)
    }

    /// Live `.chat.json` appearances for one chat, in stable item order.
    pub fn chat_render_catalog(
        &self,
        chat: ChatKey,
    ) -> Result<Vec<RenderCatalogEntry>, StateError> {
        let chat_item = ItemKey::Canonical(CanonicalKey::Chat(chat)).id();
        let mut statement = self.conn().prepare_cached(
            "SELECT child.item_id
             FROM items AS chat
             JOIN items AS child ON child.parent_item_id = chat.item_id
             WHERE chat.canonical_item_id = ?1
               AND chat.kind = 'chat' AND chat.deleted_at_ms IS NULL
               AND child.kind = 'generated_doc' AND child.deleted_at_ms IS NULL",
        )?;
        let rows = statement.query_map(params![chat_item.as_bytes()], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        let mut catalog = Vec::new();
        for bytes in rows {
            let item = item_id_from_column("items", &bytes?)?;
            let ItemKey::Appearance(AppearanceKey {
                view,
                item: CanonicalKey::GeneratedDoc(document),
            }) = item.key()
            else {
                continue;
            };
            if document.chat == chat
                && document.partition == DocPartition::Chat
                && document.format == DocFormat::Json
            {
                catalog.push(RenderCatalogEntry {
                    item,
                    view,
                    format: document.format,
                    schema_family: document.schema_family,
                });
            }
        }
        // The joined index traversal is deliberately unordered. Keep the
        // account-scan implementation's observable ItemId ordering.
        catalog.sort_by(|left, right| left.item.as_bytes().cmp(right.item.as_bytes()));
        Ok(catalog)
    }

    /// Every live generated-document appearance belonging to one chat,
    /// independent of civil partition.
    ///
    /// Chat-protection transitions use this account-scoped scan to revoke
    /// already-published Markdown/NDJSON ownership without relying on a
    /// possibly stale month planner.
    pub fn generated_document_items_of_chat(
        &self,
        chat: &ChatKey,
    ) -> Result<Vec<ItemId>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT item_id FROM items
             WHERE account_id = ?1 AND namespace_version = ?2
               AND kind = 'generated_doc' AND deleted_at_ms IS NULL
             ORDER BY item_id",
        )?;
        let rows = statement.query_map(params![account_id, namespace], |row| {
            row.get::<_, Vec<u8>>(0)
        })?;
        let mut items = Vec::new();
        for bytes in rows {
            let item = item_id_from_column("items", &bytes?)?;
            let belongs = matches!(
                item.key(),
                ItemKey::Appearance(AppearanceKey {
                    item: CanonicalKey::GeneratedDoc(document),
                    ..
                }) if document.chat == *chat
            );
            if belongs {
                items.push(item);
            }
        }
        Ok(items)
    }
}

impl WriteTxn<'_> {
    /// Ensures a document has render state at the given renderer and schema
    /// versions.
    ///
    /// A new document starts dirty with watermark zero. An existing one is
    /// untouched when the versions match; a version change marks it dirty —
    /// a renderer upgrade re-renders its documents (SYNC-030), without
    /// discarding the published facts until the re-render publishes.
    ///
    /// The generated-document item must already be projected
    /// ([`WriteTxn::upsert_item`]).
    pub fn ensure_render_state(
        &self,
        item: &ItemId,
        renderer_version: u32,
        schema_version: u32,
    ) -> Result<(), StateError> {
        self.conn()
            .prepare_cached(
                "INSERT INTO render_state (item_id, renderer_version, schema_version)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT (item_id) DO UPDATE SET
                     renderer_version = excluded.renderer_version,
                     schema_version = excluded.schema_version,
                     dirty = 1, skip_reason = NULL, skipped_at_ms = NULL
                 WHERE render_state.renderer_version <> excluded.renderer_version
                    OR render_state.schema_version <> excluded.schema_version",
            )?
            .execute(params![
                item.as_bytes(),
                i64::from(renderer_version),
                i64::from(schema_version),
            ])?;
        Ok(())
    }

    /// Puts a document on the re-render worklist (SYNC-024) — the change
    /// appliers' half of the watermark protocol.
    pub fn mark_render_dirty(&self, item: &ItemId) -> Result<(), StateError> {
        let changed = self
            .conn()
            .prepare_cached(
                "UPDATE render_state
                 SET dirty = 1, skip_reason = NULL, skipped_at_ms = NULL
                 WHERE item_id = ?1",
            )?
            .execute(params![item.as_bytes()])?;
        if changed == 0 {
            return Err(StateError::RowNotFound {
                entity: "render state",
            });
        }
        Ok(())
    }

    /// Revokes published render facts after an authoritative restriction.
    ///
    /// Provider item metadata and cache ownership are updated by the caller in
    /// the same transaction. Clearing the render hash/version here guarantees
    /// that a relaunch cannot treat the previously published bytes as current;
    /// Protected documents stay off the worklist. A later authoritative
    /// protection removal must explicitly mark them dirty before republishing.
    pub fn skip_render_due_to_policy(
        &self,
        item: &ItemId,
        skipped_at_ms: i64,
    ) -> Result<bool, StateError> {
        let changed = self
            .conn()
            .prepare_cached(
                "UPDATE render_state
                 SET content_version = NULL, content_hash_algo = NULL, content_hash = NULL,
                     logical_size = NULL, dirty = 0, skip_reason = ?2, skipped_at_ms = ?3,
                     rendered_at_ms = NULL
                 WHERE item_id = ?1",
            )?
            .execute(params![
                item.as_bytes(),
                RenderSkipReason::PolicyExcluded.as_str(),
                skipped_at_ms,
            ])?;
        Ok(changed > 0)
    }

    /// Publishes rendered output for a document whose inputs come from
    /// `chat`'s event log, at watermark `watermark_seq` — see the module
    /// docs for the race this closes.
    ///
    /// The document's provider-visible content facts live on its item row
    /// and move under this same transaction via
    /// [`WriteTxn::update_item_content`]; this call owns only the render
    /// bookkeeping.
    pub fn publish_render(
        &self,
        item: &ItemId,
        chat: &ChatKey,
        watermark_seq: i64,
        output: &RenderOutput,
        rendered_at_ms: i64,
    ) -> Result<RenderPublish, StateError> {
        self.publish_render_checked(item, chat, watermark_seq, None, output, rendered_at_ms)
    }

    /// Publishes a generated document whose inputs are metadata rather than
    /// the message event log.
    ///
    /// Metadata publication has no event-watermark race. Its caller rechecks
    /// the canonical metadata snapshot in the same transaction, then clears
    /// the dirty bit and records the exact output facts here.
    pub fn publish_static_render(
        &self,
        item: &ItemId,
        output: &RenderOutput,
        rendered_at_ms: i64,
    ) -> Result<(), StateError> {
        if self.read().render_state(item)?.is_none() {
            return Err(StateError::RowNotFound {
                entity: "render state",
            });
        }
        let (algo, hash_bytes) = match &output.content_hash {
            Some(hash) => {
                let (algo, bytes) = hash_columns(hash);
                (Some(algo), Some(bytes))
            }
            None => (None, None),
        };
        self.conn()
            .prepare_cached(
                "UPDATE render_state
                 SET input_watermark_seq = 0, content_version = ?2, content_hash_algo = ?3,
                     content_hash = ?4, logical_size = ?5, dirty = 0, skip_reason = NULL,
                     skipped_at_ms = NULL, rendered_at_ms = ?6
                 WHERE item_id = ?1",
            )?
            .execute(params![
                item.as_bytes(),
                output.content_version.as_str(),
                algo,
                hash_bytes,
                size_to_column(output.logical_size)?,
                rendered_at_ms,
            ])?;
        Ok(())
    }

    /// Publishes one monthly document and keeps it dirty only when a newer
    /// event affects the same UTC month range. Events in unrelated months do
    /// not cause needless rebuilds.
    pub fn publish_month_render(
        &self,
        item: &ItemId,
        chat: &ChatKey,
        watermark_seq: i64,
        month_range: std::ops::Range<i64>,
        output: &RenderOutput,
        rendered_at_ms: i64,
    ) -> Result<RenderPublish, StateError> {
        if month_range.start >= month_range.end {
            return Err(StateError::InvalidArgument {
                what: "render month start must precede end",
            });
        }
        self.publish_render_checked(
            item,
            chat,
            watermark_seq,
            Some((month_range.start, month_range.end)),
            output,
            rendered_at_ms,
        )
    }

    fn publish_render_checked(
        &self,
        item: &ItemId,
        chat: &ChatKey,
        watermark_seq: i64,
        month_range: Option<(i64, i64)>,
        output: &RenderOutput,
        rendered_at_ms: i64,
    ) -> Result<RenderPublish, StateError> {
        let current: Option<i64> = self
            .conn()
            .prepare_cached("SELECT input_watermark_seq FROM render_state WHERE item_id = ?1")?
            .query_row(params![item.as_bytes()], |row| row.get(0))
            .optional()?;
        let current = current.ok_or(StateError::RowNotFound {
            entity: "render state",
        })?;
        if watermark_seq < current {
            return Err(StateError::WatermarkRegression {
                current,
                proposed: watermark_seq,
            });
        }
        // The race check: did the chat's log grow past the watermark while
        // the renderer worked outside this transaction?
        let (account_id, namespace) = scope_columns(&chat.scope);
        let newer_events: bool = match month_range {
            None => self
                .conn()
                .prepare_cached(
                    "SELECT EXISTS (
                     SELECT 1 FROM message_events
                     WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
                       AND event_seq > ?4)",
                )?
                .query_row(
                    params![account_id, namespace, chat.chat_id.0, watermark_seq],
                    |row| row.get(0),
                )?,
            Some((start_ms, end_ms)) => self
                .conn()
                .prepare_cached(
                    "SELECT EXISTS (
                     SELECT 1
                     FROM message_events e
                     JOIN messages m
                       ON m.account_id = e.account_id
                      AND m.namespace_version = e.namespace_version
                      AND m.chat_id = e.chat_id
                      AND m.message_id = e.message_id
                     WHERE e.account_id = ?1 AND e.namespace_version = ?2
                       AND e.chat_id = ?3 AND e.event_seq > ?4
                       AND m.sent_at_ms >= ?5 AND m.sent_at_ms < ?6)",
                )?
                .query_row(
                    params![
                        account_id,
                        namespace,
                        chat.chat_id.0,
                        watermark_seq,
                        start_ms,
                        end_ms
                    ],
                    |row| row.get(0),
                )?,
        };
        let (algo, hash_bytes) = match &output.content_hash {
            Some(hash) => {
                let (algo, bytes) = hash_columns(hash);
                (Some(algo), Some(bytes))
            }
            None => (None, None),
        };
        self.conn()
            .prepare_cached(
                "UPDATE render_state
                 SET input_watermark_seq = ?2, content_version = ?3, content_hash_algo = ?4,
                     content_hash = ?5, logical_size = ?6, dirty = ?7, skip_reason = NULL,
                     skipped_at_ms = NULL, rendered_at_ms = ?8
                 WHERE item_id = ?1",
            )?
            .execute(params![
                item.as_bytes(),
                watermark_seq,
                output.content_version.as_str(),
                algo,
                hash_bytes,
                size_to_column(output.logical_size)?,
                newer_events,
                rendered_at_ms,
            ])?;
        Ok(RenderPublish {
            clean: !newer_events,
        })
    }
}
