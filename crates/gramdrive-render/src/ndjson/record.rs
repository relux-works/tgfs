//! The renderer's input contract: the structured message records the NDJSON
//! renderer projects, plus the retention mode that governs the projection.
//!
//! # Why this contract lives here
//!
//! `gramdrive-render` depends on `gramdrive-model` only (crate layering,
//! `crates/README.md`): it cannot read `gramdrive-state`. Rendering is a pure
//! function of canonical records (DOM-006), so the engine reads a chat's
//! messages, events, and attachments from the state repositories up to a render
//! watermark, builds the records below, and hands them to
//! [`crate::ndjson::render_messages`]. The state layer stores each observed
//! revision as an opaque payload blob (`message_events.payload`), never
//! interpreted by SQL; this module is the interpreted, provider-neutral shape
//! that payload decodes to for rendering.
//!
//! # Losslessness
//!
//! Every field the domain model's *Message record* names is representable
//! (`.spec/domain-model.md`): sender, timestamps, text and entities,
//! reply/thread/topic/album relationships, reactions, service action,
//! attachments, protection flags, and observed-deletion tombstones. The `Other`
//! variants on the closed enums ([`EntityKind`], [`MediaKind`],
//! [`ServiceAction`]) keep the schema lossless as Telegram adds kinds a v1
//! build has no named variant for: an unknown kind renders as `other` with its
//! raw tag preserved, never dropped.

use gramdrive_model::identity::{AttachmentIndex, ContentHash, MessageId, SchemaFamily};

/// Per-account edit/delete retention policy (POL-3, DEC-015). It selects the
/// projection the renderer emits, nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionMode {
    /// The archive reflects current Telegram state: one record per live
    /// message (its current revision), deleted messages omitted, prior
    /// revisions purged.
    Mirror,
    /// Everything observed is retained: every revision as a record, and a
    /// content-preserving tombstone for each observed deletion.
    Audit,
}

impl RetentionMode {
    /// The stable token written into the document header.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            RetentionMode::Mirror => "mirror",
            RetentionMode::Audit => "audit",
        }
    }
}

/// The full observed history of one message: its identity, and every revision
/// and deletion the archive has seen.
///
/// `revisions` must be non-empty — a message exists in the archive because an
/// observation created it. Order does not matter: the renderer sorts revisions
/// by [`Revision::event_seq`] before emitting, so shuffled input renders
/// byte-identical output. A [`MessageHistory`] with no revisions is malformed
/// and is skipped rather than rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageHistory {
    /// Telegram message identity within its chat.
    pub message_id: MessageId,
    /// The sender, if known. Absent for anonymous admins, channel posts, and
    /// imported history without a resolvable sender (SYNC-034 missing sender).
    pub sender: Option<Sender>,
    /// When the message was sent (ms since the Unix epoch); fixed across
    /// revisions.
    pub sent_at_ms: i64,
    /// Every observed revision, first sight and edits alike. Non-empty for a
    /// well-formed history; sorted by [`Revision::event_seq`] at render time.
    pub revisions: Vec<Revision>,
    /// The observed deletion, if any (POL-3). In Mirror mode a deleted message
    /// renders nothing; in Audit mode its latest revision becomes a
    /// content-preserving tombstone.
    pub deletion: Option<Deletion>,
}

/// Identity of a message sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sender {
    /// Telegram user or chat id of the sender.
    pub id: i64,
}

/// One observed revision of a message — a full content snapshot, as first sight
/// or as an edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    /// The append-only event-log sequence that recorded this revision
    /// (`message_events.event_seq`). Watermark-safe, never reused, and unique
    /// within a chat — the renderer's total sort key over revisions, and the
    /// provenance a reader joins back to the log (SYNC-024).
    pub event_seq: i64,
    /// When this revision was edited, if it is an edit rather than first sight.
    pub edited_at_ms: Option<i64>,
    /// When the source observed this revision (ms since the Unix epoch);
    /// source-explicit, never invented (SYNC-073).
    pub observed_at_ms: i64,
    /// Schema family of the payload this revision decoded from — the
    /// raw-schema/version metadata a lossless migration needs (DOM-023).
    pub payload_schema: SchemaFamily,
    /// The message content of this revision.
    pub body: MessageBody,
}

/// An observed deletion of a message (POL-3 tombstone).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deletion {
    /// When the deletion was observed (ms since the Unix epoch).
    pub observed_at_ms: i64,
}

