//! The versioned, human-readable monthly Markdown renderer (SYNC-031,
//! SYNC-032, POL-3, POL-4).
//!
//! A chat's history for one calendar month rendered as a Markdown transcript:
//! a self-describing front-matter header, then the messages grouped by day,
//! with senders, wall-clock times in an explicit timezone, replies, edits,
//! reactions, service events, and links from attachments to their files under
//! `media/`. Where NDJSON ([`crate::ndjson`]) is the lossless record view, this
//! is the readable one — a *view*, never a source of truth (DOM-006), so a
//! rerun over unchanged records rewrites nothing.
//!
//! # Determinism
//!
//! Rendering is a pure function of the input records and the frozen schema and
//! renderer versions: identical input yields byte-identical output
//! (`.spec/quality-and-release.md`, NFR-011). Within a message, revisions are
//! sorted by [`Revision::event_seq`] before rendering, so shuffled input
//! renders identically; across messages, output follows input order (a
//! well-formed export supplies messages in ascending `(sent_at_ms,
//! message_id)` order, which the day grouping relies on).
//!
//! # Timezone (SYNC-031)
//!
//! All civil dates and times are computed in one caller-supplied [`UtcOffset`],
//! declared once in the header. The offset is a fixed number of seconds east of
//! UTC — a display setting the engine holds for the account, not a per-document
//! degree of freedom — so the renderer needs no timezone database and stays
//! dependency-free (POL-6). Like the retention mode, the offset is a rendering
//! configuration held constant per account and is not folded into
//! [`content_version_token`]; a configuration change is re-rendered by the
//! engine, exactly as a renderer-version bump is.
//!
//! # Injection safety (SYNC-031)
//!
//! Message text, file names, titles, and reaction emoji are untrusted. Every
//! such value is escaped so it cannot alter document structure: Markdown block
//! and inline syntax is backslash-escaped, HTML-significant characters become
//! entities, control characters are replaced, and attachment links are
//! percent-encoded (`text` module). No untrusted value reaches the output raw.
//!
//! # Retention modes (POL-3)
//!
//! [`RetentionMode`] selects the projection, mirroring the NDJSON renderer:
//! - **Mirror** renders current state — each live message's latest revision,
//!   deleted messages omitted.
//! - **Audit** additionally annotates edits (a note plus the earlier revisions'
//!   text) and renders a deletion as a content-preserving, marked tombstone.
//!
//! # Schema versioning
//!
//! [`SCHEMA_VERSION`] and [`RENDERER_VERSION`] are frozen for format v1; the
//! header declares both. A format change is a version bump plus a new golden
//! fixture, never a silent mutation of v1 — the same discipline the NDJSON
//! renderer, identity codec, and cursor formats follow.

mod render;
mod text;

pub use crate::record::{
    Attachment, Availability, Deletion, Entity, EntityKind, MediaKind, MessageBody, MessageHistory,
    Reaction, ReactionKey, RetentionMode, Revision, Sender, ServiceAction,
};

use std::fmt;

use gramdrive_model::identity::{
    CanonicalKey, ChatKey, DocFormat, DocPartition, GeneratedDocKey, ItemId, ItemKey, SchemaFamily,
};

/// Stable schema identifier written into every document header.
pub const SCHEMA_ID: &str = "gramdrive.transcript";

/// Format-schema version of the Markdown output, frozen for format v1
/// (SYNC-031).
pub const SCHEMA_VERSION: u32 = 1;

/// Version of this renderer implementation, frozen for v1 (DOM-006).
pub const RENDERER_VERSION: u32 = 1;

/// Schema family of the monthly Markdown document (DOM-023). Part of its stable
/// identity; the family lineage is per-format, so it is independent of the
/// NDJSON document's family even when the numbers coincide.
pub const MONTH_MARKDOWN_SCHEMA_FAMILY: SchemaFamily = SchemaFamily(1);

/// The largest offset magnitude a [`UtcOffset`] accepts: one whole day, so
/// every real-world zone (from `-12:00` to `+14:00`) is representable while a
/// nonsensical multi-day offset is rejected.
const MAX_OFFSET_SECONDS: i32 = 24 * 60 * 60;

/// A fixed display timezone: a whole number of seconds east of UTC.
///
/// Deliberately not a named IANA zone. A fixed offset is all a deterministic,
/// dependency-free renderer can honor without shipping a time-zone database
/// (POL-6), and it is exactly "timezone-explicit" in the SYNC-031 sense: the
/// header states the offset, and every timestamp in the document is computed in
/// it. Daylight-saving transitions within a rendered month are out of scope for
/// v1; the engine picks the offset in effect for the partition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UtcOffset {
    /// Seconds east of UTC. Invariant: `|seconds| <= MAX_OFFSET_SECONDS`.
    seconds: i32,
}

impl UtcOffset {
    /// Coordinated Universal Time — zero offset.
    pub const UTC: UtcOffset = UtcOffset { seconds: 0 };

    /// Builds an offset from seconds east of UTC (negative for west).
    ///
    /// Rejects any magnitude beyond a single day, the residual nonsense a
    /// caller could otherwise pass; every real zone is well within it.
    pub fn from_seconds(seconds: i32) -> Result<Self, InvalidUtcOffset> {
        if seconds.abs() <= MAX_OFFSET_SECONDS {
            Ok(Self { seconds })
        } else {
            Err(InvalidUtcOffset { seconds })
        }
    }

