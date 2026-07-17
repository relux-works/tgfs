//! The versioned, lossless `messages.ndjson` renderer (SYNC-030, SYNC-032,
//! POL-3, POL-4).
//!
//! Newline-delimited JSON: a header line, then one line per emitted message
//! record. Rendering is a pure function of the input records and the schema and
//! renderer versions frozen here — identical input yields byte-identical output
//! (`.spec/quality-and-release.md`, NFR-011), so a sync that changed nothing
//! rewrites nothing. The full schema is documented in this crate's README.
//!
//! # What the caller supplies
//!
//! The renderer depends on `gramdrive-model` only and never touches the state
//! store (crate layering, `crates/README.md`). The engine reads a chat's
//! messages, events, and attachments from the state repositories up to a render
//! watermark, builds the [`MessageHistory`] records, and calls
//! [`render_messages`] (or streams with [`write_messages`]). The watermark it
//! rendered from is [`MessagesInput::input_watermark_seq`], recorded in the
//! header and folded into the content version — the same watermark
//! `gramdrive-state`'s `publish_render` re-checks under the publishing
//! transaction to close the render/append race (SYNC-024).
//!
//! # Retention modes (POL-3)
//!
//! [`RetentionMode`] selects the projection:
//! - **Mirror** renders the archive's current state: the latest revision of
//!   each live message, deleted messages omitted.
//! - **Audit** renders everything observed: every revision, and a
//!   content-preserving tombstone per observed deletion.
//!
//! # Determinism and ordering
//!
//! Within a message, revisions render in `event_seq` order regardless of input
//! order. Across messages, the renderer emits in the order given: a well-formed
//! export supplies messages in ascending `(sent_at_ms, message_id)` order,
//! which the state layer's time-windowed queries already return.
//!
//! # Schema versioning (schema migration)
//!
//! [`SCHEMA_VERSION`] and [`RENDERER_VERSION`] are frozen for format v1; the
//! header declares both. A schema change is a version bump plus a new golden
//! fixture, never a silent mutation of the v1 output — a reader keyed on
//! `schema_version` migrates deterministically, exactly as the durable
//! identity and cursor formats evolve elsewhere in the core.

mod record;
mod render;

pub use record::{
    Attachment, Availability, Deletion, Entity, EntityKind, MediaKind, MessageBody, MessageHistory,
    Reaction, ReactionKey, RetentionMode, Revision, Sender, ServiceAction,
};

use std::fmt;

use gramdrive_model::identity::{
    CanonicalKey, ChatKey, DocFormat, DocPartition, GeneratedDocKey, ItemId, ItemKey, SchemaFamily,
};

/// Stable schema identifier written into every document header.
pub const SCHEMA_ID: &str = "gramdrive.messages";

/// Record-schema version of the NDJSON output, frozen for format v1 (SYNC-030).
pub const SCHEMA_VERSION: u32 = 1;

/// Version of this renderer implementation, frozen for v1 (DOM-006).
pub const RENDERER_VERSION: u32 = 1;

/// Schema family of the `messages.ndjson` generated document (DOM-023). Part of
/// its stable identity; distinct from the family of any other generated doc.
pub const MESSAGES_SCHEMA_FAMILY: SchemaFamily = SchemaFamily(1);

/// Everything the renderer needs to produce one `messages.ndjson` document.
///
/// Borrows its message slice so the engine can render a page it already holds
/// without copying. Messages should arrive in ascending
/// `(sent_at_ms, message_id)` order; see the module docs.
#[derive(Debug, Clone, Copy)]
pub struct MessagesInput<'a> {
    /// The chat the document is rendered from.
    pub chat: ChatKey,
    /// The source range this document covers (whole chat, year, or month).
    pub partition: DocPartition,
    /// The account's retention mode, which selects the POL-3 projection.
    pub retention_mode: RetentionMode,
    /// The event-log watermark these records reflect: the document is current
    /// as of every event at or below this sequence (SYNC-024).
    pub input_watermark_seq: i64,
    /// The message histories to render, in canonical order.
    pub messages: &'a [MessageHistory],
}

/// The stable identity of the `messages.ndjson` document for a chat partition
/// (DOM-023). Independent of the chat title, renderer version, and watermark.
///
/// The engine uses this to key the document's `render_state` and item rows; the
/// header embeds its text form so a reader can join a rendered file back to the
/// item it projects.
pub fn document_id(chat: ChatKey, partition: DocPartition) -> ItemId {
    ItemKey::Canonical(CanonicalKey::GeneratedDoc(GeneratedDocKey {
        chat,
        partition,
        format: DocFormat::Ndjson,
        schema_family: MESSAGES_SCHEMA_FAMILY,
    }))
    .id()
}

/// The composite content-version token for a document rendered at
/// `input_watermark_seq` (DOM-006): the renderer and schema versions plus the
/// watermark. Two renders that agree on all three produce byte-identical bytes,
/// so equal tokens mean equal content.
///
/// Returned as text; the engine wraps it in `gramdrive_model::version::ContentVersion`
/// when it publishes. The token is always a valid version token (a fixed ASCII
/// prefix and decimal integers), so no fallible construction happens here.
pub fn content_version_token(input_watermark_seq: i64) -> String {
    format!("{SCHEMA_ID}/s{SCHEMA_VERSION}/r{RENDERER_VERSION}/w{input_watermark_seq}")
}

/// Renders a complete `messages.ndjson` document to a string.
///
/// A convenience over [`write_messages`]; the streaming form is preferred for
/// large histories. Byte-identical to what [`write_messages`] streams for the
/// same input.
pub fn render_messages(input: &MessagesInput<'_>) -> String {
    let mut out = String::new();
    // A `String` sink's `fmt::Write` never returns `Err` — the `Result` is
    // `Ok` by construction, so discarding it drops no real error.
    let _ = write_messages(&mut out, input);
    out
}

/// Streams a complete `messages.ndjson` document to `out`, one line at a time.
///
/// The header is written first, then the message records in input order. Each
/// line is built into a single reused buffer, so memory stays bounded by the
/// largest record rather than the document size. Propagates any error the sink
/// returns.
pub fn write_messages<W: fmt::Write>(out: &mut W, input: &MessagesInput<'_>) -> fmt::Result {
    let mut line = String::new();
    render::header_line(&mut line, input);
    out.write_str(&line)?;
    out.write_str("\n")?;

    let mut emit = |built: &str| -> fmt::Result {
        out.write_str(built)?;
        out.write_str("\n")
    };
    for message in input.messages {
        render::message_lines(
            &mut line,
            input.chat,
            input.retention_mode,
            message,
            &mut emit,
        )?;
    }
    Ok(())
}
