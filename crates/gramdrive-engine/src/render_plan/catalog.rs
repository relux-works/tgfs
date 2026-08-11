//! The generated-document catalog: the fixed set of documents the core renders
//! for every chat, each carrying the partitioning granularity and the frozen
//! renderer/schema versions of its format.
//!
//! This is the single list that answers "what documents does a chat produce"
//! (DOM-006, DOM-023; the tree layout in
//! `.spec/sync-and-filesystem-semantics.md`). The planner reads every version
//! and identity through it, so the numbers it compares against `render_state`
//! and the tokens it stamps into a plan come from exactly the constants the
//! renderers publish — there is no second copy to drift.

use gramdrive_model::identity::{
    CanonicalKey, ChatKey, DocFormat, DocPartition, GeneratedDocKey, ItemId, ItemKey, SchemaFamily,
};
use gramdrive_render::chat_json::{ChatKind, ChatMetadataInput};
use gramdrive_render::{chat_json, markdown, ndjson};
use gramdrive_state::repo::{ChatRecord, ChatType, RetentionMode as StateRetentionMode};

/// A class of generated document the core renders for every chat.
///
/// - [`DocClass::ChatJson`] — privacy-bounded whole-chat metadata.
/// - [`DocClass::NdjsonMonth`] — bounded lossless monthly `Messages.ndjson`.
/// - [`DocClass::MarkdownMonth`] — the human-readable monthly transcript
///   (`YYYY-MM/Messages.md`, `DocPartition::Month`), paired with NDJSON so a
///   change regenerates only the month it fell in (SYNC-024, SYNC-031).
///
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocClass {
    /// The whole-chat metadata document.
    ChatJson,
    /// The bounded lossless monthly `Messages.ndjson`.
    NdjsonMonth,
    /// The human-readable monthly Markdown transcript.
    MarkdownMonth,
}

/// Every document class the planner renders, in a stable order.
pub const DOCUMENT_CLASSES: [DocClass; 3] = [
    DocClass::ChatJson,
    DocClass::MarkdownMonth,
    DocClass::NdjsonMonth,
];

impl DocClass {
    /// The output format of this class.
    pub fn format(self) -> DocFormat {
        match self {
            DocClass::ChatJson => DocFormat::Json,
            DocClass::NdjsonMonth => DocFormat::Ndjson,
            DocClass::MarkdownMonth => DocFormat::Markdown,
        }
    }

    /// The record-schema family of this class's documents (DOM-023) — part of
    /// their stable identity.
    pub fn schema_family(self) -> SchemaFamily {
        match self {
            DocClass::ChatJson => chat_json::CHAT_SCHEMA_FAMILY,
            DocClass::NdjsonMonth => ndjson::MESSAGES_SCHEMA_FAMILY,
            DocClass::MarkdownMonth => markdown::MONTH_MARKDOWN_SCHEMA_FAMILY,
        }
    }

    /// The frozen renderer-implementation version of this class's format
    /// (SYNC-030) — a bump re-renders every document of the class.
    pub fn renderer_version(self) -> u32 {
        match self {
            DocClass::ChatJson => chat_json::RENDERER_VERSION,
            DocClass::NdjsonMonth => ndjson::RENDERER_VERSION,
            DocClass::MarkdownMonth => markdown::RENDERER_VERSION,
        }
    }

    /// The frozen record-schema version of this class's format (DOM-023).
    pub fn schema_version(self) -> u32 {
        match self {
            DocClass::ChatJson => chat_json::SCHEMA_VERSION,
            DocClass::NdjsonMonth => ndjson::SCHEMA_VERSION,
            DocClass::MarkdownMonth => markdown::SCHEMA_VERSION,
        }
    }

    /// The content-version token a document of this class carries once rendered
    /// at the pinned message/policy provenance (DOM-006), produced by the
    /// renderer that owns the format so plan and publication agree exactly.
    pub fn content_version_token(
        self,
        watermark_seq: i64,
        render_generation: i64,
        retention_mode: StateRetentionMode,
        display_timezone: &str,
        chat: Option<&ChatRecord>,
    ) -> Option<String> {
        let retention_mode = match retention_mode {
            StateRetentionMode::Mirror => markdown::RetentionMode::Mirror,
            StateRetentionMode::Audit => markdown::RetentionMode::Audit,
        };
        match self {
            DocClass::ChatJson => {
                let chat = chat?;
                let kind = match chat.chat_type {
                    ChatType::Private => ChatKind::Private,
                    ChatType::Group => ChatKind::Group,
                    ChatType::Supergroup => ChatKind::Supergroup,
                    ChatType::Channel => ChatKind::Channel,
                };
                let bytes = chat_json::render(&ChatMetadataInput {
                    kind,
                    title: &chat.title,
                    username: chat.username.as_deref(),
                    is_protected: chat.is_protected,
                    archive_mode: chat.archive_mode,
                    left_at_ms: chat.left_at_ms,
                    deleted_at_ms: chat.deleted_at_ms,
                    last_update_at_ms: chat.last_update_at_ms,
                });
                Some(chat_json::content_version_token(bytes.as_bytes()))
            }
            DocClass::NdjsonMonth => Some(ndjson::content_version_token(
                watermark_seq,
                render_generation,
                retention_mode,
                display_timezone,
            )),
            DocClass::MarkdownMonth => Some(markdown::content_version_token(
                watermark_seq,
                render_generation,
                retention_mode,
                display_timezone,
            )),
        }
    }

    /// The generated-document identity of this class at `partition`.
    ///
    /// Live message formats pair with [`DocPartition::Month`]; `.chat.json`
    /// pairs with [`DocPartition::Chat`]. Identity is title-,
    /// renderer-version- and watermark-independent (DOM-023).
    pub fn document_key(self, chat: ChatKey, partition: DocPartition) -> GeneratedDocKey {
        GeneratedDocKey {
            chat,
            partition,
            format: self.format(),
            schema_family: self.schema_family(),
        }
    }

    /// The opaque canonical identity of this logical document at `partition`.
    /// Provider-visible tree positions and `render_state` rows are appearance
    /// ids; this canonical id groups their shared render job.
    pub fn document_id(self, chat: ChatKey, partition: DocPartition) -> ItemId {
        ItemKey::Canonical(CanonicalKey::GeneratedDoc(
            self.document_key(chat, partition),
        ))
        .id()
    }

    /// The class of a decoded generated-document key, or `None` when this
    /// planner does not render it (a format/family it has no renderer for, such
    /// as `.chat.json`). Matched by format and schema family, the two
    /// identity facets that select a renderer.
    pub fn for_key(key: &GeneratedDocKey) -> Option<DocClass> {
        DOCUMENT_CLASSES.into_iter().find(|class| {
            let partition_matches = match class {
                DocClass::ChatJson => key.partition == DocPartition::Chat,
                DocClass::NdjsonMonth | DocClass::MarkdownMonth => {
                    matches!(key.partition, DocPartition::Month { .. })
                }
            };
            partition_matches
                && class.format() == key.format
                && class.schema_family() == key.schema_family
        })
    }
}