    /// Builds an offset from whole minutes east of UTC — Telegram's own
    /// timezone granularity.
    pub fn from_minutes(minutes: i32) -> Result<Self, InvalidUtcOffset> {
        let seconds = minutes
            .checked_mul(60)
            .ok_or(InvalidUtcOffset { seconds: minutes })?;
        Self::from_seconds(seconds)
    }

    /// Seconds east of UTC.
    pub fn seconds(self) -> i32 {
        self.seconds
    }

    /// The header label for this offset: `UTC`, or `UTC±HH:MM` (with `:SS`
    /// appended only when the offset carries a nonzero second component).
    pub(crate) fn label(self) -> String {
        if self.seconds == 0 {
            return "UTC".to_owned();
        }
        let sign = if self.seconds > 0 { '+' } else { '-' };
        let magnitude = self.seconds.unsigned_abs();
        let hours = magnitude / 3_600;
        let minutes = (magnitude % 3_600) / 60;
        let seconds = magnitude % 60;
        if seconds == 0 {
            format!("UTC{sign}{hours:02}:{minutes:02}")
        } else {
            format!("UTC{sign}{hours:02}:{minutes:02}:{seconds:02}")
        }
    }
}

/// Why a [`UtcOffset`] could not be built: the magnitude exceeds one day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUtcOffset {
    /// The rejected value (seconds for [`UtcOffset::from_seconds`], the
    /// unmultiplied minutes when [`UtcOffset::from_minutes`] overflowed).
    pub seconds: i32,
}

impl fmt::Display for InvalidUtcOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "utc offset {} is out of range (|offset| must be <= {MAX_OFFSET_SECONDS} seconds)",
            self.seconds
        )
    }
}

impl std::error::Error for InvalidUtcOffset {}

/// Everything the renderer needs to produce one monthly Markdown document.
///
/// Borrows its message slice so the engine can render a partition it already
/// holds without copying. Messages should arrive in ascending `(sent_at_ms,
/// message_id)` order; the day grouping depends on it (module docs).
#[derive(Debug, Clone, Copy)]
pub struct MarkdownInput<'a> {
    /// The chat the document is rendered from.
    pub chat: ChatKey,
    /// The source range this document covers — a month in normal use, though
    /// any [`DocPartition`] renders.
    pub partition: DocPartition,
    /// The account's retention mode, which selects the POL-3 projection.
    pub retention_mode: RetentionMode,
    /// The fixed display timezone for every date and time in the document.
    pub timezone: UtcOffset,
    /// The event-log watermark these records reflect: the document is current
    /// as of every event at or below this sequence (SYNC-024).
    pub input_watermark_seq: i64,
    /// The message histories to render, in canonical order.
    pub messages: &'a [MessageHistory],
}

/// The stable identity of the monthly Markdown document for a chat partition
/// (DOM-023). Independent of the chat title, renderer version, and watermark.
///
/// The engine uses this to key the document's `render_state` and item rows; the
/// header embeds its text form so a reader can join a rendered file back to the
/// item it projects.
pub fn document_id(chat: ChatKey, partition: DocPartition) -> ItemId {
    ItemKey::Canonical(CanonicalKey::GeneratedDoc(GeneratedDocKey {
        chat,
        partition,
        format: DocFormat::Markdown,
        schema_family: MONTH_MARKDOWN_SCHEMA_FAMILY,
    }))
    .id()
}

/// The composite content-version token for a document rendered at
/// `input_watermark_seq` (DOM-006): the renderer and schema versions plus the
/// watermark. Two renders that agree on all three — at a fixed retention mode
/// and timezone (see the module docs) — produce byte-identical bytes, so equal
/// tokens mean equal content.
///
/// Returned as text; the engine wraps it in
/// `gramdrive_model::version::ContentVersion` when it publishes.
pub fn content_version_token(input_watermark_seq: i64) -> String {
    format!("{SCHEMA_ID}/s{SCHEMA_VERSION}/r{RENDERER_VERSION}/w{input_watermark_seq}")
}

/// Renders a complete monthly Markdown document to a string.
///
/// A convenience over [`write_transcript`]; the streaming form is preferred for
/// large histories. Byte-identical to what [`write_transcript`] streams for the
/// same input.
pub fn render_transcript(input: &MarkdownInput<'_>) -> String {
    let mut out = String::new();
    // A `String` sink's `fmt::Write` never returns `Err` — the `Result` is
    // `Ok` by construction, so discarding it drops no real error.
    let _ = write_transcript(&mut out, input);
    out
}

/// Streams a complete monthly Markdown document to `out`, one block at a time.
///
/// The front-matter header is written first, then the title, then the messages
/// grouped by day. Blocks are built into a small reused buffer, so memory stays
/// bounded by the largest single block rather than the document size (the
/// story's bounded-output criterion). Propagates any error the sink returns.
pub fn write_transcript<W: fmt::Write>(out: &mut W, input: &MarkdownInput<'_>) -> fmt::Result {
    render::write_document(out, input)
}
