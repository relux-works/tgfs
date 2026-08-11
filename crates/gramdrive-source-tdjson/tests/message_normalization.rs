//! The PRD-022 message-normalization fixture corpus (TASK-260715-1ynmct):
//! realistic tdjson `message` wire objects — including the fields the
//! normalizer deliberately ignores and the int64-as-string encodings the C
//! JSON interface actually sends — driven through [`normalize_message`],
//! with the normalized records asserted whole.
//!
//! One fixture per PRD-022 class: identity/time/sender, text with
//! entities, captioned media of every PRD-030 v1 attachment kind, replies
//! (message, cross-chat with quote, story), topics, album grouping,
//! reactions, edits, service actions, POL-4 protection (restricted,
//! view-once, expired), and the explicit degradations — unknown content
//! preserved raw, peripheral unknowns kept typed, malformed identity a
//! typed error. Unknown content must degrade without crashing (the task's
//! acceptance criterion); every fixture here proves the no-panic path by
//! running.

// clippy.toml exempts test code keyed on `#[test]` functions; the fixture
// helpers below sit at module level in an integration-test binary. This file
// links into no product artifact (established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use serde_json::{Value, json};

use gramdrive_source_tdjson::{
    AttachmentAvailability, AttachmentKind, FormattedText, MessageContent, MessageError,
    MessageRecord, RAW_SCHEMA_VERSION, Reaction, ReactionKind, ReplyTarget, SelfDestruct,
    SenderRef, ServiceAction, TextEntity, TextEntityKind, TopicRef, normalize_message,
};

const CHAT: i64 = -1001234567890;
const USER: i64 = 111222333;

/// A realistic tdjson message envelope around `content`: the identity and
/// bookkeeping fields TDLib always sends, including several the normalizer
/// deliberately ignores (additive-schema tolerance).
fn wire_message(id: i64, content: Value) -> Value {
    json!({
        "@type": "message",
        "id": id,
        "sender_id": {"@type": "messageSenderUser", "user_id": USER},
        "chat_id": CHAT,
        "is_outgoing": false,
        "can_be_saved": true,
        "date": 1_752_800_000,
        "author_signature": "",
        "content": content
    })
}

fn plain_text(text: &str) -> FormattedText {
    FormattedText {
        text: text.to_owned(),
        entities: Vec::new(),
    }
}

fn normalized(message: &Value) -> MessageRecord {
    normalize_message(message).expect("fixture normalizes")
}

// ---------------------------------------------------------------------------
// Identity, time, sender (PRD-022: identity, time, sender)
// ---------------------------------------------------------------------------

#[test]
fn identity_time_and_sender_map_verbatim() {
    let record = normalized(&wire_message(
        105906176,
        json!({
            "@type": "messageText",
            "text": {"@type": "formattedText", "text": "hi", "entities": []}
        }),
    ));
    assert_eq!(record.chat_id, CHAT);
    assert_eq!(record.message_id, 105906176);
    assert_eq!(record.sender, SenderRef::User { user_id: USER });
    assert_eq!(record.sent_at_ms, 1_752_800_000_000);
    assert_eq!(record.edited_at_ms, None);
    assert_eq!(record.reply, None);
    assert_eq!(record.topic, None);
    assert_eq!(record.album_id, None);
    assert_eq!(record.reactions, Vec::new());
    assert!(record.can_be_saved);
    assert_eq!(record.self_destruct, None);
    assert_eq!(
        record.content,
        MessageContent::Text {
            text: plain_text("hi")
        }
    );
}

#[test]
fn chat_sender_and_channel_post_map_to_a_chat_ref() {
    let mut message = wire_message(
        2,
        json!({
            "@type": "messageText",
            "text": {"@type": "formattedText", "text": "post", "entities": []}
        }),
    );
    message["sender_id"] = json!({"@type": "messageSenderChat", "chat_id": CHAT});
    message["is_channel_post"] = json!(true);
    assert_eq!(
        normalized(&message).sender,
        SenderRef::Chat { chat_id: CHAT }
    );
}

