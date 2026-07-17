//! Line builders: structured records in, deterministic JSON lines out.
//!
//! Every function here builds one NDJSON line into a caller-owned scratch
//! buffer and hands it back through a closure, so a whole document streams
//! through a single reused `String` — memory stays bounded by the largest
//! single record, not by the chat's history length (the story's bounded-output
//! criterion). Field order is fixed by construction ([`crate::json::Json`]);
//! nothing here sorts or maps.

use std::fmt;

use gramdrive_model::identity::{
    AttachmentIndex, AttachmentKey, CanonicalKey, ChatKey, ContentHash, DocPartition, ItemId,
    ItemKey, MessageId, MessageKey,
};

use crate::json::Json;
use crate::ndjson::{
    MESSAGES_SCHEMA_FAMILY, MessagesInput, RENDERER_VERSION, SCHEMA_ID, SCHEMA_VERSION,
    content_version_token, document_id,
};
use crate::record::{
    Attachment, Entity, EntityKind, MediaKind, MessageHistory, Reaction, ReactionKey,
    RetentionMode, Sender, ServiceAction,
};

/// The disposition of a rendered message record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordDisposition {
    /// The current, live revision of a message.
    Present,
    /// A superseded prior revision (Audit mode only).
    Superseded,
    /// A content-preserving deletion tombstone (Audit mode only).
    Deleted {
        /// When the deletion was observed (ms since the Unix epoch).
        observed_at_ms: i64,
    },
}

impl RecordDisposition {
    fn tag(self) -> &'static str {
        match self {
            RecordDisposition::Present => "present",
            RecordDisposition::Superseded => "superseded",
            RecordDisposition::Deleted { .. } => "deleted",
        }
    }

    fn deleted_ms(self) -> Option<i64> {
        match self {
            RecordDisposition::Deleted { observed_at_ms } => Some(observed_at_ms),
            RecordDisposition::Present | RecordDisposition::Superseded => None,
        }
    }
}

/// Builds the document header line into `buf` (cleared first).
///
/// The header carries the schema/renderer/provenance metadata (SYNC-030,
/// DOM-006): schema id and version, renderer version, schema family, the
/// generated-document id, the chat scope, the partition, the retention mode,
/// the input watermark, and the composite content-version token.
pub(super) fn header_line(buf: &mut String, input: &MessagesInput<'_>) {
    buf.clear();
    let scope = input.chat.scope;
    let header = Json::Object(vec![
        ("type", Json::str("header")),
        ("schema", Json::str(SCHEMA_ID)),
        ("schema_version", Json::U64(u64::from(SCHEMA_VERSION))),
        ("renderer_version", Json::U64(u64::from(RENDERER_VERSION))),
        (
            "schema_family",
            Json::U64(u64::from(MESSAGES_SCHEMA_FAMILY.0)),
        ),
        (
            "document_id",
            Json::owned(document_id(input.chat, input.partition).text()),
        ),
        ("account_id", Json::I64(scope.account.account_id.0)),
        (
            "namespace_version",
            Json::U64(u64::from(scope.namespace_version.0)),
        ),
        ("chat_id", Json::I64(input.chat.chat_id.0)),
        ("partition", partition_json(input.partition)),
        ("retention_mode", Json::str(input.retention_mode.tag())),
        ("input_watermark_seq", Json::I64(input.input_watermark_seq)),
        (
            "content_version",
            Json::owned(content_version_token(input.input_watermark_seq)),
        ),
    ]);
    header.write(buf);
}

fn partition_json<'a>(partition: DocPartition) -> Json<'a> {
    match partition {
        DocPartition::Chat => Json::Object(vec![("kind", Json::str("chat"))]),
        DocPartition::Year { year } => Json::Object(vec![
            ("kind", Json::str("year")),
            ("year", Json::U64(u64::from(year))),
        ]),
        DocPartition::Month { year, month } => Json::Object(vec![
            ("kind", Json::str("month")),
            ("year", Json::U64(u64::from(year))),
            ("month", Json::U64(u64::from(month))),
        ]),
    }
}

