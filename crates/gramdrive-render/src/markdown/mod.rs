//! The versioned, human-readable monthly Markdown renderer (SYNC-031,
//! SYNC-032, POL-3, POL-4).
//!
//! A chat's history for one calendar month rendered as a Markdown transcript:
//! a self-describing front-matter header, then the messages grouped by day,
//! with senders, wall-clock times in an explicit timezone, replies, edits,
//! reactions, service events, and links from attachments to their files under
//! the same direct `YYYY-MM/` namespace. Where NDJSON ([`crate::ndjson`]) is the lossless record view, this
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
//! All civil dates and times are computed in one caller-supplied
//! [`DisplayTimeZone`], declared once in the header. Production callers resolve
//! the account's persisted IANA zone, including historical offset transitions;
//! [`UtcOffset`] remains available for fixed-zone fixtures. Like the retention
//! mode, the timezone and the account's monotonic render-policy generation are
//! folded into [`content_version_token`]. A policy-only transition therefore
//! cannot reuse a message-watermark version for different bytes.
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
//! [`SCHEMA_VERSION`] and [`RENDERER_VERSION`] are frozen per format; the
//! header declares both. A format change is a version bump plus a new golden
//! fixture, never a silent mutation of a published version — the same discipline the NDJSON
//! renderer, identity codec, and cursor formats follow.

mod render;
mod text;

pub use crate::record::{
    Attachment, AttachmentFidelity, Availability, Deletion, Entity, EntityKind, MediaKind,
    MessageBody, MessageHistory, Reaction, ReactionKey, RetentionMode, Revision, Sender,
    ServiceAction, TelegramRepresentation,
};

use std::fmt;

use gramdrive_model::identity::{
    CanonicalKey, ChatKey, DocFormat, DocPartition, GeneratedDocKey, ItemId, ItemKey, SchemaFamily,
};
use jiff::{
    civil::Date,
    tz::{Offset, TimeZone},
};

/// Stable schema identifier written into every document header.
pub const SCHEMA_ID: &str = "gramdrive.transcript";

/// Format-schema version of the Markdown output; v2 uses direct month links
/// (SYNC-031).
pub const SCHEMA_VERSION: u32 = 2;

/// Version of this renderer implementation (DOM-006).
pub const RENDERER_VERSION: u32 = 4;

/// Schema family of the monthly Markdown document (DOM-023). Part of its stable
/// identity; the family lineage is per-format, so it is independent of the
/// NDJSON document's family even when the numbers coincide.
pub const MONTH_MARKDOWN_SCHEMA_FAMILY: SchemaFamily = SchemaFamily(1);

/// The largest offset magnitude a [`UtcOffset`] accepts: one whole day, so
/// every real-world zone (from `-12:00` to `+14:00`) is representable while a
/// nonsensical multi-day offset is rejected.
const MAX_OFFSET_SECONDS: i32 = 24 * 60 * 60;

/// A fixed timezone helper: a whole number of seconds east of UTC.
///
/// Production rendering uses [`DisplayTimeZone::named`]. This type is retained
/// for explicit UTC/fixed-offset fixtures and callers whose persisted setting
/// is itself a fixed offset.
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

/// Persisted account display timezone used for civil partitions and text.
#[derive(Debug, Clone)]
pub struct DisplayTimeZone {
    label: String,
    timezone: TimeZone,
}

impl DisplayTimeZone {
    /// Resolves a persisted IANA zone name such as `Asia/Tbilisi`.
    pub fn named(name: &str) -> Result<Self, InvalidDisplayTimeZone> {
        let timezone = TimeZone::get(name).map_err(|error| InvalidDisplayTimeZone {
            name: name.to_owned(),
            detail: error.to_string(),
        })?;
        Ok(Self {
            label: name.to_owned(),
            timezone,
        })
    }

