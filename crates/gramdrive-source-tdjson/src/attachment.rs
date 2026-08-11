//! Attachment metadata mapping (TASK-260715-23arcu; PRD-030/PRD-032/PRD-033,
//! POL-4).
//!
//! # Where it sits
//!
//! [`crate::message`] normalizes one TDLib `message` into a [`MessageRecord`]
//! whose media content carries an [`AttachmentDescriptor`] — the source's
//! *raw* facts: kind, Telegram locators, original name, MIME, size, dimensions,
//! previews, and the POL-4 [`AttachmentAvailability`]. This module is the step
//! that turns those raw facts into the DOM § Attachment record a consumer keys
//! and names by: it binds the stable [`AttachmentKey`] identity and derives the
//! deterministic safe filename, keeping the raw descriptor intact alongside.
//! Like the normalizer, nothing here does I/O.
//!
//! # Identity and provenance (DOM-021, PRD-033)
//!
//! An attachment's identity is `(account scope, chat, message, ordinal)` — a
//! GramDrive key, never a Telegram locator, so a reference refresh (SYNC-045)
//! never changes it. v1 message content carries at most one attachment, at
//! ordinal zero; a Telegram *album* is many one-attachment messages sharing a
//! `media_album_id`, so its items are already distinct identities (distinct
//! message ids) that carry the same [`MappedAttachment::album_id`] as
//! provenance. The Telegram dedup key ([`AttachmentDescriptor::remote_unique_id`],
//! PRD-033) rides along in the descriptor; deduplicating stored bytes never
//! merges these distinct virtual items.
//!
//! # Names (PRD-032)
//!
//! A sender filename is preserved only for `messageDocument`, whose Telegram
//! representation is an original document even when its MIME logically denotes
//! an image, video, or audio file. Processed Telegram media never promotes its
//! transport filename to a source filename and instead gets a truthful
//! kind-and-MIME default (`photo.jpg`, `video.mp4`, `voice.ogg`, …). The mapped
//! leaf is sanitized by [`gramdrive_model::naming`]. Its account-timezone date
//! prefix is added by the projection layer, and direct-month sibling collisions
//! are settled by stable attachment identity through
//! [`gramdrive_model::naming::resolve_siblings`].
//!
//! # Capabilities (POL-4)
//!
//! [`MappedAttachment`] carries both the derived [`AttachmentAvailability`] (on
//! the descriptor) and Telegram's raw `can_be_saved` flag, because they are
//! distinct facts: a view-once attachment is unfetchable whatever `can_be_saved`
//! says. A restricted or view-once attachment already carries no preview bytes
//! or locators (the normalizer drops them, fail-closed); this layer never makes
//! an unfetchable attachment appear fetchable.

use gramdrive_model::identity::{
    AccountScope, AttachmentIndex, AttachmentKey, ChatId, ChatKey, MessageId, MessageKey,
};
use gramdrive_model::naming::{SafeName, attachment_leaf_name};

use crate::message::{
    AttachmentAvailability, AttachmentDescriptor, AttachmentKind, MessageContent, MessageRecord,
};

/// Logical media kind, independent from Telegram's message representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentLogicalKind {
    /// Still image bytes.
    Photo,
    /// Video bytes.
    Video,
    /// Looping animation bytes.
    Animation,
    /// Music or general audio bytes.
    Audio,
    /// Voice-note audio bytes.
    Voice,
    /// Round video-note bytes.
    VideoNote,
    /// Sticker bytes.
    Sticker,
    /// General document bytes.
    Document,
}

impl AttachmentLogicalKind {
    /// Stable state/render vocabulary token.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Photo => "photo",
            Self::Video => "video",
            Self::Animation => "animation",
            Self::Audio => "audio",
            Self::Voice => "voice",
            Self::VideoNote => "video_note",
            Self::Sticker => "sticker",
            Self::Document => "document",
        }
    }
}

/// Telegram message representation that exposed the attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramRepresentation {
    /// `messageDocument`, including image/video MIME documents.
    OriginalDocument,
    /// Telegram-processed `messagePhoto`.
    Photo,
    /// Telegram-processed `messageVideo`.
    Video,
    /// Telegram-processed `messageAnimation`.
    Animation,
    /// Telegram `messageAudio`.
    Audio,
    /// Telegram `messageVoiceNote`.
    Voice,
    /// Telegram `messageVideoNote`.
    VideoNote,
    /// Telegram `messageSticker`.
    Sticker,
}