// ---------------------------------------------------------------------------
// Text and entities (PRD-022: text/caption entities)
// ---------------------------------------------------------------------------

#[test]
fn text_with_the_entity_vocabulary() {
    let record = normalized(&wire_message(
        3,
        json!({
            "@type": "messageText",
            "text": {
                "@type": "formattedText",
                "text": "bold code @user link emoji",
                "entities": [
                    {"@type": "textEntity", "offset": 0, "length": 4,
                     "type": {"@type": "textEntityTypeBold"}},
                    {"@type": "textEntity", "offset": 5, "length": 4,
                     "type": {"@type": "textEntityTypePreCode", "language": "rust"}},
                    {"@type": "textEntity", "offset": 10, "length": 5,
                     "type": {"@type": "textEntityTypeMentionName", "user_id": USER}},
                    {"@type": "textEntity", "offset": 16, "length": 4,
                     "type": {"@type": "textEntityTypeTextUrl", "url": "https://example.com"}},
                    {"@type": "textEntity", "offset": 21, "length": 5,
                     "type": {"@type": "textEntityTypeCustomEmoji",
                              "custom_emoji_id": "5368324170671202286"}}
                ]
            }
        }),
    ));
    let MessageContent::Text { text } = record.content else {
        panic!("expected text content");
    };
    assert_eq!(
        text.entities,
        vec![
            TextEntity {
                offset: 0,
                length: 4,
                kind: TextEntityKind::Bold
            },
            TextEntity {
                offset: 5,
                length: 4,
                kind: TextEntityKind::PreCode {
                    language: "rust".to_owned()
                }
            },
            TextEntity {
                offset: 10,
                length: 5,
                kind: TextEntityKind::MentionName { user_id: USER }
            },
            TextEntity {
                offset: 16,
                length: 4,
                kind: TextEntityKind::TextUrl {
                    url: "https://example.com".to_owned()
                }
            },
            TextEntity {
                offset: 21,
                length: 5,
                kind: TextEntityKind::CustomEmoji {
                    custom_emoji_id: 5368324170671202286
                }
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// Captioned media and attachment descriptors (PRD-022/PRD-030/PRD-032)
// ---------------------------------------------------------------------------

#[test]
fn document_descriptor_carries_original_metadata() {
    let record = normalized(&wire_message(
        4,
        json!({
            "@type": "messageDocument",
            "document": {
                "@type": "document",
                "file_name": "report.pdf",
                "mime_type": "application/pdf",
                "document": {
                    "@type": "file", "id": 517, "size": 2_048_576, "expected_size": 2_048_576,
                    "local": {"@type": "localFile", "path": "", "can_be_downloaded": true},
                    "remote": {"@type": "remoteFile", "id": "BQACAgIAAx0", "unique_id": "AgADdoc"}
                }
            },
            "caption": {"@type": "formattedText", "text": "the report", "entities": []}
        }),
    ));
    let MessageContent::Document {
        caption,
        attachment,
    } = record.content
    else {
        panic!("expected document content");
    };
    assert_eq!(caption, plain_text("the report"));
    assert_eq!(attachment.kind, AttachmentKind::Document);
    assert_eq!(attachment.file_id, Some(517));
    assert_eq!(attachment.remote_id.as_deref(), Some("BQACAgIAAx0"));
    assert_eq!(attachment.remote_unique_id.as_deref(), Some("AgADdoc"));
    assert_eq!(attachment.file_name.as_deref(), Some("report.pdf"));
    assert_eq!(attachment.mime_type.as_deref(), Some("application/pdf"));
    assert_eq!(attachment.size, Some(2_048_576));
    assert_eq!(attachment.availability, AttachmentAvailability::Fetchable);
}

#[test]
fn every_media_class_normalizes_with_its_descriptor() {
    let file = |id: i64, unique: &str| {
        json!({"@type": "file", "id": id, "size": 1000, "expected_size": 1000,
               "remote": {"@type": "remoteFile", "id": format!("r{unique}"), "unique_id": unique}})
    };
    let cases = vec![
        (
            json!({"@type": "messageVideo",
                   "video": {"@type": "video", "duration": 30, "width": 1280, "height": 720,
                             "file_name": "clip.mp4", "mime_type": "video/mp4",
                             "video": file(1, "vid")},
                   "caption": {"@type": "formattedText", "text": "clip", "entities": []}}),
            AttachmentKind::Video,
        ),
        (
            json!({"@type": "messageAnimation",
                   "animation": {"@type": "animation", "duration": 3, "width": 320, "height": 240,
                                 "file_name": "loop.mp4", "mime_type": "video/mp4",
                                 "animation": file(2, "anim")},
                   "caption": {"@type": "formattedText", "text": "", "entities": []}}),
            AttachmentKind::Animation,
        ),
        (
            json!({"@type": "messageAudio",
                   "audio": {"@type": "audio", "duration": 200, "title": "Song",
                             "performer": "Band", "file_name": "song.mp3",
                             "mime_type": "audio/mpeg", "audio": file(3, "aud")},
                   "caption": {"@type": "formattedText", "text": "", "entities": []}}),
            AttachmentKind::Audio,
        ),
        (
            json!({"@type": "messageVoiceNote",
                   "voice_note": {"@type": "voiceNote", "duration": 7, "waveform": "",
                                  "mime_type": "audio/ogg", "voice": file(4, "voice")},
                   "caption": {"@type": "formattedText", "text": "", "entities": []}}),
            AttachmentKind::VoiceNote,
        ),
        (
            json!({"@type": "messageVideoNote",
                   "video_note": {"@type": "videoNote", "duration": 5, "length": 384,
                                  "video": file(5, "note")}}),
            AttachmentKind::VideoNote,
        ),
        (
            json!({"@type": "messageSticker",
                   "sticker": {"@type": "sticker", "id": "88", "set_id": "99",
                               "width": 512, "height": 512, "emoji": "😀",
                               "sticker": file(6, "stick")}}),
            AttachmentKind::Sticker,
        ),
    ];
    for (content, kind) in cases {
        let record = normalized(&wire_message(5, content));
        let attachment = match record.content {
            MessageContent::Video { attachment, .. }
            | MessageContent::Animation { attachment, .. }
            | MessageContent::Audio { attachment, .. }
            | MessageContent::VoiceNote { attachment, .. }
            | MessageContent::VideoNote { attachment }
            | MessageContent::Sticker { attachment, .. } => attachment,
            other => panic!("expected media content for {kind:?}, got {other:?}"),
        };
        assert_eq!(attachment.kind, kind);
        assert_eq!(attachment.size, Some(1000));
        assert!(
            attachment.remote_unique_id.is_some(),
            "{kind:?} keeps its dedup key"
        );
        assert_eq!(attachment.availability, AttachmentAvailability::Fetchable);
    }
}

#[test]
fn sticker_keeps_its_emoji() {
    let record = normalized(&wire_message(
        6,
        json!({
            "@type": "messageSticker",
            "sticker": {"@type": "sticker", "width": 512, "height": 512, "emoji": "🎉",
                        "sticker": {"@type": "file", "id": 7, "size": 100,
                                    "remote": {"id": "r", "unique_id": "u"}}}
        }),
    ));
    let MessageContent::Sticker { emoji, .. } = record.content else {
        panic!("expected sticker content");
    };
    assert_eq!(emoji, "🎉");
}

// ---------------------------------------------------------------------------
// Albums (PRD-022: albums)
// ---------------------------------------------------------------------------

#[test]
fn album_members_share_the_grouping_key() {
    let photo = |id: i64, file_id: i64| {
        let mut message = wire_message(
            id,
            json!({
                "@type": "messagePhoto",
                "photo": {"@type": "photo", "sizes": [
                    {"@type": "photoSize", "type": "x", "width": 1280, "height": 960,
                     "photo": {"@type": "file", "id": file_id, "size": 200_000,
                               "remote": {"id": "r", "unique_id": format!("u{file_id}")}}}
                ]},
                "caption": {"@type": "formattedText", "text": "", "entities": []}
            }),
        );
        // The C JSON interface serializes the int64 album id as a string.
        message["media_album_id"] = json!("13188342471575478");
        message
    };
    let first = normalized(&photo(10, 71));
    let second = normalized(&photo(11, 72));
    assert_eq!(first.album_id, Some(13188342471575478));
    assert_eq!(first.album_id, second.album_id, "one album, one key");
    assert!(matches!(first.content, MessageContent::Photo { .. }));
}

// ---------------------------------------------------------------------------
// Replies and topics (PRD-022: replies, topics)
// ---------------------------------------------------------------------------

#[test]
fn reply_shapes_normalize_and_unknown_degrades() {
    let text = json!({"@type": "messageText",
                      "text": {"@type": "formattedText", "text": "re", "entities": []}});
    let mut same_chat = wire_message(20, text.clone());
    same_chat["reply_to"] = json!({
        "@type": "messageReplyToMessage", "chat_id": 0, "message_id": 19,
        "quote": {"@type": "textQuote", "position": 2,
                  "text": {"@type": "formattedText", "text": "quoted", "entities": []}}
    });
    assert_eq!(
        normalized(&same_chat).reply,
        Some(ReplyTarget::Message {
            chat_id: None,
            message_id: 19,
            quote: Some(plain_text("quoted"))
        })
    );

    let mut cross_chat = wire_message(21, text.clone());
    cross_chat["reply_to"] = json!({
        "@type": "messageReplyToMessage", "chat_id": -100999, "message_id": 5
    });
    assert_eq!(
        normalized(&cross_chat).reply,
        Some(ReplyTarget::Message {
            chat_id: Some(-100999),
            message_id: 5,
            quote: None
        })
    );

    let mut story = wire_message(22, text.clone());
    story["reply_to"] = json!({
        "@type": "messageReplyToStory", "story_poster_chat_id": 777, "story_id": 42
    });
    assert_eq!(
        normalized(&story).reply,
        Some(ReplyTarget::Story {
            poster_chat_id: 777,
            story_id: 42
        })
    );

    let mut future = wire_message(23, text);
    future["reply_to"] = json!({"@type": "messageReplyToChecklistTask", "task_id": 3});
    assert_eq!(
        normalized(&future).reply,
        Some(ReplyTarget::Unknown {
            raw_type: Some("messageReplyToChecklistTask".to_owned())
        })
    );
}

#[test]
fn topic_shapes_normalize_and_unknown_degrades() {
    let text = json!({"@type": "messageText",
                      "text": {"@type": "formattedText", "text": "t", "entities": []}});
    let with_topic = |topic: Value| {
        let mut message = wire_message(30, text.clone());
        message["topic_id"] = topic;
        normalized(&message).topic
    };
    assert_eq!(
        with_topic(json!({"@type": "messageTopicForum", "forum_topic_id": 105906176})),
        Some(TopicRef::Forum {
            forum_topic_id: 105906176
        })
    );
    assert_eq!(
        with_topic(json!({"@type": "messageTopicDirectMessages",
                          "direct_messages_chat_topic_id": 12})),
        Some(TopicRef::DirectMessages { topic_id: 12 })
    );
    assert_eq!(
        with_topic(json!({"@type": "messageTopicSavedMessages",
                          "saved_messages_topic_id": 9})),
        Some(TopicRef::SavedMessages { topic_id: 9 })
    );
    assert_eq!(
        with_topic(json!({"@type": "messageTopicGalactic", "galaxy_id": 1})),
        Some(TopicRef::Unknown {
            raw_type: Some("messageTopicGalactic".to_owned())
        })
    );
}

// ---------------------------------------------------------------------------
// Reactions and edits (PRD-022: reactions, edits)
// ---------------------------------------------------------------------------

#[test]
fn reactions_and_edit_revision_map() {
    let mut message = wire_message(
        40,
        json!({
            "@type": "messageText",
            "text": {"@type": "formattedText", "text": "edited", "entities": []}
        }),
    );
    message["edit_date"] = json!(1_752_800_100);
    message["interaction_info"] = json!({
        "@type": "messageInteractionInfo",
        "view_count": 15,
        "forward_count": 1,
        "reactions": {"@type": "messageReactions", "reactions": [
            {"@type": "messageReaction",
             "type": {"@type": "reactionTypeEmoji", "emoji": "🔥"},
             "total_count": 4, "is_chosen": true},
            {"@type": "messageReaction",
             "type": {"@type": "reactionTypeCustomEmoji",
                      "custom_emoji_id": "5445284980978621387"},
             "total_count": 2}
        ]}
    });
    let record = normalized(&message);
    assert_eq!(record.edited_at_ms, Some(1_752_800_100_000));
    assert_eq!(
        record.reactions,
        vec![
            Reaction {
                kind: ReactionKind::Emoji {
                    emoji: "🔥".to_owned()
                },
                count: 4,
                chosen: true
            },
            Reaction {
                kind: ReactionKind::CustomEmoji {
                    custom_emoji_id: 5445284980978621387
                },
                count: 2,
                chosen: false
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// Service actions (PRD-022: service actions)
// ---------------------------------------------------------------------------

#[test]
fn the_modeled_service_actions_normalize() {
    let cases = vec![
        (
            json!({"@type": "messageBasicGroupChatCreate", "title": "Crew",
                   "member_user_ids": [1, 2, 3]}),
            ServiceAction::ChatCreated {
                title: "Crew".to_owned(),
                member_user_ids: vec![1, 2, 3],
            },
        ),
        (
            json!({"@type": "messageSupergroupChatCreate", "title": "Big Crew"}),
            ServiceAction::ChatCreated {
                title: "Big Crew".to_owned(),
                member_user_ids: Vec::new(),
            },
        ),
        (
            json!({"@type": "messageChatChangeTitle", "title": "New Name"}),
            ServiceAction::TitleChanged {
                title: "New Name".to_owned(),
            },
        ),
        (
            json!({"@type": "messageChatChangePhoto", "photo": {"@type": "chatPhoto"}}),
            ServiceAction::PhotoChanged,
        ),
        (
            json!({"@type": "messageChatDeletePhoto"}),
            ServiceAction::PhotoDeleted,
        ),
        (
            json!({"@type": "messageChatAddMembers", "member_user_ids": [USER]}),
            ServiceAction::MembersAdded {
                user_ids: vec![USER],
            },
        ),
        (
            json!({"@type": "messageChatJoinByLink"}),
            ServiceAction::JoinedByLink,
        ),
        (
            json!({"@type": "messageChatJoinByRequest"}),
            ServiceAction::JoinedByRequest,
        ),
        (
            json!({"@type": "messageChatDeleteMember", "user_id": USER}),
            ServiceAction::MemberRemoved { user_id: USER },
        ),
        (
            json!({"@type": "messageChatUpgradeTo", "supergroup_id": 555}),
            ServiceAction::UpgradedToSupergroup { supergroup_id: 555 },
        ),
        (
            json!({"@type": "messageChatUpgradeFrom", "title": "Old", "basic_group_id": 44}),
            ServiceAction::UpgradedFromBasicGroup {
                title: "Old".to_owned(),
                basic_group_id: 44,
            },
        ),
        (
            json!({"@type": "messagePinMessage", "message_id": 105906176}),
            ServiceAction::MessagePinned {
                message_id: 105906176,
            },
        ),
        (
            json!({"@type": "messageScreenshotTaken"}),
            ServiceAction::ScreenshotTaken,
        ),
        (
            json!({"@type": "messageChatSetMessageAutoDeleteTime",
                   "message_auto_delete_time": 86400, "from_user_id": USER}),
            ServiceAction::AutoDeleteTimeChanged { seconds: 86400 },
        ),
        (
            json!({"@type": "messageForumTopicCreated", "name": "General",
                   "icon": {"@type": "forumTopicIcon", "color": 7322096}}),
            ServiceAction::TopicCreated {
                name: "General".to_owned(),
            },
        ),
        (
            json!({"@type": "messageForumTopicEdited", "name": "Renamed"}),
            ServiceAction::TopicEdited {
                name: Some("Renamed".to_owned()),
            },
        ),
        (
            json!({"@type": "messageForumTopicEdited",
                   "edit_icon_custom_emoji_id": true, "icon_custom_emoji_id": "5"}),
            ServiceAction::TopicEdited { name: None },
        ),
        (
            json!({"@type": "messageForumTopicIsClosedToggled", "is_closed": true}),
            ServiceAction::TopicClosedToggled { closed: true },
        ),
        (
            json!({"@type": "messageContactRegistered"}),
            ServiceAction::ContactRegistered,
        ),
    ];
    for (content, expected) in cases {
        let record = normalized(&wire_message(50, content));
        assert_eq!(record.content, MessageContent::Service { action: expected });
    }
}

// ---------------------------------------------------------------------------
// Protection (POL-4: can_be_saved, view-once, expired)
// ---------------------------------------------------------------------------

#[test]
fn save_restricted_media_is_a_restricted_placeholder() {
    let mut message = wire_message(
        60,
        json!({
            "@type": "messagePhoto",
            "photo": {"@type": "photo", "sizes": [
                {"@type": "photoSize", "type": "x", "width": 800, "height": 600,
                 "photo": {"@type": "file", "id": 9, "size": 50_000,
                           "remote": {"id": "r", "unique_id": "u"}}}
            ]},
            "caption": {"@type": "formattedText", "text": "protected", "entities": []}
        }),
    );
    // tdjson omits false booleans; an absent can_be_saved *is* the wire
    // shape of a protected message.
    message.as_object_mut().unwrap().remove("can_be_saved");
    let record = normalized(&message);
    assert!(!record.can_be_saved);
    let MessageContent::Photo {
        caption,
        attachment,
    } = record.content
    else {
        panic!("expected photo content");
    };
    assert_eq!(caption.text, "");
    assert!(caption.entities.is_empty());
    assert_eq!(attachment.availability, AttachmentAvailability::Restricted);
    assert_eq!(attachment.file_id, None);
    assert_eq!(attachment.remote_id, None);
    assert_eq!(attachment.remote_unique_id, None);
    assert_eq!(attachment.thumbnail, None);
    assert_eq!(attachment.minithumbnail, None);
}

#[test]
fn self_destructing_media_is_view_once_even_when_saveable() {
    let with_self_destruct = |self_destruct: Value| {
        let mut message = wire_message(
            61,
            json!({
                "@type": "messagePhoto",
                "photo": {"@type": "photo", "sizes": [
                    {"@type": "photoSize", "type": "x", "width": 100, "height": 100,
                     "photo": {"@type": "file", "id": 9, "size": 1000,
                               "remote": {"id": "r", "unique_id": "u"}}}
                ]},
                "caption": {"@type": "formattedText", "text": "", "entities": []}
            }),
        );
        message["self_destruct_type"] = self_destruct;
        normalized(&message)
    };
    let timer = with_self_destruct(json!({
        "@type": "messageSelfDestructTypeTimer", "self_destruct_time": 30
    }));
    assert_eq!(
        timer.self_destruct,
        Some(SelfDestruct::Timer { seconds: 30 })
    );
    let immediate = with_self_destruct(json!({
        "@type": "messageSelfDestructTypeImmediately"
    }));
    assert_eq!(immediate.self_destruct, Some(SelfDestruct::Immediate));
    // A self-destruct flavor this build does not know still fails closed.
    let unknown = with_self_destruct(json!({"@type": "messageSelfDestructTypeQuantum"}));
    assert_eq!(
        unknown.self_destruct,
        Some(SelfDestruct::Unknown {
            raw_type: Some("messageSelfDestructTypeQuantum".to_owned())
        })
    );
    for record in [timer, immediate, unknown] {
        assert!(record.can_be_saved, "fixture allows saving");
        let MessageContent::Photo {
            caption,
            attachment,
        } = record.content
        else {
            panic!("expected photo content");
        };
        assert_eq!(caption.text, "");
        assert_eq!(attachment.availability, AttachmentAvailability::ViewOnce);
        assert_eq!(attachment.file_id, None);
        assert_eq!(attachment.remote_id, None);
        assert_eq!(attachment.remote_unique_id, None);
    }
}

#[test]
fn expired_media_is_explicitly_unavailable() {
    use gramdrive_source_tdjson::ExpiredKind;
    let cases = [
        ("messageExpiredPhoto", ExpiredKind::Photo),
        ("messageExpiredVideo", ExpiredKind::Video),
        ("messageExpiredVideoNote", ExpiredKind::VideoNote),
        ("messageExpiredVoiceNote", ExpiredKind::VoiceNote),
    ];
    for (raw_type, kind) in cases {
        let record = normalized(&wire_message(62, json!({"@type": raw_type})));
        let MessageContent::Expired {
            kind: actual_kind,
            attachment,
        } = record.content
        else {
            panic!("expected expired attachment placeholder");
        };
        assert_eq!(actual_kind, kind);
        assert_eq!(attachment.file_id, None);
        assert_eq!(attachment.availability, AttachmentAvailability::Unavailable);
    }
}

// ---------------------------------------------------------------------------
// Explicit degradation (PRD-024: unknown content, malformed objects)
// ---------------------------------------------------------------------------

#[test]
fn unknown_content_degrades_to_a_typed_record_with_raw_preserved() {
    let content = json!({
        "@type": "messageHolographicCall",
        "duration": 12,
        "participants": [{"@type": "messageSenderUser", "user_id": USER}]
    });
    let record = normalized(&wire_message(70, content.clone()));
    let MessageContent::Unsupported { content: preserved } = record.content else {
        panic!("expected Unsupported, got {:?}", record.content);
    };
    assert_eq!(preserved.raw_type, "messageHolographicCall");
    assert_eq!(preserved.raw_schema_version, RAW_SCHEMA_VERSION);
    let reparsed: Value = serde_json::from_str(&preserved.raw_json).expect("raw is JSON");
    assert_eq!(reparsed, content, "raw preservation is lossless");
}

#[test]
fn unmodeled_v1_content_classes_degrade_the_same_way() {
    // Known-to-Telegram but outside the PRD-022/PRD-030 v1 classes: polls,
    // locations, contacts. Explicitly unsupported, raw kept, no crash.
    for raw_type in ["messagePoll", "messageLocation", "messageContact"] {
        let record = normalized(&wire_message(71, json!({"@type": raw_type})));
        let MessageContent::Unsupported { content } = record.content else {
            panic!("expected Unsupported for {raw_type}");
        };
        assert_eq!(content.raw_type, raw_type);
    }
}

#[test]
fn malformed_identity_is_a_typed_error_never_a_guess() {
    let text = json!({"@type": "messageText",
                      "text": {"@type": "formattedText", "text": "x", "entities": []}});
    let mut no_id = wire_message(1, text.clone());
    no_id.as_object_mut().unwrap().remove("id");
    let mut no_chat = wire_message(1, text.clone());
    no_chat.as_object_mut().unwrap().remove("chat_id");
    let mut no_date = wire_message(1, text.clone());
    no_date.as_object_mut().unwrap().remove("date");
    let mut no_content = wire_message(1, text.clone());
    no_content.as_object_mut().unwrap().remove("content");
    let mut typeless_content = wire_message(1, text);
    typeless_content["content"] = json!({"text": "orphan"});
    for message in [no_id, no_chat, no_date, no_content, typeless_content] {
        let err = normalize_message(&message).expect_err("identity is strict");
        assert!(matches!(err, MessageError::Malformed { .. }), "{err}");
    }
}