    /// Builds a fixed-offset timezone for deterministic fixtures.
    pub fn fixed(offset: UtcOffset) -> Self {
        Self {
            label: offset.label(),
            timezone: TimeZone::fixed(
                Offset::from_seconds(offset.seconds()).unwrap_or(Offset::UTC),
            ),
        }
    }

    /// Stable zone name written into generated-document headers.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// IANA/fixed transition rules shared with the planner.
    pub fn timezone(&self) -> &TimeZone {
        &self.timezone
    }

    /// UTC millisecond bounds of one local civil month.
    pub fn month_bounds_ms(
        &self,
        year: u16,
        month: u8,
    ) -> Result<(i64, i64), InvalidDisplayTimeZone> {
        let year = i16::try_from(year).map_err(|_| InvalidDisplayTimeZone {
            name: self.label.clone(),
            detail: format!("year {year} is outside the supported civil range"),
        })?;
        let month = i8::try_from(month).map_err(|_| InvalidDisplayTimeZone {
            name: self.label.clone(),
            detail: format!("month {month} is outside the supported civil range"),
        })?;
        let (next_year, next_month) = if month == 12 {
            (year.saturating_add(1), 1)
        } else {
            (year, month.saturating_add(1))
        };
        let start = Date::new(year, month, 1)
            .and_then(|date| date.to_zoned(self.timezone.clone()))
            .map_err(|error| InvalidDisplayTimeZone {
                name: self.label.clone(),
                detail: error.to_string(),
            })?;
        let end = Date::new(next_year, next_month, 1)
            .and_then(|date| date.to_zoned(self.timezone.clone()))
            .map_err(|error| InvalidDisplayTimeZone {
                name: self.label.clone(),
                detail: error.to_string(),
            })?;
        Ok((
            start.timestamp().as_millisecond(),
            end.timestamp().as_millisecond(),
        ))
    }
}

impl PartialEq for DisplayTimeZone {
    fn eq(&self, other: &Self) -> bool {
        self.label == other.label
    }
}

impl Eq for DisplayTimeZone {}

/// A persisted display timezone could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDisplayTimeZone {
    /// Rejected persisted name.
    pub name: String,
    /// Resolver detail suitable for diagnostics.
    pub detail: String,
}

impl fmt::Display for InvalidDisplayTimeZone {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "display timezone '{}' could not be resolved: {}",
            self.name, self.detail
        )
    }
}

impl std::error::Error for InvalidDisplayTimeZone {}

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
    pub timezone: &'a DisplayTimeZone,
    /// The event-log watermark these records reflect: the document is current
    /// as of every event at or below this sequence (SYNC-024).
    pub input_watermark_seq: i64,
    /// Monotonic account policy generation pinned with the input snapshot.
    pub render_generation: i64,
    /// The message histories to render, in canonical order.
    pub messages: &'a [MessageHistory],
}

/// The stable identity of the monthly Markdown document for a chat partition
/// (DOM-023). Independent of the chat title, renderer version, and watermark.
///
/// Provider-visible `render_state` and item rows use appearance ids. This
/// canonical id is embedded in the header as the stable logical document key
/// shared by those appearances.
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
/// `input_watermark_seq` (DOM-006): renderer/schema versions, watermark, and
/// every byte-shaping account policy input. Equal tokens therefore witness the
/// same retention projection, timezone, and monotonic policy generation.
///
/// Returned as text; the engine wraps it in
/// `gramdrive_model::version::ContentVersion` when it publishes.
pub fn content_version_token(
    input_watermark_seq: i64,
    render_generation: i64,
    retention_mode: RetentionMode,
    display_timezone: &str,
) -> String {
    format!(
        "{SCHEMA_ID}/s{SCHEMA_VERSION}/r{RENDERER_VERSION}/w{input_watermark_seq}/g{render_generation}/retention-{}/tz-{}",
        retention_mode.tag(),
        display_timezone
    )
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
    if !crate::record::attachment_contract_is_valid(input.messages) {
        return Err(fmt::Error);
    }
    render::write_document(out, input)
}