impl TelegramRepresentation {
    /// Stable state/render vocabulary token.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::OriginalDocument => "original_document",
            Self::Photo => "message_photo",
            Self::Video => "message_video",
            Self::Animation => "message_animation",
            Self::Audio => "message_audio",
            Self::Voice => "message_voice",
            Self::VideoNote => "message_video_note",
            Self::Sticker => "message_sticker",
        }
    }
}

/// Truthful fidelity of the represented bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentFidelity {
    /// Exact original document bytes.
    Original,
    /// Telegram-generated/transcoded variant bytes.
    TelegramVariant,
    /// Metadata is retained but bytes are not fetchable.
    MetadataOnly,
}

impl AttachmentFidelity {
    /// Stable state/render vocabulary token.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Original => "original",
            Self::TelegramVariant => "telegram_variant",
            Self::MetadataOnly => "metadata_only",
        }
    }
}

/// One attachment mapped to its DOM § Attachment record: stable identity and
/// deterministic safe name bound over the source's raw [`AttachmentDescriptor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedAttachment {
    /// The attachment's stable identity `(account scope, chat, message,
    /// ordinal)` (DOM-021).
    pub key: AttachmentKey,
    /// The deterministic filesystem-safe display name (PRD-032).
    pub safe_name: SafeName,
    /// Logical media kind, orthogonal to Telegram representation.
    pub logical_kind: AttachmentLogicalKind,
    /// Telegram representation that exposed the attachment.
    pub telegram_representation: TelegramRepresentation,
    /// Truthful fidelity claim for the represented bytes.
    pub fidelity: AttachmentFidelity,
    /// Sender filename only for an original-document representation.
    pub source_name: Option<String>,
    /// The Telegram album (media group) this attachment's message belongs to,
    /// when any — shared provenance across an album's distinct items.
    pub album_id: Option<i64>,
    /// Telegram's per-message `can_be_saved` flag, verbatim (POL-4). Distinct
    /// from the descriptor's derived availability.
    pub can_be_saved: bool,
    /// The source's raw facts: locators, original name, MIME, size, dimensions,
    /// previews, and derived availability.
    pub descriptor: AttachmentDescriptor,
}

/// Map every attachment of one normalized message into its DOM § Attachment
/// record, in ordinal order.
///
/// `scope` is the account and namespace epoch the message was observed under —
/// the identity context the record cannot carry itself. Returns an empty vector
/// for a message with no downloadable attachment (text, service, expired,
/// unsupported).
pub fn map_message_attachments(
    record: &MessageRecord,
    scope: AccountScope,
) -> Vec<MappedAttachment> {
    let message = MessageKey {
        chat: ChatKey {
            scope,
            chat_id: ChatId(record.chat_id),
        },
        message_id: MessageId(record.message_id),
    };
    // v1 content carries at most one attachment, at ordinal zero (module docs);
    // an album's distinct identities come from distinct message ids, not from
    // multiple ordinals within one message. Returning a vector keeps the shape
    // ready for any future multi-attachment content without a caller change.
    record
        .content
        .attachment()
        .map(|descriptor| {
            let key = AttachmentKey {
                message,
                index: AttachmentIndex(0),
            };
            let representation = representation_of(&record.content);
            map_attachment(
                descriptor.clone(),
                key,
                record.album_id,
                record.can_be_saved,
                representation,
            )
        })
        .into_iter()
        .collect()
}

/// Bind identity and derive the safe name for one attachment descriptor.
///
/// The lower-level entry point [`map_message_attachments`] builds on; exposed so
/// a caller that already holds an [`AttachmentKey`] and the message's protection
/// facts can map a descriptor directly.
pub fn map_attachment(
    descriptor: AttachmentDescriptor,
    key: AttachmentKey,
    album_id: Option<i64>,
    can_be_saved: bool,
    telegram_representation: TelegramRepresentation,
) -> MappedAttachment {
    let logical_kind = logical_kind_of(&descriptor, telegram_representation);
    let source_name = (telegram_representation == TelegramRepresentation::OriginalDocument)
        .then(|| descriptor.file_name.clone())
        .flatten();
    let fidelity = if descriptor.availability != AttachmentAvailability::Fetchable {
        AttachmentFidelity::MetadataOnly
    } else if telegram_representation == TelegramRepresentation::OriginalDocument {
        AttachmentFidelity::Original
    } else {
        AttachmentFidelity::TelegramVariant
    };
    let safe_name = attachment_leaf_name(
        logical_kind.tag(),
        telegram_representation.tag(),
        source_name.as_deref(),
        descriptor.mime_type.as_deref(),
    );
    MappedAttachment {
        key,
        safe_name,
        logical_kind,
        telegram_representation,
        fidelity,
        source_name,
        album_id,
        can_be_saved,
        descriptor,
    }
}

