//! Block builders: structured records in, deterministic Markdown out.
//!
//! The document is a sequence of blocks separated by one blank line and
//! terminated by a single newline. Every block is built into a caller-owned
//! scratch buffer and streamed immediately, so a whole document flows through a
//! small reused `String` — memory stays bounded by the largest single block,
//! not the month's length (the story's bounded-output criterion). Every
//! untrusted value is routed through the `text` escapers; nothing here writes
//! message text, a file name, a title, or a reaction emoji raw.

use std::fmt;

use gramdrive_model::identity::DocPartition;

use crate::markdown::text::{self, Civil};
use crate::markdown::{
    MONTH_MARKDOWN_SCHEMA_FAMILY, MarkdownInput, RENDERER_VERSION, SCHEMA_ID, SCHEMA_VERSION,
    content_version_token, document_id,
};
use crate::record::{
    Attachment, Availability, MediaKind, MessageBody, MessageHistory, Reaction, ReactionKey,
    RetentionMode, Revision, Sender, ServiceAction,
};

/// A blank-line-separated block writer over an arbitrary sink.
///
/// Emits `\n\n` before every block but the first and a single trailing `\n`
/// once anything was written, so the block boundaries are exact and stable
/// regardless of which blocks a given message produces.
struct Blocks<'sink, W: fmt::Write> {
    sink: &'sink mut W,
    started: bool,
}

impl<'sink, W: fmt::Write> Blocks<'sink, W> {
    fn new(sink: &'sink mut W) -> Self {
        Self {
            sink,
            started: false,
        }
    }

    /// Writes one block, prefixed with a blank-line separator unless it is the
    /// first.
    fn emit(&mut self, block: &str) -> fmt::Result {
        if self.started {
            self.sink.write_str("\n\n")?;
        }
        self.started = true;
        self.sink.write_str(block)
    }

    /// Terminates the document with a single newline if any block was written.
    fn finish(self) -> fmt::Result {
        if self.started {
            self.sink.write_str("\n")?;
        }
        Ok(())
    }
}

/// Streams the whole document: front matter, title, subtitle, then the messages
/// grouped by civil day in the input's timezone.
pub(super) fn write_document<W: fmt::Write>(
    sink: &mut W,
    input: &MarkdownInput<'_>,
) -> fmt::Result {
    let mut blocks = Blocks::new(sink);
    let mut buf = String::new();

    front_matter(&mut buf, input);
    blocks.emit(&buf)?;

    title(&mut buf, input);
    blocks.emit(&buf)?;

    subtitle(&mut buf, input);
    blocks.emit(&buf)?;

    let offset = input.timezone.seconds();
    let mut current_day: Option<String> = None;
    for message in input.messages {
        if !message_is_rendered(input.retention_mode, message) {
            continue;
        }
        let day = Civil::from_millis(message.sent_at_ms, offset).date();
        if current_day.as_deref() != Some(day.as_str()) {
            buf.clear();
            buf.push_str("## ");
            buf.push_str(&day);
            blocks.emit(&buf)?;
            current_day = Some(day);
        }
        render_message(&mut blocks, &mut buf, input, message)?;
    }

    blocks.finish()
}

/// A message is rendered unless it is malformed (no revisions) or purged by the
/// retention policy (a deletion in Mirror mode, POL-3).
fn message_is_rendered(mode: RetentionMode, message: &MessageHistory) -> bool {
    if message.revisions.is_empty() {
        return false;
    }
    match mode {
        RetentionMode::Mirror => message.deletion.is_none(),
        RetentionMode::Audit => true,
    }
}

/// Builds the YAML front-matter header into `buf` (cleared first).
///
/// Self-describing provenance (DOM-006, SYNC-031): schema and renderer
/// versions, schema family, the generated-document id, the chat scope, the
/// partition, the retention mode, the explicit timezone, the input watermark,
/// and the composite content-version token. Every value is renderer-controlled,
/// so none is escaped and none can carry a `\n` or a `---` that would break the
/// block.
fn front_matter(buf: &mut String, input: &MarkdownInput<'_>) {
    buf.clear();
    let scope = input.chat.scope;
    buf.push_str("---\n");
    fm(buf, "schema", SCHEMA_ID);
    fm(buf, "schema_version", &SCHEMA_VERSION.to_string());
    fm(buf, "renderer_version", &RENDERER_VERSION.to_string());
    fm(
        buf,
        "schema_family",
        &MONTH_MARKDOWN_SCHEMA_FAMILY.0.to_string(),
    );
    fm(
        buf,
        "document_id",
        &document_id(input.chat, input.partition).text(),
    );
    fm(buf, "account_id", &scope.account.account_id.0.to_string());
    fm(
        buf,
        "namespace_version",
        &scope.namespace_version.0.to_string(),
    );
    fm(buf, "chat_id", &input.chat.chat_id.0.to_string());
    fm(buf, "partition", &partition_label(input.partition));
    fm(buf, "retention_mode", input.retention_mode.tag());
    fm(buf, "timezone", &input.timezone.label());
    fm(
        buf,
        "input_watermark_seq",
        &input.input_watermark_seq.to_string(),
    );
    fm(
        buf,
        "content_version",
        &content_version_token(input.input_watermark_seq),
    );
    buf.push_str("---");
}