/// The content of one message revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageBody {
    /// Message text or media caption, if any.
    pub text: Option<String>,
    /// Formatting/semantic entities over `text`, in source order.
    pub entities: Vec<Entity>,
    /// The message this one replies to, if any.
    pub reply_to: Option<MessageId>,
    /// The top message of the thread this message belongs to, if any.
    pub thread_top: Option<MessageId>,
    /// The forum topic this message belongs to, if any.
    pub topic_id: Option<i64>,
    /// The media group (album) this message belongs to, if any.
    pub album_id: Option<i64>,
    /// Reactions on the message, in source order.
    pub reactions: Vec<Reaction>,
    /// Attachments of the message, in attachment-index order.
    pub attachments: Vec<Attachment>,
    /// A service action, when the message is a service message rather than a
    /// user message.
    pub service: Option<ServiceAction>,
    /// Whether Telegram forbids saving this message's content
    /// (`can_be_saved == false`, POL-4). Protected content is still described;
    /// its bytes are never fetched.
    pub protected: bool,
}

/// A formatting or semantic entity over a span of message text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    /// The entity kind and its kind-specific data.
    pub kind: EntityKind,
    /// UTF-16 code-unit offset of the span (Telegram's own convention).
    pub offset: u32,
    /// UTF-16 code-unit length of the span.
    pub length: u32,
}

/// The kind of a text [`Entity`].
///
/// Data-carrying variants keep the entity lossless (a `text_link` without its
/// URL is not the same entity). [`EntityKind::Other`] preserves the raw tag of
/// a kind this build has no named variant for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityKind {
    /// Bold text.
    Bold,
    /// Italic text.
    Italic,
    /// Underlined text.
    Underline,
    /// Struck-through text.
    Strikethrough,
    /// A hidden (spoiler) span.
    Spoiler,
    /// Inline monospace code.
    Code,
    /// A preformatted code block, with an optional language tag.
    Pre {
        /// The declared language of the block, if any.
        language: Option<String>,
    },
    /// A block quotation.
    Blockquote,
    /// A bare URL in the text.
    Url,
    /// A hyperlink whose target differs from the displayed text.
    TextLink {
        /// The link target.
        url: String,
    },
    /// An `@username` mention.
    Mention,
    /// A mention of a user with no public username, carrying the user id.
    TextMention {
        /// The mentioned user's id.
        user_id: i64,
    },
    /// A `#hashtag`.
    Hashtag,
    /// A `$cashtag`.
    Cashtag,
    /// A `/bot_command`.
    BotCommand,
    /// An email address.
    Email,
    /// A phone number.
    PhoneNumber,
    /// A bank-card number.
    BankCard,
    /// A custom emoji, carrying its document id.
    CustomEmoji {
        /// The custom-emoji document id.
        document_id: i64,
    },
    /// A kind with no named variant in this build; the raw tag is preserved.
    Other {
        /// The source kind tag.
        kind: String,
    },
}

impl EntityKind {
    /// The stable token written for this kind.
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            EntityKind::Bold => "bold",
            EntityKind::Italic => "italic",
            EntityKind::Underline => "underline",
            EntityKind::Strikethrough => "strikethrough",
            EntityKind::Spoiler => "spoiler",
            EntityKind::Code => "code",
            EntityKind::Pre { .. } => "pre",
            EntityKind::Blockquote => "blockquote",
            EntityKind::Url => "url",
            EntityKind::TextLink { .. } => "text_link",
            EntityKind::Mention => "mention",
            EntityKind::TextMention { .. } => "text_mention",
            EntityKind::Hashtag => "hashtag",
            EntityKind::Cashtag => "cashtag",
            EntityKind::BotCommand => "bot_command",
            EntityKind::Email => "email",
            EntityKind::PhoneNumber => "phone_number",
            EntityKind::BankCard => "bank_card",
            EntityKind::CustomEmoji { .. } => "custom_emoji",
            EntityKind::Other { .. } => "other",
        }
    }
}

/// A reaction on a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    /// Which reaction this counts.
    pub key: ReactionKey,
    /// How many users reacted with it.
    pub count: u32,
    /// Whether the archive's own account reacted with it.
    pub chosen: bool,
}

/// The identity of a reaction: a unicode emoji or a custom emoji.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionKey {
    /// A standard unicode-emoji reaction.
    Emoji(String),
    /// A custom-emoji reaction, carrying its document id.
    Custom(i64),
}

