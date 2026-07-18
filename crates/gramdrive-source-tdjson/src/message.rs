//! Message normalization: one TDLib `message` object becomes one typed,
//! provider-neutral [`MessageRecord`] (TASK-260715-1ynmct, PRD-022).
//!
//! # Where it sits
//!
//! The history crawl (TASK-260715-26dnp6) and the ordered update loop
//! (TASK-260715-10p5zp) both receive raw TDLib `message` JSON — from
//! `getChatHistory` answers and from `updateNewMessage`/
//! `updateMessageContent` pushes. Both feed it through this one normalizer,
//! so the state layer's append-only message event log (`message_events`)
//! stores a single vocabulary regardless of which path observed the
//! message. Like the rest of this crate, nothing here performs I/O: these
//! are pure functions over parsed `serde_json::Value`s.
//!
//! # What the record carries (PRD-022)
//!
//! Identity (chat and message id), time (sent/edited, integer milliseconds
//! by the boundary rule — no OS time type crosses the contract), the sender
//! reference, text or caption with formatting entities, the reply target,
//! the topic, album membership, reactions, service actions, the POL-4
//! protection facts, and one attachment descriptor per media content —
//! exactly the DOM § Message record facts a source can know. Deliberately
//! absent, per the PRD-022 v1 scope: forward origins, view/forward
//! counters, reply markup, and paid-content accounting.
//!
//! # Degradation, not omission (PRD-024)
//!
//! The strict/lenient split follows the snapshot machine's rule — strict
//! about identity, lenient about the periphery:
//!
//! - A message whose *identity* is unreadable (no integer `id`, `chat_id`,
//!   or `date`; no `content` object; a content object with no `@type`) is a
//!   typed [`MessageError::Malformed`] — the caller decides whether that
//!   fails a crawl page. It is never a guessed record.
//! - A *content type* this build does not model — a future TDLib addition,
//!   or one of the many types outside the PRD-022/PRD-030 v1 classes
//!   (polls, locations, invoices, video chats …) — becomes
//!   [`MessageContent::Unsupported`]: an explicit typed record carrying the
//!   raw `@type` plus the verbatim content JSON under
//!   [`RAW_SCHEMA_VERSION`], so a future build can re-normalize what this
//!   one could not (DOM § Message record: versioned raw preservation for
//!   migration — kept *only* here, where normalization lost information).
//!   A known content type whose required members are missing degrades the
//!   same way: the raw JSON preserves what the strict parse rejected.
//! - A *peripheral* fact of an unknown shape — a sender, reply target,
//!   topic, reaction type, or self-destruct flavor this build does not know
//!   — degrades to that vocabulary's own `Unknown` variant, keeping the
//!   rest of the record intact. Self-destructing media of an unknown
//!   flavor still counts as self-destructing (fail-closed, POL-4).
//! - Text formatting *entities* that fail structural parse (no offset,
//!   length, or type object) are dropped — the text itself is never lost,
//!   only broken decoration — while an entity of an unknown *type* keeps
//!   its span as [`TextEntityKind::Unknown`], so a renderer can treat it as
//!   plain text.
//!
//! # Protection (POL-4)
//!
//! Every record carries Telegram's `can_be_saved` flag verbatim, and every
//! attachment descriptor carries a derived [`AttachmentAvailability`]:
//! self-destructing or secret media is [`AttachmentAvailability::ViewOnce`]
//! (never persisted), save-restricted media is
//! [`AttachmentAvailability::Restricted`] (visible placeholder, bytes never
//! fetched), everything else is fetchable. Expired self-destruct
//! placeholders (`messageExpiredPhoto` and friends) normalize to
//! [`MessageContent::Expired`] — explicitly unavailable, no attachment, no
//! fabricated recoverability (PRD-024).
//!
//! # Albums
//!
//! Telegram models an album as consecutive messages sharing a
//! `media_album_id`; the record carries that id verbatim
//! ([`MessageRecord::album_id`]) as the grouping key. Assembling the group
//! is the consumer's join — the normalizer sees one message at a time.

use serde_json::Value;

use crate::wire::parse_int64;

/// Version of this module's raw-preservation dialect, stored beside every
/// [`UnsupportedContent::raw_json`]. A future build that changes how raw
/// content is captured (or re-normalizes preserved raws after a TDLib pin
/// bump changed the wire shapes) bumps this and keys its migration on the
/// stored value.
pub const RAW_SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Text and entities
// ---------------------------------------------------------------------------

/// Text with its formatting entities — TDLib's `formattedText`, verbatim.
/// Offsets are UTF-16 code units, exactly as Telegram defines them; the
/// renderer owns any re-encoding.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FormattedText {
    /// The plain text.
    pub text: String,
    /// Formatting entities over `text`, in wire order.
    pub entities: Vec<TextEntity>,
}

/// One formatting entity: a span of the text plus what decorates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEntity {
    /// Span start, in UTF-16 code units.
    pub offset: u32,
    /// Span length, in UTF-16 code units.
    pub length: u32,
    /// What the span is.
    pub kind: TextEntityKind,
}

/// The entity vocabulary — TDLib's `TextEntityType`, with unknown types
/// degrading to [`TextEntityKind::Unknown`] so the span renders as plain
/// text instead of vanishing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextEntityKind {
    /// A `@username` mention.
    Mention,
    /// A `#hashtag`.
    Hashtag,
    /// A `$cashtag`.
    Cashtag,
    /// A `/bot_command`.
    BotCommand,
    /// A bare URL.
    Url,
    /// An email address.
    EmailAddress,
    /// A phone number.
    PhoneNumber,
    /// A bank card number.
    BankCardNumber,
    /// Bold text.
    Bold,
    /// Italic text.
    Italic,
    /// Underlined text.
    Underline,
    /// Struck-through text.
    Strikethrough,
    /// A spoiler.
    Spoiler,
    /// Inline code.
    Code,
    /// A preformatted block.
    Pre,
    /// A preformatted block with a language tag.
    PreCode {
        /// The stated programming language; empty means unstated.
        language: String,
    },
    /// A block quote.
    BlockQuote,
    /// A collapsible block quote.
    ExpandableBlockQuote,
    /// Text linking to a URL.
    TextUrl {
        /// The link target.
        url: String,
    },
    /// A mention of a user without a username.
    MentionName {
        /// The mentioned user.
        user_id: i64,
    },
    /// A custom emoji placeholder.
    CustomEmoji {
        /// The custom emoji id.
        custom_emoji_id: i64,
    },
    /// A media timestamp link (jumps playback).
    MediaTimestamp {
        /// The playback position, in seconds.
        seconds: u32,
    },
    /// An entity type this build does not know, or a known type whose
    /// required members were missing. The span stays; a renderer treats it
    /// as plain text.
    Unknown {
        /// The TDLib `@type`.
        raw_type: String,
    },
}