/// Appends one `key: value` front-matter line.
fn fm(buf: &mut String, key: &str, value: &str) {
    buf.push_str(key);
    buf.push_str(": ");
    buf.push_str(value);
    buf.push('\n');
}

/// The document title: `# Chat <id>`. The chat id, not the title, identifies
/// the document — the renderer is title-independent by construction (DOM-023),
/// so a rename never changes these bytes.
fn title(buf: &mut String, input: &MarkdownInput<'_>) {
    buf.clear();
    buf.push_str("# Chat ");
    buf.push_str(&input.chat.chat_id.0.to_string());
}

/// The human context line under the title: what range, in which timezone, at
/// which retention.
fn subtitle(buf: &mut String, input: &MarkdownInput<'_>) {
    buf.clear();
    buf.push('_');
    buf.push_str(&partition_phrase(input.partition));
    buf.push_str(" · times in ");
    buf.push_str(&input.timezone.label());
    buf.push_str(" · retention: ");
    buf.push_str(input.retention_mode.tag());
    buf.push_str("._");
}

/// Machine-facing partition token for the front matter.
fn partition_label(partition: DocPartition) -> String {
    match partition {
        DocPartition::Chat => "chat".to_owned(),
        DocPartition::Year { year } => format!("{year:04}"),
        DocPartition::Month { year, month } => format!("{year:04}-{month:02}"),
    }
}

/// Human-facing partition phrase for the subtitle.
fn partition_phrase(partition: DocPartition) -> String {
    match partition {
        DocPartition::Chat => "Whole-chat transcript".to_owned(),
        DocPartition::Year { year } => format!("Transcript for {year:04}"),
        DocPartition::Month { year, month } => format!("Transcript for {year:04}-{month:02}"),
    }
}

/// Renders one message as a run of blocks: header, optional relationship line,
/// content (service note, text, protected note, attachments, reactions), and —
/// in Audit mode — a deletion note and the earlier-revision history (POL-3).
fn render_message<W: fmt::Write>(
    blocks: &mut Blocks<'_, W>,
    buf: &mut String,
    input: &MarkdownInput<'_>,
    message: &MessageHistory,
) -> fmt::Result {
    let offset = input.timezone.seconds();
    let audit = matches!(input.retention_mode, RetentionMode::Audit);

    // Total, input-order-independent ordering over revisions: event_seq is
    // unique within a chat and never reused. The last is the current one.
    let mut order: Vec<usize> = (0..message.revisions.len()).collect();
    order.sort_by_key(|&index| message.revisions[index].event_seq);
    let Some(&last) = order.last() else {
        return Ok(());
    };
    let display = &message.revisions[last];

    header_line(buf, input, message, display);
    blocks.emit(buf)?;

    if relationship_line(buf, &display.body) {
        blocks.emit(buf)?;
    }

    if let Some(service) = &display.body.service {
        buf.clear();
        buf.push('_');
        buf.push_str(&service_phrase(service));
        buf.push_str("._");
        blocks.emit(buf)?;
    }

    if let Some(body_text) = nonempty(display.body.text.as_deref()) {
        buf.clear();
        buf.push_str(&text::escape_paragraph(body_text));
        blocks.emit(buf)?;
    }

    if display.body.protected {
        blocks.emit(
            "_Protected content: Telegram forbids saving; media is not fetched (POL\\-4)._",
        )?;
    }

    if !display.body.attachments.is_empty() {
        attachments_block(buf, &display.body.attachments);
        blocks.emit(buf)?;
    }

    if !display.body.reactions.is_empty() {
        reactions_block(buf, &display.body.reactions);
        blocks.emit(buf)?;
    }

    if audit {
        if let Some(deletion) = message.deletion {
            buf.clear();
            buf.push_str("_Deleted ");
            buf.push_str(&stamp(deletion.observed_at_ms, message.sent_at_ms, offset));
            buf.push_str("._");
            blocks.emit(buf)?;
        }
        if order.len() > 1 {
            blocks.emit("_Earlier revisions:_")?;
            earlier_revisions(buf, message, &order, offset);
            blocks.emit(buf)?;
        }
    }

    Ok(())
}