fn representation_of(content: &MessageContent) -> TelegramRepresentation {
    match content {
        MessageContent::Document { .. } => TelegramRepresentation::OriginalDocument,
        MessageContent::Photo { .. }
        | MessageContent::Expired {
            kind: crate::message::ExpiredKind::Photo,
            ..
        } => TelegramRepresentation::Photo,
        MessageContent::Video { .. }
        | MessageContent::Expired {
            kind: crate::message::ExpiredKind::Video,
            ..
        } => TelegramRepresentation::Video,
        MessageContent::Animation { .. } => TelegramRepresentation::Animation,
        MessageContent::Audio { .. } => TelegramRepresentation::Audio,
        MessageContent::VoiceNote { .. }
        | MessageContent::Expired {
            kind: crate::message::ExpiredKind::VoiceNote,
            ..
        } => TelegramRepresentation::Voice,
        MessageContent::VideoNote { .. }
        | MessageContent::Expired {
            kind: crate::message::ExpiredKind::VideoNote,
            ..
        } => TelegramRepresentation::VideoNote,
        MessageContent::Sticker { .. } => TelegramRepresentation::Sticker,
        _ => TelegramRepresentation::OriginalDocument,
    }
}

fn logical_kind_of(
    descriptor: &AttachmentDescriptor,
    representation: TelegramRepresentation,
) -> AttachmentLogicalKind {
    if representation == TelegramRepresentation::OriginalDocument {
        let mime = descriptor
            .mime_type
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase();
        if mime.starts_with("image/") {
            return AttachmentLogicalKind::Photo;
        }
        if mime.starts_with("video/") {
            return AttachmentLogicalKind::Video;
        }
        if mime.starts_with("audio/") {
            return AttachmentLogicalKind::Audio;
        }
    }
    match descriptor.kind {
        AttachmentKind::Photo => AttachmentLogicalKind::Photo,
        AttachmentKind::Video => AttachmentLogicalKind::Video,
        AttachmentKind::Animation => AttachmentLogicalKind::Animation,
        AttachmentKind::Audio => AttachmentLogicalKind::Audio,
        AttachmentKind::Document => AttachmentLogicalKind::Document,
        AttachmentKind::VoiceNote => AttachmentLogicalKind::Voice,
        AttachmentKind::VideoNote => AttachmentLogicalKind::VideoNote,
        AttachmentKind::Sticker => AttachmentLogicalKind::Sticker,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gramdrive_model::identity::{AccountId, AccountKey, NamespaceVersion};
    use serde_json::{Value, json};

    use crate::message::{AttachmentAvailability, MessageContent, normalize_message};

    const CHAT: i64 = -100_500;
    const USER: i64 = 42;

    fn scope() -> AccountScope {
        AccountScope {
            account: AccountKey {
                account_id: AccountId(7),
            },
            namespace_version: NamespaceVersion(1),
        }
    }

    fn wire_message(id: i64, extra: Value, content: Value) -> Value {
        let mut message = json!({
            "@type": "message",
            "id": id,
            "sender_id": {"@type": "messageSenderUser", "user_id": USER},
            "chat_id": CHAT,
            "can_be_saved": true,
            "date": 1_752_800_000,
            "content": content
        });
        let object = message.as_object_mut().expect("object");
        for (key, value) in extra.as_object().expect("extra object") {
            object.insert(key.clone(), value.clone());
        }
        message
    }

    fn document(name: Value, mime: Value) -> Value {
        json!({
            "@type": "messageDocument",
            "caption": {"text": ""},
            "document": {
                "file_name": name,
                "mime_type": mime,
                "document": {"id": 5, "size": 2048, "remote": {"id": "r", "unique_id": "u"}}
            }
        })
    }

    fn map_one(message: &Value) -> MappedAttachment {
        let record = normalize_message(message).expect("normalizes");
        let mut mapped = map_message_attachments(&record, scope());
        assert_eq!(mapped.len(), 1, "expected exactly one attachment");
        mapped.remove(0)
    }

    #[test]
    fn identity_is_chat_message_and_ordinal_zero() {
        let mapped = map_one(&wire_message(
            9001,
            json!({}),
            document(json!("report.pdf"), json!("application/pdf")),
        ));
        assert_eq!(mapped.key.message.chat.scope, scope());
        assert_eq!(mapped.key.message.chat.chat_id.0, CHAT);
        assert_eq!(mapped.key.message.message_id.0, 9001);
        assert_eq!(mapped.key.index.0, 0);
    }

    #[test]
    fn original_name_is_preserved_and_sanitized() {
        // A hostile name with a path separator and a Windows-forbidden char is
        // preserved raw in the descriptor and projected to one safe component.
        let mapped = map_one(&wire_message(
            1,
            json!({}),
            document(json!("a/b:report.pdf"), json!("application/pdf")),
        ));
        assert_eq!(
            mapped.descriptor.file_name.as_deref(),
            Some("a/b:report.pdf")
        );
        assert_eq!(mapped.safe_name.as_str(), "a_b_report.pdf");
    }

    #[test]
    fn nameless_media_gets_a_stable_kind_default_name() {
        // A photo carries no file name; the safe name is a kind-and-MIME
        // default, deterministic for every such photo.
        let photo = json!({
            "@type": "messagePhoto",
            "photo": {"sizes": [
                {"type": "x", "width": 800, "height": 600,
                 "photo": {"id": 2, "size": 90_000, "remote": {"id": "r", "unique_id": "u"}}}
            ]}
        });
        let mapped = map_one(&wire_message(2, json!({}), photo));
        assert_eq!(mapped.descriptor.file_name, None);
        assert_eq!(mapped.safe_name.as_str(), "photo.jpg");
    }

    #[test]
    fn album_items_are_distinct_identities_sharing_provenance() {
        // Two messages of one album: distinct identities (distinct message
        // ids), the same album id as provenance, each unfetchable? no — both
        // fetchable, each its own attachment.
        let album = json!({"media_album_id": "770077"});
        let a = map_one(&wire_message(
            10,
            album.clone(),
            document(json!("a.pdf"), json!("application/pdf")),
        ));
        let b = map_one(&wire_message(
            11,
            album,
            document(json!("b.pdf"), json!("application/pdf")),
        ));
        assert_ne!(a.key, b.key, "distinct messages are distinct identities");
        assert_eq!(a.album_id, Some(770_077));
        assert_eq!(b.album_id, Some(770_077));
        assert_eq!(a.key.index.0, 0);
        assert_eq!(b.key.index.0, 0);
    }

    #[test]
    fn restricted_attachment_is_marked_and_carries_no_previews() {
        let mapped = map_one(&wire_message(
            3,
            json!({"can_be_saved": false}),
            document(json!("secret.pdf"), json!("application/pdf")),
        ));
        assert!(!mapped.can_be_saved);
        assert_eq!(
            mapped.descriptor.availability,
            AttachmentAvailability::Restricted
        );
        assert_eq!(mapped.descriptor.thumbnail, None);
        assert_eq!(mapped.descriptor.minithumbnail, None);
    }

    #[test]
    fn non_media_messages_map_to_no_attachments() {
        let text = wire_message(
            4,
            json!({}),
            json!({"@type": "messageText", "text": {"text": "hi"}}),
        );
        let record = normalize_message(&text).expect("normalizes");
        assert!(matches!(record.content, MessageContent::Text { .. }));
        assert!(map_message_attachments(&record, scope()).is_empty());
    }

    #[test]
    fn nameless_extension_follows_mime_then_kind() {
        assert_eq!(
            attachment_leaf_name("photo", "message_photo", None, None).as_str(),
            "photo.jpg"
        );
        assert_eq!(
            attachment_leaf_name("voice", "message_voice", None, None).as_str(),
            "voice.ogg"
        );
        assert_eq!(
            attachment_leaf_name("video_note", "message_video_note", None, None).as_str(),
            "video_note.mp4"
        );
        // A sticker's format is in its MIME: static webp vs. animated tgs vs.
        // video webm.
        assert_eq!(
            attachment_leaf_name(
                "sticker",
                "message_sticker",
                None,
                Some("application/x-tgsticker")
            )
            .as_str(),
            "sticker.tgs"
        );
        assert_eq!(
            attachment_leaf_name("sticker", "message_sticker", None, Some("video/webm")).as_str(),
            "sticker.webm"
        );
        assert_eq!(
            attachment_leaf_name("sticker", "message_sticker", None, None).as_str(),
            "sticker.webp"
        );
        // A MIME with parameters still resolves, case-insensitively.
        assert_eq!(
            attachment_leaf_name(
                "audio",
                "message_audio",
                None,
                Some("AUDIO/MPEG; charset=binary")
            )
            .as_str(),
            "audio.mp3"
        );
        // An unknown MIME on a kind with no fixed convention: no extension
        // rather than a wrong one.
        assert_eq!(
            attachment_leaf_name(
                "document",
                "original_document",
                None,
                Some("application/x-unknowable")
            )
            .as_str(),
            "document"
        );
    }
}