// ---------------------------------------------------------------------------
// Peripheral references
// ---------------------------------------------------------------------------

/// Who sent a message — TDLib's `MessageSender` (DOM § Message record:
/// sender identity reference; resolving it to a display name is the chat
/// metadata layer's job).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SenderRef {
    /// A user.
    User {
        /// The sending user.
        user_id: i64,
    },
    /// A chat (anonymous admins, channel posts).
    Chat {
        /// The sending chat.
        chat_id: i64,
    },
    /// A sender shape this build does not know, or a broken one.
    Unknown {
        /// The TDLib `@type`; `None` when the field was absent or carried
        /// no type at all.
        raw_type: Option<String>,
    },
}

/// What a message replies to — TDLib's `MessageReplyTo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyTarget {
    /// A reply to another message.
    Message {
        /// The replied-to message's chat when it differs from the reply's
        /// own chat; `None` for the ordinary same-chat reply.
        chat_id: Option<i64>,
        /// The replied-to message.
        message_id: i64,
        /// The manually chosen quote, when the reply carries one.
        quote: Option<FormattedText>,
    },
    /// A reply to a story.
    Story {
        /// The chat that posted the story.
        poster_chat_id: i64,
        /// The story id within that chat.
        story_id: i64,
    },
    /// A reply shape this build does not know, or a broken one.
    Unknown {
        /// The TDLib `@type`; `None` when the object carried no type.
        raw_type: Option<String>,
    },
}

/// The topic a message belongs to — TDLib's `MessageTopic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicRef {
    /// A forum topic of a supergroup.
    Forum {
        /// The forum topic id.
        forum_topic_id: i64,
    },
    /// A topic in a channel's direct-messages chat.
    DirectMessages {
        /// The direct-messages chat topic id.
        topic_id: i64,
    },
    /// A Saved Messages topic.
    SavedMessages {
        /// The Saved Messages topic id.
        topic_id: i64,
    },
    /// A topic shape this build does not know, or a broken one.
    Unknown {
        /// The TDLib `@type`; `None` when the object carried no type.
        raw_type: Option<String>,
    },
}

/// One reaction tally on a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    /// What was reacted with.
    pub kind: ReactionKind,
    /// How many senders chose it.
    pub count: u32,
    /// Whether the account's own sender is among them.
    pub chosen: bool,
}

/// The reaction vocabulary — TDLib's `ReactionType`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReactionKind {
    /// A plain emoji reaction.
    Emoji {
        /// The emoji.
        emoji: String,
    },
    /// A custom emoji reaction.
    CustomEmoji {
        /// The custom emoji id.
        custom_emoji_id: i64,
    },
    /// The paid (Telegram Stars) reaction.
    Paid,
    /// A reaction type this build does not know, or a broken one.
    Unknown {
        /// The TDLib `@type`; `None` when the object carried no type.
        raw_type: Option<String>,
    },
}

/// How a message self-destructs — TDLib's `MessageSelfDestructType`. Any
/// present flavor, known or not, marks the message's media view-once:
/// never persisted, shown as unavailable (POL-4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfDestruct {
    /// Destroyed a fixed time after viewing.
    Timer {
        /// The timer, in seconds.
        seconds: u32,
    },
    /// Destroyed immediately after viewing.
    Immediate,
    /// A flavor this build does not know — still self-destructing.
    Unknown {
        /// The TDLib `@type`; `None` when the object carried no type.
        raw_type: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Attachments (PRD-030/PRD-032, POL-4)
// ---------------------------------------------------------------------------

/// The attachment flavor vocabulary — the PRD-030 v1 downloadable classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttachmentKind {
    /// A photo (its largest available size).
    Photo,
    /// A video.
    Video,
    /// An animation (GIF/MP4 loop).
    Animation,
    /// A music/audio file.
    Audio,
    /// A generic document.
    Document,
    /// A voice note.
    VoiceNote,
    /// A video note (round video).
    VideoNote,
    /// A sticker.
    Sticker,
}

/// Whether an attachment's bytes may ever be fetched (POL-4). Derived at
/// normalization from the message's protection facts, never parsed from a
/// flag of its own — so a restricted attachment cannot claim otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentAvailability {
    /// The bytes may be fetched on demand.
    Fetchable,
    /// Telegram restricts saving (`can_be_saved` is false): the attachment
    /// stays a visible placeholder and its bytes never enter the archive.
    Restricted,
    /// View-once / self-destructing media: never persisted, shown as
    /// unavailable.
    ViewOnce,
}

/// One downloadable attachment as the message describes it — the source's
/// half of DOM § Attachment: original metadata plus Telegram locators.
/// Attachment identity (chat, message, ordinal) is the consumer's key; v1
/// message contents carry at most one attachment, at ordinal zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentDescriptor {
    /// The attachment flavor.
    pub kind: AttachmentKind,
    /// TDLib's file id — the local locator `downloadFile` takes, stable
    /// within one TDLib database.
    pub file_id: i32,
    /// Telegram's refreshable remote locator, when known (SYNC-045).
    pub remote_id: Option<String>,
    /// Telegram's stable remote file identifier, when known — the PRD-033
    /// dedup key.
    pub remote_unique_id: Option<String>,
    /// Original file name, when the message carried one (PRD-032).
    pub file_name: Option<String>,
    /// MIME type, when the source states one (PRD-032).
    pub mime_type: Option<String>,
    /// Size in bytes: exact when TDLib knows it, else Telegram's expected
    /// size, else `None`.
    pub size: Option<u64>,
    /// Pixel width, for visual media.
    pub width: Option<u32>,
    /// Pixel height, for visual media.
    pub height: Option<u32>,
    /// Duration in seconds, for timed media.
    pub duration_secs: Option<u32>,
    /// Whether the bytes may ever be fetched (POL-4).
    pub availability: AttachmentAvailability,
}