/// Emits the message records for one message's history under `mode`, calling
/// `emit` once per line with the built JSON (no trailing newline).
///
/// POL-3 projection:
/// - **Mirror:** a deleted message emits nothing (content and view purged); a
///   live message emits exactly its latest revision, `state: present`.
/// - **Audit:** every revision emits in `event_seq` order — earlier revisions
///   `state: superseded`, the latest `state: present`, or, if a deletion was
///   observed, the latest as `state: deleted` with the deletion timestamp and
///   its last-known content preserved.
///
/// A history with no revisions is malformed and emits nothing.
pub(super) fn message_lines<E>(
    buf: &mut String,
    chat: ChatKey,
    mode: RetentionMode,
    message: &MessageHistory,
    emit: &mut E,
) -> fmt::Result
where
    E: FnMut(&str) -> fmt::Result,
{
    // Total, input-order-independent ordering over revisions: event_seq is
    // unique within a chat and never reused.
    let mut order: Vec<usize> = (0..message.revisions.len()).collect();
    order.sort_by_key(|&index| message.revisions[index].event_seq);
    let count = order.len();
    if count == 0 {
        return Ok(());
    }

    match mode {
        RetentionMode::Mirror => {
            if message.deletion.is_some() {
                // Deleted: content and rendered view purged (POL-3).
                return Ok(());
            }
            let Some(&last) = order.last() else {
                return Ok(());
            };
            build_message(
                buf,
                chat,
                message,
                count - 1,
                last,
                RecordDisposition::Present,
            );
            emit(buf.as_str())?;
        }
        RetentionMode::Audit => {
            for (ordinal, &index) in order.iter().enumerate() {
                let is_last = ordinal + 1 == count;
                let disposition = match (is_last, &message.deletion) {
                    (true, Some(deletion)) => RecordDisposition::Deleted {
                        observed_at_ms: deletion.observed_at_ms,
                    },
                    (true, None) => RecordDisposition::Present,
                    (false, _) => RecordDisposition::Superseded,
                };
                build_message(buf, chat, message, ordinal, index, disposition);
                emit(buf.as_str())?;
            }
        }
    }
    Ok(())
}

fn build_message(
    buf: &mut String,
    chat: ChatKey,
    message: &MessageHistory,
    ordinal: usize,
    revision_index: usize,
    disposition: RecordDisposition,
) {
    buf.clear();
    let revision = &message.revisions[revision_index];
    let body = &revision.body;
    let record = Json::Object(vec![
        ("type", Json::str("message")),
        ("message_id", Json::I64(message.message_id.0)),
        ("state", Json::str(disposition.tag())),
        ("revision", Json::U64(ordinal as u64)),
        ("sender", sender_json(message.sender)),
        ("date_ms", Json::I64(message.sent_at_ms)),
        ("edited_ms", opt_i64(revision.edited_at_ms)),
        ("observed_ms", Json::I64(revision.observed_at_ms)),
        ("text", opt_str(body.text.as_deref())),
        ("entities", entities_json(&body.entities)),
        ("reply_to_message_id", opt_message_id(body.reply_to)),
        ("thread_top_message_id", opt_message_id(body.thread_top)),
        ("topic_id", opt_i64(body.topic_id)),
        ("album_id", opt_i64(body.album_id)),
        ("reactions", reactions_json(&body.reactions)),
        (
            "attachments",
            attachments_json(chat, message.message_id, &body.attachments),
        ),
        ("service", service_json(body.service.as_ref())),
        ("protected", Json::Bool(body.protected)),
        ("deleted_ms", opt_i64(disposition.deleted_ms())),
        (
            "provenance",
            Json::Object(vec![
                (
                    "schema_family",
                    Json::U64(u64::from(revision.payload_schema.0)),
                ),
                ("event_seq", Json::I64(revision.event_seq)),
            ]),
        ),
    ]);
    record.write(buf);
}

fn sender_json<'a>(sender: Option<Sender>) -> Json<'a> {
    match sender {
        Some(sender) => Json::Object(vec![("id", Json::I64(sender.id))]),
        None => Json::Null,
    }
}

fn opt_i64<'a>(value: Option<i64>) -> Json<'a> {
    match value {
        Some(value) => Json::I64(value),
        None => Json::Null,
    }
}

fn opt_message_id<'a>(value: Option<MessageId>) -> Json<'a> {
    match value {
        Some(id) => Json::I64(id.0),
        None => Json::Null,
    }
}

fn opt_str(value: Option<&str>) -> Json<'_> {
    match value {
        Some(value) => Json::str(value),
        None => Json::Null,
    }
}

fn opt_u64<'a>(value: Option<u64>) -> Json<'a> {
    match value {
        Some(value) => Json::U64(value),
        None => Json::Null,
    }
}

fn entities_json(entities: &[Entity]) -> Json<'_> {
    Json::Array(entities.iter().map(entity_json).collect())
}