/// Builds the bold message header: time, sender, message id, and — depending on
/// the display revision and mode — `edited`/`deleted` markers.
fn header_line(
    buf: &mut String,
    input: &MarkdownInput<'_>,
    message: &MessageHistory,
    display: &Revision,
) {
    let offset = input.timezone.seconds();
    buf.clear();
    buf.push_str("**");
    buf.push_str(&Civil::from_millis(message.sent_at_ms, offset).time());
    buf.push_str(" · ");
    buf.push_str(&sender_label(message.sender));
    buf.push_str(" · #");
    buf.push_str(&message.message_id.0.to_string());
    buf.push_str("**");
    if let Some(edited_ms) = display.edited_at_ms {
        buf.push_str(" · edited ");
        buf.push_str(&stamp(edited_ms, message.sent_at_ms, offset));
    }
    // A deletion is only rendered in Audit mode (Mirror purges the message).
    if matches!(input.retention_mode, RetentionMode::Audit) && message.deletion.is_some() {
        buf.push_str(" · deleted");
    }
}

/// The sender label. The render contract carries only a numeric id (no user
/// directory lives in this crate); a missing sender is a channel post or
/// anonymous admin (SYNC-034).
fn sender_label(sender: Option<Sender>) -> String {
    match sender {
        Some(sender) => format!("user {}", sender.id),
        None => "unknown sender".to_owned(),
    }
}

/// Builds the italic relationship line (reply/thread/topic/album) into `buf`,
/// returning `false` when the message has no relationships and no block should
/// be emitted. All values are numeric ids, hence safe unescaped.
fn relationship_line(buf: &mut String, body: &MessageBody) -> bool {
    let mut parts: Vec<String> = Vec::new();
    if let Some(reply_to) = body.reply_to {
        parts.push(format!("in reply to #{}", reply_to.0));
    }
    if let Some(thread_top) = body.thread_top
        && body.reply_to != Some(thread_top)
    {
        parts.push(format!("thread #{}", thread_top.0));
    }
    if let Some(topic_id) = body.topic_id {
        parts.push(format!("topic {topic_id}"));
    }
    if let Some(album_id) = body.album_id {
        parts.push(format!("album {album_id}"));
    }
    if parts.is_empty() {
        return false;
    }
    buf.clear();
    buf.push('_');
    buf.push_str(&parts.join(" · "));
    buf.push('_');
    true
}

/// Builds the attachments list into `buf`. One tight-list item per attachment,
/// in index order: media kind, a link into `media/` when a file exists, the
/// size, and an explicit availability note for anything not downloaded (POL-4,
/// SYNC-032).
fn attachments_block(buf: &mut String, attachments: &[Attachment]) {
    buf.clear();
    let mut first = true;
    for attachment in attachments {
        if !first {
            buf.push('\n');
        }
        first = false;
        buf.push_str("- **");
        buf.push_str(&media_kind_label(&attachment.media_kind));
        buf.push_str("** — ");
        let link_text = match nonempty(attachment.name.as_deref()) {
            Some(name) => text_escaped(name),
            None => "(unnamed)".to_owned(),
        };
        match nonempty(attachment.media_name.as_deref()) {
            Some(media_name) => {
                buf.push('[');
                buf.push_str(&link_text);
                buf.push_str("](media/");
                text::percent_encode_component(media_name, buf);
                buf.push(')');
            }
            None => buf.push_str(&link_text),
        }
        if let Some(size) = attachment.size {
            buf.push_str(&format!(" ({size} bytes)"));
        }
        buf.push_str(availability_note(
            attachment.availability,
            attachment.content_hash.is_some(),
        ));
    }
}

/// The trailing availability note for an attachment, empty when a downloaded,
/// fetchable file needs none.
fn availability_note(availability: Availability, downloaded: bool) -> &'static str {
    match availability {
        Availability::Fetchable if downloaded => "",
        Availability::Fetchable => " — _not downloaded yet_",
        Availability::Restricted => " — _restricted by Telegram; not fetched_",
        Availability::Unavailable => " — _unavailable_",
        Availability::ViewOnce => " — _view-once; not stored_",
    }
}