/// The POL-4 protection facts of one message, as inputs to
/// [`normalize_content`] — split out so the content-only entry point (the
/// `updateMessageContent` path) states them explicitly instead of guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtectionFacts {
    /// Telegram's per-message save permission (`can_be_saved`).
    pub can_be_saved: bool,
    /// Whether the message self-destructs (any `self_destruct_type`, or
    /// secret media flagged on the content itself).
    pub self_destructing: bool,
}

impl ProtectionFacts {
    /// The availability these facts imply for the message's attachments
    /// (POL-4): view-once wins over restricted wins over fetchable.
    pub fn availability(self) -> AttachmentAvailability {
        if self.self_destructing {
            AttachmentAvailability::ViewOnce
        } else if !self.can_be_saved {
            AttachmentAvailability::Restricted
        } else {
            AttachmentAvailability::Fetchable
        }
    }
}

// ---------------------------------------------------------------------------
// Content
// ---------------------------------------------------------------------------

/// A media class whose self-destructing bytes are already gone — TDLib's
/// `messageExpired*` placeholders (POL-4: shown as unavailable, nothing to
/// fetch, no recoverability implied).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpiredKind {
    /// An expired photo.
    Photo,
    /// An expired video.
    Video,
    /// An expired video note.
    VideoNote,
    /// An expired voice note.
    VoiceNote,
}

/// A service action — the chat events Telegram delivers as messages, in
/// the subset the v1 renderers narrate (PRD-022). Service types outside
/// this subset degrade to [`MessageContent::Unsupported`] like any other
/// unmodeled content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceAction {
    /// The chat was created (basic group or supergroup/channel).
    ChatCreated {
        /// The initial title.
        title: String,
        /// Founding members, when Telegram lists them (basic groups only;
        /// empty for supergroups and channels).
        member_user_ids: Vec<i64>,
    },
    /// The chat title changed.
    TitleChanged {
        /// The new title.
        title: String,
    },
    /// The chat photo changed. The photo itself is not persisted (v1 keeps
    /// no avatars); the event is narrated.
    PhotoChanged,
    /// The chat photo was removed.
    PhotoDeleted,
    /// Members were added.
    MembersAdded {
        /// The added users.
        user_ids: Vec<i64>,
    },
    /// Someone joined via an invite link.
    JoinedByLink,
    /// Someone joined after an approved join request.
    JoinedByRequest,
    /// A member left or was removed.
    MemberRemoved {
        /// The user who left or was removed.
        user_id: i64,
    },
    /// The basic group upgraded to a supergroup.
    UpgradedToSupergroup {
        /// The supergroup it became.
        supergroup_id: i64,
    },
    /// The supergroup was created by upgrading a basic group.
    UpgradedFromBasicGroup {
        /// The title carried over.
        title: String,
        /// The basic group it came from.
        basic_group_id: i64,
    },
    /// A message was pinned.
    MessagePinned {
        /// The pinned message.
        message_id: i64,
    },
    /// A screenshot was taken in the chat.
    ScreenshotTaken,
    /// The auto-delete timer changed.
    AutoDeleteTimeChanged {
        /// The new timer in seconds; zero disables it.
        seconds: u32,
    },
    /// A forum topic was created.
    TopicCreated {
        /// The topic name.
        name: String,
    },
    /// A forum topic was edited.
    TopicEdited {
        /// The new name; `None` when the edit changed something else.
        name: Option<String>,
    },
    /// A forum topic was closed or reopened.
    TopicClosedToggled {
        /// Whether the topic is now closed.
        closed: bool,
    },
    /// A contact of the account registered on Telegram.
    ContactRegistered,
}

/// A content type this build could not normalize, preserved raw and
/// versioned for a future migration (DOM § Message record). The one place
/// raw TDLib JSON is retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedContent {
    /// The TDLib `@type` of the content object.
    pub raw_type: String,
    /// The verbatim content object as compact JSON with sorted keys —
    /// deterministic bytes for identical wire content.
    pub raw_json: String,
    /// Which raw-preservation dialect wrote `raw_json`
    /// ([`RAW_SCHEMA_VERSION`] of the writing build).
    pub raw_schema_version: u32,
}

/// What a message *is* — the PRD-022/PRD-030 v1 content classes, plus the
/// explicit degradations ([`MessageContent::Expired`],
/// [`MessageContent::Unsupported`]). Nothing is dropped silently: every
/// TDLib content object maps onto exactly one variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageContent {
    /// A text message.
    Text {
        /// The text with entities.
        text: FormattedText,
    },
    /// A photo.
    Photo {
        /// The caption with entities; empty when uncaptioned.
        caption: FormattedText,
        /// The photo's largest available size.
        attachment: AttachmentDescriptor,
    },
    /// A video.
    Video {
        /// The caption with entities; empty when uncaptioned.
        caption: FormattedText,
        /// The video file.
        attachment: AttachmentDescriptor,
    },
    /// An animation (GIF/MP4 loop).
    Animation {
        /// The caption with entities; empty when uncaptioned.
        caption: FormattedText,
        /// The animation file.
        attachment: AttachmentDescriptor,
    },
    /// A music/audio file.
    Audio {
        /// The caption with entities; empty when uncaptioned.
        caption: FormattedText,
        /// The audio file.
        attachment: AttachmentDescriptor,
    },
    /// A generic document.
    Document {
        /// The caption with entities; empty when uncaptioned.
        caption: FormattedText,
        /// The document file.
        attachment: AttachmentDescriptor,
    },
    /// A voice note.
    VoiceNote {
        /// The caption with entities; empty when uncaptioned.
        caption: FormattedText,
        /// The voice recording.
        attachment: AttachmentDescriptor,
    },
    /// A video note (round video; never captioned).
    VideoNote {
        /// The video note file.
        attachment: AttachmentDescriptor,
    },
    /// A sticker.
    Sticker {
        /// The emoji the sticker corresponds to.
        emoji: String,
        /// The sticker file.
        attachment: AttachmentDescriptor,
    },
    /// Self-destructing media whose bytes are already gone (POL-4).
    Expired {
        /// Which media class expired.
        kind: ExpiredKind,
    },
    /// A chat service event delivered as a message.
    Service {
        /// The narrated action.
        action: ServiceAction,
    },
    /// A content type this build could not normalize — explicit, raw
    /// preserved, never a crash and never a silent drop (PRD-024).
    Unsupported {
        /// The preserved raw content.
        content: UnsupportedContent,
    },
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// One message, normalized: the DOM § Message record facts a source
/// observation can carry, in provider-neutral vocabulary. This is the
/// payload the composing caller serializes into the state layer's
/// append-only `message_events` log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    /// The chat the message belongs to.
    pub chat_id: i64,
    /// Telegram's message id, unique within the chat.
    pub message_id: i64,
    /// Who sent it.
    pub sender: SenderRef,
    /// When it was sent, in milliseconds since the Unix epoch (UTC).
    pub sent_at_ms: i64,
    /// When it was last edited, when Telegram reports an edit. Which
    /// revision the archive keeps is the retention policy's decision
    /// (TASK-260715-37nhe5); the record states the observation.
    pub edited_at_ms: Option<i64>,
    /// What the message replies to, when it replies at all.
    pub reply: Option<ReplyTarget>,
    /// The topic it belongs to, when the chat is topic-structured.
    pub topic: Option<TopicRef>,
    /// The album (media group) it belongs to — Telegram's opaque grouping
    /// key, shared by the album's consecutive messages.
    pub album_id: Option<i64>,
    /// Reaction tallies, in wire order.
    pub reactions: Vec<Reaction>,
    /// Telegram's per-message save permission, verbatim (POL-4).
    pub can_be_saved: bool,
    /// How the message self-destructs, when it does (POL-4: its media is
    /// never persisted).
    pub self_destruct: Option<SelfDestruct>,
    /// What the message is.
    pub content: MessageContent,
}

