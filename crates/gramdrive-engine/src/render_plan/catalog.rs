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
use gramdrive_render::{markdown, ndjson};

/// A class of generated document the core renders for every chat.
///
/// - [`DocClass::Ndjson`] — the lossless whole-chat `messages.ndjson`, one file
///   per chat (`DocPartition::Chat`), so *any* change to the chat regenerates
///   it.
/// - [`DocClass::MarkdownMonth`] — the human-readable monthly transcript
///   (`YYYY/MM.md`, `DocPartition::Month`), one file per calendar month, so a
///   change regenerates only the month it fell in (SYNC-024, SYNC-031).
///
/// The chat-metadata `chat.json` ([`DocFormat::Json`]) is deliberately absent:
/// its renderer is a separate task, so this planner neither plans it nor treats
/// its render state as stale work — [`DocClass::for_key`] returns `None` for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocClass {
    /// The lossless whole-chat `messages.ndjson`.
    Ndjson,
    /// The human-readable monthly Markdown transcript.
    MarkdownMonth,
}

/// Every document class the planner renders, in a stable order.
pub const DOCUMENT_CLASSES: [DocClass; 2] = [DocClass::Ndjson, DocClass::MarkdownMonth];

impl DocClass {
    /// The output format of this class.
    pub fn format(self) -> DocFormat {
        match self {
            DocClass::Ndjson => DocFormat::Ndjson,
            DocClass::MarkdownMonth => DocFormat::Markdown,
        }
    }

    /// The record-schema family of this class's documents (DOM-023) — part of
    /// their stable identity.
    pub fn schema_family(self) -> SchemaFamily {
        match self {
            DocClass::Ndjson => ndjson::MESSAGES_SCHEMA_FAMILY,
            DocClass::MarkdownMonth => markdown::MONTH_MARKDOWN_SCHEMA_FAMILY,
        }
    }

    /// The frozen renderer-implementation version of this class's format
    /// (SYNC-030) — a bump re-renders every document of the class.
    pub fn renderer_version(self) -> u32 {
        match self {
            DocClass::Ndjson => ndjson::RENDERER_VERSION,
            DocClass::MarkdownMonth => markdown::RENDERER_VERSION,
        }
    }

    /// The frozen record-schema version of this class's format (DOM-023).
    pub fn schema_version(self) -> u32 {
        match self {
            DocClass::Ndjson => ndjson::SCHEMA_VERSION,
            DocClass::MarkdownMonth => markdown::SCHEMA_VERSION,
        }
    }

    /// The content-version token a document of this class carries once rendered
    /// at `watermark_seq` (DOM-006): the format's schema id, schema version,
    /// renderer version, and the input watermark, produced by the renderer that
    /// owns the format so the planned token matches the published one exactly.
    pub fn content_version_token(self, watermark_seq: i64) -> String {
        match self {
            DocClass::Ndjson => ndjson::content_version_token(watermark_seq),
            DocClass::MarkdownMonth => markdown::content_version_token(watermark_seq),
        }
    }

    /// The generated-document identity of this class at `partition`.
    ///
    /// Whole-chat NDJSON pairs with [`DocPartition::Chat`]; a monthly transcript
    /// with a [`DocPartition::Month`]. Identity is title-, renderer-version- and
    /// watermark-independent (DOM-023).
    pub fn document_key(self, chat: ChatKey, partition: DocPartition) -> GeneratedDocKey {
        GeneratedDocKey {
            chat,
            partition,
            format: self.format(),
            schema_family: self.schema_family(),
        }
    }

    /// The opaque item id of this class's document at `partition` — the key
    /// `render_state` and the item tree store, matching the renderer's own
    /// `document_id`.
    pub fn document_id(self, chat: ChatKey, partition: DocPartition) -> ItemId {
        ItemKey::Canonical(CanonicalKey::GeneratedDoc(
            self.document_key(chat, partition),
        ))
        .id()
    }

    /// The class of a decoded generated-document key, or `None` when this
    /// planner does not render it (a format/family it has no renderer for, such
    /// as a future `chat.json`). Matched by format and schema family, the two
    /// identity facets that select a renderer.
    pub fn for_key(key: &GeneratedDocKey) -> Option<DocClass> {
        DOCUMENT_CLASSES.into_iter().find(|class| {
            class.format() == key.format && class.schema_family() == key.schema_family
        })
    }
}