/// A downloadable attachment of a message.
///
/// The renderer derives the attachment's stable item-id link from the message
/// identity and [`Attachment::index`] (SYNC-032); the caller does not supply
/// it. [`Attachment::content_hash`] is present only once the bytes are
/// downloaded and verified — absent means a dataless placeholder (POL-2) or an
/// unavailable item (POL-4), disambiguated by [`Attachment::availability`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// GramDrive ordinal within the message's attachments (DOM-021).
    pub index: AttachmentIndex,
    /// What kind of media this attachment is.
    pub media_kind: MediaKind,
    /// The original file name, if the source provided one.
    pub name: Option<String>,
    /// The MIME type, if known.
    pub mime_type: Option<String>,
    /// The logical size in bytes, if known.
    pub size: Option<u64>,
    /// Whether and why the bytes are (un)fetchable (POL-4).
    pub availability: Availability,
    /// The content hash of the downloaded bytes, once materialized.
    pub content_hash: Option<ContentHash>,
}

/// The media kind of an [`Attachment`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaKind {
    /// A photo.
    Photo,
    /// A video.
    Video,
    /// A round video note.
    VideoNote,
    /// An animation (GIF/muted looping video).
    Animation,
    /// A sticker.
    Sticker,
    /// An audio track.
    Audio,
    /// A voice note.
    Voice,
    /// A generic document/file.
    Document,
    /// A shared contact.
    Contact,
    /// A shared location.
    Location,
    /// A shared venue.
    Venue,
    /// A poll.
    Poll,
    /// A dice/animated-emoji throw.
    Dice,
    /// A kind with no named variant in this build; the raw tag is preserved.
    Other {
        /// The source kind tag.
        kind: String,
    },
}

impl MediaKind {
    /// The stable token written for this kind.
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            MediaKind::Photo => "photo",
            MediaKind::Video => "video",
            MediaKind::VideoNote => "video_note",
            MediaKind::Animation => "animation",
            MediaKind::Sticker => "sticker",
            MediaKind::Audio => "audio",
            MediaKind::Voice => "voice",
            MediaKind::Document => "document",
            MediaKind::Contact => "contact",
            MediaKind::Location => "location",
            MediaKind::Venue => "venue",
            MediaKind::Poll => "poll",
            MediaKind::Dice => "dice",
            MediaKind::Other { .. } => "other",
        }
    }
}

/// Whether an attachment's bytes can be fetched, and why not when they cannot
/// (POL-4). Mirrors the state layer's `attachments.availability` vocabulary so
/// the two never drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// The bytes may be fetched (subject to placeholder/hydration policy).
    Fetchable,
    /// Protected content: visible in structure, bytes never fetched.
    Restricted,
    /// The item is unavailable (deleted at source, expired, unsupported).
    Unavailable,
    /// View-once/self-destructing media: never persisted.
    ViewOnce,
}

impl Availability {
    /// The stable token written for this state.
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Availability::Fetchable => "fetchable",
            Availability::Restricted => "restricted",
            Availability::Unavailable => "unavailable",
            Availability::ViewOnce => "view_once",
        }
    }
}

/// A service action carried by a service message.
///
/// A pragmatic v1 set of the common actions plus [`ServiceAction::Other`] for
/// forward compatibility; the normalizer that populates these lives in the
/// engine/source layer, and a kind it cannot map yet is preserved raw rather
/// than dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceAction {
    /// The chat/group was created with the given title.
    ChatCreated {
        /// The initial title.
        title: String,
    },
    /// The chat title changed to the given value.
    ChatTitleChanged {
        /// The new title.
        title: String,
    },
    /// Members were added.
    MembersAdded {
        /// The added users' ids.
        user_ids: Vec<i64>,
    },
    /// A member was removed or left.
    MemberRemoved {
        /// The removed user's id.
        user_id: i64,
    },
    /// A message was pinned.
    MessagePinned {
        /// The pinned message's id.
        message_id: MessageId,
    },
    /// The auto-delete timer changed.
    AutoDeleteTimerChanged {
        /// The new timer in seconds (0 disables it).
        seconds: i64,
    },
    /// An action with no named variant in this build; the raw tag is preserved.
    Other {
        /// The source action tag.
        kind: String,
    },
}

impl ServiceAction {
    /// The stable token written for this action.
    pub(crate) fn tag(&self) -> &'static str {
        match self {
            ServiceAction::ChatCreated { .. } => "chat_created",
            ServiceAction::ChatTitleChanged { .. } => "chat_title_changed",
            ServiceAction::MembersAdded { .. } => "members_added",
            ServiceAction::MemberRemoved { .. } => "member_removed",
            ServiceAction::MessagePinned { .. } => "message_pinned",
            ServiceAction::AutoDeleteTimerChanged { .. } => "auto_delete_timer_changed",
            ServiceAction::Other { .. } => "other",
        }
    }
}