/// Why a message could not be normalized at all. Everything short of this
/// degrades inside the record instead (module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessageError {
    /// The object is missing the identity or content structure the tdjson
    /// protocol promises every message.
    Malformed {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
}

impl std::fmt::Display for MessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageError::Malformed { detail } => {
                write!(f, "malformed message object: {detail}")
            }
        }
    }
}

impl std::error::Error for MessageError {}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Normalize one TDLib `message` object into a [`MessageRecord`].
///
/// Strict about identity (`id`, `chat_id`, `date`, and the presence of a
/// typed `content` object), degrading about everything else — see the
/// module docs for the exact split.
pub fn normalize_message(message: &Value) -> Result<MessageRecord, MessageError> {
    let chat_id = require_i64(message, "chat_id")?;
    let message_id = require_i64(message, "id")?;
    let date = require_i64(message, "date")?;
    let edited = message
        .get("edit_date")
        .and_then(Value::as_i64)
        .filter(|date| *date > 0);
    let self_destruct = message.get("self_destruct_type").map(self_destruct_kind);
    let protection = ProtectionFacts {
        can_be_saved: message
            .get("can_be_saved")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        self_destructing: self_destruct.is_some(),
    };
    let Some(content) = message.get("content") else {
        return Err(MessageError::Malformed {
            detail: format!("message {message_id} in chat {chat_id} has no content object"),
        });
    };
    Ok(MessageRecord {
        chat_id,
        message_id,
        sender: sender_ref(message.get("sender_id")),
        sent_at_ms: date.saturating_mul(1000),
        edited_at_ms: edited.map(|date| date.saturating_mul(1000)),
        reply: message.get("reply_to").map(reply_target),
        topic: message.get("topic_id").map(topic_ref),
        album_id: message
            .get("media_album_id")
            .and_then(parse_int64)
            .filter(|id| *id != 0),
        reactions: message
            .get("interaction_info")
            .map(normalize_reactions)
            .unwrap_or_default(),
        can_be_saved: protection.can_be_saved,
        self_destruct,
        content: normalize_content(content, protection)?,
    })
}

