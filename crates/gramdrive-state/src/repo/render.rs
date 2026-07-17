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

use gramdrive_model::identity::{ChatKey, ContentHash, ItemId};
use gramdrive_model::version::ContentVersion;
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{
    ReadTxn, WriteTxn, hash_columns, hash_from_columns, item_id_from_column, scope_columns,
    size_from_column, size_to_column,
};

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
    /// When the document was last rendered (ms since the Unix epoch).
    pub rendered_at_ms: Option<i64>,
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
        rendered_at_ms: row.get(9)?,
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
        rendered_at_ms: raw.rendered_at_ms,
    })
}

const RENDER_COLUMNS: &str = "item_id, renderer_version, schema_version, input_watermark_seq,
     content_version, content_hash_algo, content_hash, logical_size, dirty, rendered_at_ms";

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

    /// The re-render worklist: every dirty document, from the covering
    /// partial index (SYNC-024).
    pub fn dirty_render_items(&self, limit: u32) -> Result<Vec<ItemId>, StateError> {
        let mut statement = self
            .conn()
            .prepare_cached("SELECT item_id FROM render_state WHERE dirty = 1 LIMIT ?1")?;
        let rows =
            statement.query_map(params![i64::from(limit)], |row| row.get::<_, Vec<u8>>(0))?;
        let mut items = Vec::new();
        for row in rows {
            items.push(item_id_from_column("render_state", &row?)?);
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
                     dirty = 1
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
            .prepare_cached("UPDATE render_state SET dirty = 1 WHERE item_id = ?1")?
            .execute(params![item.as_bytes()])?;
        if changed == 0 {
            return Err(StateError::RowNotFound {
                entity: "render state",
            });
        }
        Ok(())
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
        let newer_events: bool = self
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
            )?;
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
                     content_hash = ?5, logical_size = ?6, dirty = ?7, rendered_at_ms = ?8
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