/// The media-kind label, preserving the raw tag of an unknown kind (escaped).
fn media_kind_label(kind: &MediaKind) -> String {
    match kind {
        MediaKind::Other { kind: raw } => format!("other ({})", text_escaped(raw)),
        known => known.tag().to_owned(),
    }
}

/// Builds the reactions line into `buf`: `Reactions: <emoji or custom> ×<n>`
/// entries, `(you)` marking the account's own reaction.
fn reactions_block(buf: &mut String, reactions: &[Reaction]) {
    buf.clear();
    buf.push_str("Reactions:");
    for reaction in reactions {
        buf.push(' ');
        match &reaction.key {
            ReactionKey::Emoji(emoji) => buf.push_str(&text_escaped(emoji)),
            ReactionKey::Custom(document_id) => {
                buf.push_str(&format!("custom emoji {document_id}"));
            }
        }
        buf.push_str(&format!(" ×{}", reaction.count));
        if reaction.chosen {
            buf.push_str(" (you)");
        }
        buf.push_str(" ·");
    }
    // Drop the trailing " ·" separator the loop leaves.
    if buf.ends_with(" ·") {
        buf.truncate(buf.len() - " ·".len());
    }
}

/// Builds the Audit-mode earlier-revisions list into `buf`: every revision but
/// the current one, in event_seq order, each flattened to a single line.
fn earlier_revisions(buf: &mut String, message: &MessageHistory, order: &[usize], offset: i32) {
    buf.clear();
    let prior = &order[..order.len() - 1];
    let mut first = true;
    for &index in prior {
        if !first {
            buf.push('\n');
        }
        first = false;
        let revision = &message.revisions[index];
        let when = revision.edited_at_ms.unwrap_or(message.sent_at_ms);
        buf.push_str("- ");
        buf.push_str(&stamp(when, message.sent_at_ms, offset));
        buf.push_str(": ");
        match nonempty(revision.body.text.as_deref()) {
            Some(body_text) => buf.push_str(&text::escape_flattened(body_text)),
            None => buf.push_str("_(no text)_"),
        }
    }
}

/// A localized timestamp: the time alone when it falls on the message's own
/// civil day, otherwise the full date and time — enough to disambiguate an edit
/// or deletion that landed on a later day, without repeating the date needlessly.
fn stamp(instant_ms: i64, message_sent_ms: i64, offset: i32) -> String {
    let civil = Civil::from_millis(instant_ms, offset);
    let base = Civil::from_millis(message_sent_ms, offset);
    if (civil.year, civil.month, civil.day) == (base.year, base.month, base.day) {
        civil.time()
    } else {
        civil.date_time()
    }
}

/// A human sentence for a service action, with every untrusted value escaped.
fn service_phrase(service: &ServiceAction) -> String {
    match service {
        ServiceAction::ChatCreated { title } => format!("Group created: “{}”", text_escaped(title)),
        ServiceAction::ChatTitleChanged { title } => {
            format!("Renamed to “{}”", text_escaped(title))
        }
        ServiceAction::MembersAdded { user_ids } => {
            if user_ids.is_empty() {
                "Members added".to_owned()
            } else {
                format!("Added {}", join_ids(user_ids))
            }
        }
        ServiceAction::MemberRemoved { user_id } => format!("Removed user {user_id}"),
        ServiceAction::MessagePinned { message_id } => {
            format!("Pinned message #{}", message_id.0)
        }
        ServiceAction::AutoDeleteTimerChanged { seconds } => {
            if *seconds == 0 {
                "Auto-delete timer disabled".to_owned()
            } else {
                format!("Auto-delete timer set to {seconds} s")
            }
        }
        ServiceAction::Other { kind } => format!("Service event: {}", text_escaped(kind)),
    }
}

/// Joins numeric user ids with `, ` — safe unescaped.
fn join_ids(ids: &[i64]) -> String {
    let mut out = String::new();
    for (position, id) in ids.iter().enumerate() {
        if position > 0 {
            out.push_str(", ");
        }
        out.push_str(&id.to_string());
    }
    out
}

/// `Some(text)` when the option holds a non-empty string, else `None` — an
/// empty caption is treated as no caption.
fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|text| !text.is_empty())
}

/// Inline-escapes `value` into a fresh string.
fn text_escaped(value: &str) -> String {
    let mut out = String::new();
    text::escape_inline(value, &mut out);
    out
}