/// Normalize one TDLib `MessageContent` object under the message's POL-4
/// protection facts — the entry point the edit path (`updateMessageContent`
/// carries only the new content) shares with [`normalize_message`].
///
/// The only error is a content object with no `@type` (a tdjson protocol
/// violation); every unmodeled or structurally broken *typed* content
/// degrades to [`MessageContent::Unsupported`] with its raw JSON preserved.
pub fn normalize_content(
    content: &Value,
    protection: ProtectionFacts,
) -> Result<MessageContent, MessageError> {
    let Some(raw_type) = content.get("@type").and_then(Value::as_str) else {
        return Err(MessageError::Malformed {
            detail: "content object without an @type".to_owned(),
        });
    };
    // Secret media is flagged on the content object too; fold it into the
    // self-destruct fact so availability fails closed either way (POL-4).
    let protection = if content
        .get("is_secret")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        ProtectionFacts {
            self_destructing: true,
            ..protection
        }
    } else {
        protection
    };
    let normalized = match raw_type {
        "messageText" => Some(MessageContent::Text {
            text: content.get("text").map(formatted_text).unwrap_or_default(),
        }),
        "messagePhoto" => photo_attachment(content.get("photo"), protection).map(|attachment| {
            MessageContent::Photo {
                caption: caption(content),
                attachment,
            }
        }),
        "messageVideo" => media_attachment(content, "video", AttachmentKind::Video, protection)
            .map(|attachment| MessageContent::Video {
                caption: caption(content),
                attachment,
            }),
        "messageAnimation" => {
            media_attachment(content, "animation", AttachmentKind::Animation, protection).map(
                |attachment| MessageContent::Animation {
                    caption: caption(content),
                    attachment,
                },
            )
        }
        "messageAudio" => media_attachment(content, "audio", AttachmentKind::Audio, protection)
            .map(|attachment| MessageContent::Audio {
                caption: caption(content),
                attachment,
            }),
        "messageDocument" => {
            media_attachment(content, "document", AttachmentKind::Document, protection).map(
                |attachment| MessageContent::Document {
                    caption: caption(content),
                    attachment,
                },
            )
        }
        "messageVoiceNote" => {
            media_attachment(content, "voice_note", AttachmentKind::VoiceNote, protection).map(
                |attachment| MessageContent::VoiceNote {
                    caption: caption(content),
                    attachment,
                },
            )
        }
        "messageVideoNote" => {
            media_attachment(content, "video_note", AttachmentKind::VideoNote, protection)
                .map(|attachment| MessageContent::VideoNote { attachment })
        }
        "messageSticker" => {
            media_attachment(content, "sticker", AttachmentKind::Sticker, protection).map(
                |attachment| MessageContent::Sticker {
                    emoji: content
                        .get("sticker")
                        .and_then(|sticker| sticker.get("emoji"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    attachment,
                },
            )
        }
        "messageExpiredPhoto" => Some(expired(ExpiredKind::Photo)),
        "messageExpiredVideo" => Some(expired(ExpiredKind::Video)),
        "messageExpiredVideoNote" => Some(expired(ExpiredKind::VideoNote)),
        "messageExpiredVoiceNote" => Some(expired(ExpiredKind::VoiceNote)),
        _ => service_action(raw_type, content).map(|action| MessageContent::Service { action }),
    };
    Ok(normalized.unwrap_or_else(|| MessageContent::Unsupported {
        content: UnsupportedContent {
            raw_type: raw_type.to_owned(),
            raw_json: content.to_string(),
            raw_schema_version: RAW_SCHEMA_VERSION,
        },
    }))
}

/// Extract reaction tallies from a TDLib `messageInteractionInfo` object —
/// shared with the reaction-update path (`updateMessageInteractionInfo`
/// carries the same object). Absent or empty reaction sets are an empty
/// vector; a structurally broken tally is dropped, an unknown reaction
/// *type* is kept as [`ReactionKind::Unknown`].
pub fn normalize_reactions(interaction_info: &Value) -> Vec<Reaction> {
    interaction_info
        .get("reactions")
        .and_then(|reactions| reactions.get("reactions"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(reaction).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Leaf parsers
// ---------------------------------------------------------------------------

fn require_i64(message: &Value, field: &str) -> Result<i64, MessageError> {
    message
        .get(field)
        .and_then(Value::as_i64)
        .ok_or_else(|| MessageError::Malformed {
            detail: format!("message without an integer {field}"),
        })
}

fn raw_type_of(value: &Value) -> Option<String> {
    value
        .get("@type")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn sender_ref(value: Option<&Value>) -> SenderRef {
    let Some(value) = value else {
        return SenderRef::Unknown { raw_type: None };
    };
    match value.get("@type").and_then(Value::as_str) {
        Some("messageSenderUser") => match value.get("user_id").and_then(Value::as_i64) {
            Some(user_id) => SenderRef::User { user_id },
            None => SenderRef::Unknown {
                raw_type: raw_type_of(value),
            },
        },
        Some("messageSenderChat") => match value.get("chat_id").and_then(Value::as_i64) {
            Some(chat_id) => SenderRef::Chat { chat_id },
            None => SenderRef::Unknown {
                raw_type: raw_type_of(value),
            },
        },
        _ => SenderRef::Unknown {
            raw_type: raw_type_of(value),
        },
    }
}

fn reply_target(value: &Value) -> ReplyTarget {
    match value.get("@type").and_then(Value::as_str) {
        Some("messageReplyToMessage") => match value.get("message_id").and_then(Value::as_i64) {
            Some(message_id) => ReplyTarget::Message {
                chat_id: value
                    .get("chat_id")
                    .and_then(Value::as_i64)
                    .filter(|id| *id != 0),
                message_id,
                quote: value
                    .get("quote")
                    .and_then(|quote| quote.get("text"))
                    .map(formatted_text),
            },
            None => ReplyTarget::Unknown {
                raw_type: raw_type_of(value),
            },
        },
        Some("messageReplyToStory") => {
            let poster = value.get("story_poster_chat_id").and_then(Value::as_i64);
            let story = value.get("story_id").and_then(Value::as_i64);
            match (poster, story) {
                (Some(poster_chat_id), Some(story_id)) => ReplyTarget::Story {
                    poster_chat_id,
                    story_id,
                },
                _ => ReplyTarget::Unknown {
                    raw_type: raw_type_of(value),
                },
            }
        }
        _ => ReplyTarget::Unknown {
            raw_type: raw_type_of(value),
        },
    }
}

fn topic_ref(value: &Value) -> TopicRef {
    let known = match value.get("@type").and_then(Value::as_str) {
        Some("messageTopicForum") => value
            .get("forum_topic_id")
            .and_then(Value::as_i64)
            .map(|forum_topic_id| TopicRef::Forum { forum_topic_id }),
        Some("messageTopicDirectMessages") => value
            .get("direct_messages_chat_topic_id")
            .and_then(Value::as_i64)
            .map(|topic_id| TopicRef::DirectMessages { topic_id }),
        Some("messageTopicSavedMessages") => value
            .get("saved_messages_topic_id")
            .and_then(Value::as_i64)
            .map(|topic_id| TopicRef::SavedMessages { topic_id }),
        _ => None,
    };
    known.unwrap_or_else(|| TopicRef::Unknown {
        raw_type: raw_type_of(value),
    })
}

fn self_destruct_kind(value: &Value) -> SelfDestruct {
    match value.get("@type").and_then(Value::as_str) {
        Some("messageSelfDestructTypeTimer") => SelfDestruct::Timer {
            seconds: u32_field(value, "self_destruct_time").unwrap_or(0),
        },
        Some("messageSelfDestructTypeImmediately") => SelfDestruct::Immediate,
        _ => SelfDestruct::Unknown {
            raw_type: raw_type_of(value),
        },
    }
}

fn reaction(value: &Value) -> Option<Reaction> {
    let kind = value.get("type").map(reaction_kind)?;
    Some(Reaction {
        kind,
        count: u32_field(value, "total_count").unwrap_or(0),
        chosen: value
            .get("is_chosen")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn reaction_kind(value: &Value) -> ReactionKind {
    let known = match value.get("@type").and_then(Value::as_str) {
        Some("reactionTypeEmoji") => {
            value
                .get("emoji")
                .and_then(Value::as_str)
                .map(|emoji| ReactionKind::Emoji {
                    emoji: emoji.to_owned(),
                })
        }
        Some("reactionTypeCustomEmoji") => value
            .get("custom_emoji_id")
            .and_then(parse_int64)
            .map(|custom_emoji_id| ReactionKind::CustomEmoji { custom_emoji_id }),
        Some("reactionTypePaid") => Some(ReactionKind::Paid),
        _ => None,
    };
    known.unwrap_or_else(|| ReactionKind::Unknown {
        raw_type: raw_type_of(value),
    })
}

fn formatted_text(value: &Value) -> FormattedText {
    FormattedText {
        text: value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        entities: value
            .get("entities")
            .and_then(Value::as_array)
            .map(|entities| entities.iter().filter_map(text_entity).collect())
            .unwrap_or_default(),
    }
}

fn caption(content: &Value) -> FormattedText {
    content
        .get("caption")
        .map(formatted_text)
        .unwrap_or_default()
}

fn text_entity(value: &Value) -> Option<TextEntity> {
    let offset = u32_field(value, "offset")?;
    let length = u32_field(value, "length")?;
    let kind = entity_kind(value.get("type")?)?;
    Some(TextEntity {
        offset,
        length,
        kind,
    })
}

fn entity_kind(value: &Value) -> Option<TextEntityKind> {
    let raw_type = value.get("@type").and_then(Value::as_str)?;
    let unknown = || TextEntityKind::Unknown {
        raw_type: raw_type.to_owned(),
    };
    Some(match raw_type {
        "textEntityTypeMention" => TextEntityKind::Mention,
        "textEntityTypeHashtag" => TextEntityKind::Hashtag,
        "textEntityTypeCashtag" => TextEntityKind::Cashtag,
        "textEntityTypeBotCommand" => TextEntityKind::BotCommand,
        "textEntityTypeUrl" => TextEntityKind::Url,
        "textEntityTypeEmailAddress" => TextEntityKind::EmailAddress,
        "textEntityTypePhoneNumber" => TextEntityKind::PhoneNumber,
        "textEntityTypeBankCardNumber" => TextEntityKind::BankCardNumber,
        "textEntityTypeBold" => TextEntityKind::Bold,
        "textEntityTypeItalic" => TextEntityKind::Italic,
        "textEntityTypeUnderline" => TextEntityKind::Underline,
        "textEntityTypeStrikethrough" => TextEntityKind::Strikethrough,
        "textEntityTypeSpoiler" => TextEntityKind::Spoiler,
        "textEntityTypeCode" => TextEntityKind::Code,
        "textEntityTypePre" => TextEntityKind::Pre,
        "textEntityTypePreCode" => TextEntityKind::PreCode {
            language: value
                .get("language")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        "textEntityTypeBlockQuote" => TextEntityKind::BlockQuote,
        "textEntityTypeExpandableBlockQuote" => TextEntityKind::ExpandableBlockQuote,
        "textEntityTypeTextUrl" => match value.get("url").and_then(Value::as_str) {
            Some(url) => TextEntityKind::TextUrl {
                url: url.to_owned(),
            },
            None => unknown(),
        },
        "textEntityTypeMentionName" => match value.get("user_id").and_then(Value::as_i64) {
            Some(user_id) => TextEntityKind::MentionName { user_id },
            None => unknown(),
        },
        "textEntityTypeCustomEmoji" => match value.get("custom_emoji_id").and_then(parse_int64) {
            Some(custom_emoji_id) => TextEntityKind::CustomEmoji { custom_emoji_id },
            None => unknown(),
        },
        "textEntityTypeMediaTimestamp" => match u32_field(value, "media_timestamp") {
            Some(seconds) => TextEntityKind::MediaTimestamp { seconds },
            None => unknown(),
        },
        _ => unknown(),
    })
}

fn expired(kind: ExpiredKind) -> MessageContent {
    MessageContent::Expired { kind }
}

/// The known service subset; `None` sends the type to `Unsupported`.
fn service_action(raw_type: &str, content: &Value) -> Option<ServiceAction> {
    match raw_type {
        "messageBasicGroupChatCreate" => Some(ServiceAction::ChatCreated {
            title: string_field(content, "title"),
            member_user_ids: i64_list(content, "member_user_ids"),
        }),
        "messageSupergroupChatCreate" => Some(ServiceAction::ChatCreated {
            title: string_field(content, "title"),
            member_user_ids: Vec::new(),
        }),
        "messageChatChangeTitle" => Some(ServiceAction::TitleChanged {
            title: string_field(content, "title"),
        }),
        "messageChatChangePhoto" => Some(ServiceAction::PhotoChanged),
        "messageChatDeletePhoto" => Some(ServiceAction::PhotoDeleted),
        "messageChatAddMembers" => Some(ServiceAction::MembersAdded {
            user_ids: i64_list(content, "member_user_ids"),
        }),
        "messageChatJoinByLink" => Some(ServiceAction::JoinedByLink),
        "messageChatJoinByRequest" => Some(ServiceAction::JoinedByRequest),
        "messageChatDeleteMember" => content
            .get("user_id")
            .and_then(Value::as_i64)
            .map(|user_id| ServiceAction::MemberRemoved { user_id }),
        "messageChatUpgradeTo" => content
            .get("supergroup_id")
            .and_then(Value::as_i64)
            .map(|supergroup_id| ServiceAction::UpgradedToSupergroup { supergroup_id }),
        "messageChatUpgradeFrom" => {
            content
                .get("basic_group_id")
                .and_then(Value::as_i64)
                .map(|basic_group_id| ServiceAction::UpgradedFromBasicGroup {
                    title: string_field(content, "title"),
                    basic_group_id,
                })
        }
        "messagePinMessage" => content
            .get("message_id")
            .and_then(Value::as_i64)
            .map(|message_id| ServiceAction::MessagePinned { message_id }),
        "messageScreenshotTaken" => Some(ServiceAction::ScreenshotTaken),
        "messageChatSetMessageAutoDeleteTime" => Some(ServiceAction::AutoDeleteTimeChanged {
            seconds: u32_field(content, "message_auto_delete_time").unwrap_or(0),
        }),
        "messageForumTopicCreated" => Some(ServiceAction::TopicCreated {
            name: string_field(content, "name"),
        }),
        "messageForumTopicEdited" => Some(ServiceAction::TopicEdited {
            name: content
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
        }),
        "messageForumTopicIsClosedToggled" => Some(ServiceAction::TopicClosedToggled {
            closed: content
                .get("is_closed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "messageContactRegistered" => Some(ServiceAction::ContactRegistered),
        _ => None,
    }
}

// -- attachment assembly ----------------------------------------------------

/// The `file` object's facts: local locator, remote locators, best size.
struct FileRef {
    file_id: i32,
    remote_id: Option<String>,
    remote_unique_id: Option<String>,
    size: Option<u64>,
}

fn parse_file(value: &Value) -> Option<FileRef> {
    let file_id = value
        .get("id")
        .and_then(Value::as_i64)
        .and_then(|id| i32::try_from(id).ok())?;
    let size = value
        .get("size")
        .and_then(Value::as_i64)
        .filter(|size| *size > 0)
        .or_else(|| {
            value
                .get("expected_size")
                .and_then(Value::as_i64)
                .filter(|size| *size > 0)
        })
        .and_then(|size| u64::try_from(size).ok());
    let remote = value.get("remote");
    Some(FileRef {
        file_id,
        remote_id: remote.and_then(|remote| nonempty_string(remote, "id")),
        remote_unique_id: remote.and_then(|remote| nonempty_string(remote, "unique_id")),
        size,
    })
}

/// Build the descriptor for a single-file media object: `content[field]` is
/// the media object, whose file lives under its TDLib-conventional member
/// name (`video.video`, `document.document`, `sticker.sticker`, …).
fn media_attachment(
    content: &Value,
    field: &str,
    kind: AttachmentKind,
    protection: ProtectionFacts,
) -> Option<AttachmentDescriptor> {
    let media = content.get(field)?;
    let file = parse_file(media.get(file_member(kind))?)?;
    let (width, height) = match kind {
        // A video note is round; TDLib states its diameter as `length`.
        AttachmentKind::VideoNote => {
            let side = u32_field(media, "length");
            (side, side)
        }
        _ => (u32_field(media, "width"), u32_field(media, "height")),
    };
    Some(AttachmentDescriptor {
        kind,
        file_id: file.file_id,
        remote_id: file.remote_id,
        remote_unique_id: file.remote_unique_id,
        file_name: nonempty_string(media, "file_name"),
        mime_type: nonempty_string(media, "mime_type"),
        size: file.size,
        width,
        height,
        duration_secs: u32_field(media, "duration"),
        availability: protection.availability(),
    })
}

/// The member of each media object that holds its `file`.
fn file_member(kind: AttachmentKind) -> &'static str {
    match kind {
        AttachmentKind::Photo => "photo",
        AttachmentKind::Video => "video",
        AttachmentKind::Animation => "animation",
        AttachmentKind::Audio => "audio",
        AttachmentKind::Document => "document",
        AttachmentKind::VoiceNote => "voice",
        AttachmentKind::VideoNote => "video",
        AttachmentKind::Sticker => "sticker",
    }
}

/// A photo's attachment is its largest available size.
fn photo_attachment(
    photo: Option<&Value>,
    protection: ProtectionFacts,
) -> Option<AttachmentDescriptor> {
    let sizes = photo?.get("sizes").and_then(Value::as_array)?;
    let best = sizes.iter().max_by_key(|size| {
        let width = size.get("width").and_then(Value::as_i64).unwrap_or(0);
        let height = size.get("height").and_then(Value::as_i64).unwrap_or(0);
        width.saturating_mul(height)
    })?;
    let file = parse_file(best.get("photo")?)?;
    Some(AttachmentDescriptor {
        kind: AttachmentKind::Photo,
        file_id: file.file_id,
        remote_id: file.remote_id,
        remote_unique_id: file.remote_unique_id,
        file_name: None,
        mime_type: None,
        size: file.size,
        width: u32_field(best, "width"),
        height: u32_field(best, "height"),
        duration_secs: None,
        availability: protection.availability(),
    })
}

// -- small field helpers ----------------------------------------------------

fn u32_field(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)
        .and_then(Value::as_i64)
        .and_then(|number| u32::try_from(number).ok())
}

fn string_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn nonempty_string(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn i64_list(value: &Value, field: &str) -> Vec<i64> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OPEN: ProtectionFacts = ProtectionFacts {
        can_be_saved: true,
        self_destructing: false,
    };

    #[test]
    fn availability_derivation_fails_closed() {
        assert_eq!(OPEN.availability(), AttachmentAvailability::Fetchable);
        let restricted = ProtectionFacts {
            can_be_saved: false,
            self_destructing: false,
        };
        assert_eq!(
            restricted.availability(),
            AttachmentAvailability::Restricted
        );
        // View-once wins even when saving would be allowed, and even when it
        // would not: self-destructing media is never persisted (POL-4).
        for can_be_saved in [true, false] {
            let view_once = ProtectionFacts {
                can_be_saved,
                self_destructing: true,
            };
            assert_eq!(view_once.availability(), AttachmentAvailability::ViewOnce);
        }
    }

    #[test]
    fn sender_shapes_degrade_to_unknown_not_error() {
        assert_eq!(
            sender_ref(Some(&json!({"@type": "messageSenderUser", "user_id": 42}))),
            SenderRef::User { user_id: 42 }
        );
        assert_eq!(
            sender_ref(Some(&json!({"@type": "messageSenderChat", "chat_id": -1}))),
            SenderRef::Chat { chat_id: -1 }
        );
        // A known type missing its member, an unknown type, a typeless
        // object, and an absent field all degrade explicitly.
        assert_eq!(
            sender_ref(Some(&json!({"@type": "messageSenderUser"}))),
            SenderRef::Unknown {
                raw_type: Some("messageSenderUser".to_owned())
            }
        );
        assert_eq!(
            sender_ref(Some(&json!({"@type": "messageSenderRobot", "robot_id": 9}))),
            SenderRef::Unknown {
                raw_type: Some("messageSenderRobot".to_owned())
            }
        );
        assert_eq!(
            sender_ref(Some(&json!({}))),
            SenderRef::Unknown { raw_type: None }
        );
        assert_eq!(sender_ref(None), SenderRef::Unknown { raw_type: None });
    }

    #[test]
    fn entity_spans_survive_unknown_types_and_broken_entities_drop() {
        let text = formatted_text(&json!({
            "text": "hello world",
            "entities": [
                {"offset": 0, "length": 5, "type": {"@type": "textEntityTypeBold"}},
                {"offset": 6, "length": 5, "type": {"@type": "textEntityTypeGlitter"}},
                {"offset": 6, "length": 5, "type": {"@type": "textEntityTypeMentionName"}},
                {"offset": -3, "length": 5, "type": {"@type": "textEntityTypeBold"}},
                {"offset": 0, "length": 5},
                {"length": 5, "type": {"@type": "textEntityTypeBold"}}
            ]
        }));
        assert_eq!(text.text, "hello world");
        assert_eq!(
            text.entities,
            vec![
                TextEntity {
                    offset: 0,
                    length: 5,
                    kind: TextEntityKind::Bold
                },
                // Unknown type: span kept, renders as plain text.
                TextEntity {
                    offset: 6,
                    length: 5,
                    kind: TextEntityKind::Unknown {
                        raw_type: "textEntityTypeGlitter".to_owned()
                    }
                },
                // Known type missing its required member: kept as Unknown.
                TextEntity {
                    offset: 6,
                    length: 5,
                    kind: TextEntityKind::Unknown {
                        raw_type: "textEntityTypeMentionName".to_owned()
                    }
                },
                // The negative-offset, typeless and offsetless entities are
                // structurally broken decoration and dropped.
            ]
        );
    }

    #[test]
    fn custom_emoji_id_parses_the_int64_string_shape() {
        let kind = entity_kind(&json!({
            "@type": "textEntityTypeCustomEmoji",
            "custom_emoji_id": "5368324170671202286"
        }));
        assert_eq!(
            kind,
            Some(TextEntityKind::CustomEmoji {
                custom_emoji_id: 5368324170671202286
            })
        );
    }

    #[test]
    fn file_size_prefers_exact_over_expected() {
        let exact =
            parse_file(&json!({"id": 7, "size": 100, "expected_size": 90})).expect("file parses");
        assert_eq!(exact.size, Some(100));
        let expected =
            parse_file(&json!({"id": 7, "size": 0, "expected_size": 90})).expect("file parses");
        assert_eq!(expected.size, Some(90));
        let unknown =
            parse_file(&json!({"id": 7, "size": 0, "expected_size": 0})).expect("file parses");
        assert_eq!(unknown.size, None);
        assert!(parse_file(&json!({"size": 5})).is_none(), "no id, no file");
    }

    #[test]
    fn photo_attachment_picks_the_largest_size() {
        let photo = json!({"sizes": [
            {"type": "m", "width": 320, "height": 200,
             "photo": {"id": 1, "size": 4_000, "remote": {"id": "rm", "unique_id": "um"}}},
            {"type": "x", "width": 800, "height": 600,
             "photo": {"id": 2, "size": 90_000, "remote": {"id": "rx", "unique_id": "ux"}}},
            {"type": "s", "width": 90, "height": 60,
             "photo": {"id": 3, "size": 1_000, "remote": {"id": "rs", "unique_id": "us"}}}
        ]});
        let attachment = photo_attachment(Some(&photo), OPEN).expect("photo normalizes");
        assert_eq!(attachment.file_id, 2);
        assert_eq!(attachment.remote_unique_id.as_deref(), Some("ux"));
        assert_eq!(
            (attachment.width, attachment.height),
            (Some(800), Some(600))
        );
        assert_eq!(attachment.availability, AttachmentAvailability::Fetchable);
    }

    #[test]
    fn broken_reaction_drops_and_unknown_reaction_type_survives() {
        let reactions = normalize_reactions(&json!({
            "reactions": {"reactions": [
                {"type": {"@type": "reactionTypeEmoji", "emoji": "👍"},
                 "total_count": 3, "is_chosen": true},
                {"type": {"@type": "reactionTypePaid"}, "total_count": 1},
                {"type": {"@type": "reactionTypeGalactic"}, "total_count": 2},
                {"total_count": 9}
            ]}
        }));
        assert_eq!(
            reactions,
            vec![
                Reaction {
                    kind: ReactionKind::Emoji {
                        emoji: "👍".to_owned()
                    },
                    count: 3,
                    chosen: true
                },
                Reaction {
                    kind: ReactionKind::Paid,
                    count: 1,
                    chosen: false
                },
                Reaction {
                    kind: ReactionKind::Unknown {
                        raw_type: Some("reactionTypeGalactic".to_owned())
                    },
                    count: 2,
                    chosen: false
                },
            ]
        );
        assert_eq!(normalize_reactions(&json!({})), Vec::new());
    }

    #[test]
    fn content_without_a_type_is_malformed() {
        let err = normalize_content(&json!({"text": {"text": "hi"}}), OPEN)
            .expect_err("typeless content violates the protocol");
        assert!(matches!(err, MessageError::Malformed { .. }), "{err}");
    }

    #[test]
    fn known_type_with_broken_shape_degrades_to_unsupported_with_raw() {
        let content = json!({"@type": "messagePhoto", "caption": {"text": "no sizes"}});
        let normalized = normalize_content(&content, OPEN).expect("degrades, not errors");
        let MessageContent::Unsupported { content: preserved } = normalized else {
            panic!("expected Unsupported, got {normalized:?}");
        };
        assert_eq!(preserved.raw_type, "messagePhoto");
        assert_eq!(preserved.raw_schema_version, RAW_SCHEMA_VERSION);
        // The raw JSON round-trips to the exact wire content.
        let reparsed: Value = serde_json::from_str(&preserved.raw_json).expect("raw is JSON");
        assert_eq!(reparsed, content);
    }

    #[test]
    fn content_level_secret_flag_forces_view_once() {
        let content = json!({
            "@type": "messageVideoNote",
            "is_secret": true,
            "video_note": {
                "duration": 4, "length": 240,
                "video": {"id": 5, "size": 1000, "remote": {"id": "r", "unique_id": "u"}}
            }
        });
        let normalized = normalize_content(&content, OPEN).expect("normalizes");
        let MessageContent::VideoNote { attachment } = normalized else {
            panic!("expected VideoNote, got {normalized:?}");
        };
        assert_eq!(attachment.availability, AttachmentAvailability::ViewOnce);
        assert_eq!(
            (attachment.width, attachment.height),
            (Some(240), Some(240))
        );
    }
}