fn entity_json(entity: &Entity) -> Json<'_> {
    let mut fields = vec![
        ("kind", Json::str(entity.kind.tag())),
        ("offset", Json::U64(u64::from(entity.offset))),
        ("length", Json::U64(u64::from(entity.length))),
    ];
    match &entity.kind {
        EntityKind::Pre { language } => {
            fields.push(("language", opt_str(language.as_deref())));
        }
        EntityKind::TextLink { url } => {
            fields.push(("url", Json::str(url)));
        }
        EntityKind::TextMention { user_id } => {
            fields.push(("user_id", Json::I64(*user_id)));
        }
        EntityKind::CustomEmoji { document_id } => {
            fields.push(("document_id", Json::I64(*document_id)));
        }
        EntityKind::Other { kind } => {
            fields.push(("raw_kind", Json::str(kind)));
        }
        _ => {}
    }
    Json::Object(fields)
}

fn reactions_json(reactions: &[Reaction]) -> Json<'_> {
    Json::Array(reactions.iter().map(reaction_json).collect())
}

fn reaction_json(reaction: &Reaction) -> Json<'_> {
    let key = match &reaction.key {
        ReactionKey::Emoji(emoji) => ("emoji", Json::str(emoji)),
        ReactionKey::Custom(document_id) => ("custom_emoji_id", Json::I64(*document_id)),
    };
    Json::Object(vec![
        key,
        ("count", Json::U64(u64::from(reaction.count))),
        ("chosen", Json::Bool(reaction.chosen)),
    ])
}

fn attachments_json(chat: ChatKey, message_id: MessageId, attachments: &[Attachment]) -> Json<'_> {
    Json::Array(
        attachments
            .iter()
            .map(|attachment| attachment_json(chat, message_id, attachment))
            .collect(),
    )
}

fn attachment_json<'a>(
    chat: ChatKey,
    message_id: MessageId,
    attachment: &'a Attachment,
) -> Json<'a> {
    let item_id = attachment_item_id(chat, message_id, attachment.index).text();
    let mut fields = vec![
        ("index", Json::U64(u64::from(attachment.index.0))),
        ("item_id", Json::owned(item_id)),
        ("media_kind", Json::str(attachment.media_kind.tag())),
    ];
    if let MediaKind::Other { kind } = &attachment.media_kind {
        fields.push(("media_kind_raw", Json::str(kind)));
    }
    fields.push(("name", opt_str(attachment.name.as_deref())));
    fields.push(("mime_type", opt_str(attachment.mime_type.as_deref())));
    fields.push(("size", opt_u64(attachment.size)));
    fields.push(("availability", Json::str(attachment.availability.tag())));
    fields.push(("content", content_json(attachment.content_hash.as_ref())));
    Json::Object(fields)
}

fn content_json<'a>(hash: Option<&ContentHash>) -> Json<'a> {
    match hash {
        Some(ContentHash::Sha256(digest)) => Json::Object(vec![
            ("hash_algo", Json::str("sha256")),
            ("hash_hex", Json::owned(hex_lower(digest))),
        ]),
        None => Json::Null,
    }
}

fn service_json<'a>(service: Option<&'a ServiceAction>) -> Json<'a> {
    let Some(action) = service else {
        return Json::Null;
    };
    let mut fields = vec![("action", Json::str(action.tag()))];
    match action {
        ServiceAction::ChatCreated { title } | ServiceAction::ChatTitleChanged { title } => {
            fields.push(("title", Json::str(title)));
        }
        ServiceAction::MembersAdded { user_ids } => {
            fields.push((
                "user_ids",
                Json::Array(user_ids.iter().map(|&id| Json::I64(id)).collect()),
            ));
        }
        ServiceAction::MemberRemoved { user_id } => {
            fields.push(("user_id", Json::I64(*user_id)));
        }
        ServiceAction::MessagePinned { message_id } => {
            fields.push(("message_id", Json::I64(message_id.0)));
        }
        ServiceAction::AutoDeleteTimerChanged { seconds } => {
            fields.push(("seconds", Json::I64(*seconds)));
        }
        ServiceAction::Other { kind } => {
            fields.push(("raw_action", Json::str(kind)));
        }
    }
    Json::Object(fields)
}

fn attachment_item_id(chat: ChatKey, message_id: MessageId, index: AttachmentIndex) -> ItemId {
    ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
        message: MessageKey { chat, message_id },
        index,
    }))
    .id()
}

/// Lowercase hex of a byte digest, deterministic and allocation-once.
fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}
