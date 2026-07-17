//! Behavioural tests for the `messages.ndjson` renderer: exact-format anchors,
//! the POL-3 retention projections, field coverage, unavailable states,
//! determinism, and parseability.

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use gramdrive_model::identity::{
    AttachmentIndex, AttachmentKey, CanonicalKey, ContentHash, DocPartition, ItemKey, MessageId,
    MessageKey, SchemaFamily,
};
use gramdrive_render::ndjson::{
    self, Attachment, Availability, Deletion, MediaKind, MessageBody, MessageHistory, Revision,
    Sender, ServiceAction,
};
use support::{JsonValue, corpus, fixture_chat, parse, parse_lines};

use ndjson::{MessagesInput, RetentionMode};

fn empty_body() -> MessageBody {
    MessageBody {
        text: None,
        entities: Vec::new(),
        reply_to: None,
        thread_top: None,
        topic_id: None,
        album_id: None,
        reactions: Vec::new(),
        attachments: Vec::new(),
        service: None,
        protected: false,
    }
}

fn simple_message(text: &str) -> MessageHistory {
    let mut body = empty_body();
    body.text = Some(text.to_owned());
    MessageHistory {
        message_id: MessageId(1),
        sender: Some(Sender { id: 42 }),
        sent_at_ms: 1000,
        revisions: vec![Revision {
            event_seq: 1,
            edited_at_ms: None,
            observed_at_ms: 1000,
            payload_schema: SchemaFamily(1),
            body,
        }],
        deletion: None,
    }
}

const MESSAGE_FIELD_ORDER: &[&str] = &[
    "type",
    "message_id",
    "state",
    "revision",
    "sender",
    "date_ms",
    "edited_ms",
    "observed_ms",
    "text",
    "entities",
    "reply_to_message_id",
    "thread_top_message_id",
    "topic_id",
    "album_id",
    "reactions",
    "attachments",
    "service",
    "protected",
    "deleted_ms",
    "provenance",
];

const HEADER_FIELD_ORDER: &[&str] = &[
    "type",
    "schema",
    "schema_version",
    "renderer_version",
    "schema_family",
    "document_id",
    "account_id",
    "namespace_version",
    "chat_id",
    "partition",
    "retention_mode",
    "input_watermark_seq",
    "content_version",
];

#[test]
fn header_and_record_are_byte_exact() {
    let messages = vec![simple_message("hi")];
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Mirror,
        input_watermark_seq: 1,
        messages: &messages,
    };
    let document = ndjson::render_messages(&input);

    let doc_id = ndjson::document_id(fixture_chat(), DocPartition::Chat).text();
    let expected_header = format!(
        "{{\"type\":\"header\",\"schema\":\"gramdrive.messages\",\"schema_version\":1,\
\"renderer_version\":1,\"schema_family\":1,\"document_id\":\"{doc_id}\",\"account_id\":7,\
\"namespace_version\":2,\"chat_id\":-1001234567890,\"partition\":{{\"kind\":\"chat\"}},\
\"retention_mode\":\"mirror\",\"input_watermark_seq\":1,\
\"content_version\":\"gramdrive.messages/s1/r1/w1\"}}"
    );
    let expected_message = "{\"type\":\"message\",\"message_id\":1,\"state\":\"present\",\
\"revision\":0,\"sender\":{\"id\":42},\"date_ms\":1000,\"edited_ms\":null,\"observed_ms\":1000,\
\"text\":\"hi\",\"entities\":[],\"reply_to_message_id\":null,\"thread_top_message_id\":null,\
\"topic_id\":null,\"album_id\":null,\"reactions\":[],\"attachments\":[],\"service\":null,\
\"protected\":false,\"deleted_ms\":null,\"provenance\":{\"schema_family\":1,\"event_seq\":1}}";

    let expected = format!("{expected_header}\n{expected_message}\n");
    assert_eq!(document, expected);
}

#[test]
fn header_declares_frozen_schema_versions() {
    let messages: Vec<MessageHistory> = Vec::new();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Month {
            year: 2026,
            month: 7,
        },
        retention_mode: RetentionMode::Audit,
        input_watermark_seq: 99,
        messages: &messages,
    };
    let document = ndjson::render_messages(&input);
    let lines = parse_lines(&document);
    // An empty chat still produces exactly one line: the self-describing header.
    assert_eq!(lines.len(), 1);
    let header = &lines[0];

    assert_eq!(header.keys(), HEADER_FIELD_ORDER);
    assert_eq!(header.field("type").as_str(), Some("header"));
    assert_eq!(header.field("schema").as_str(), Some(ndjson::SCHEMA_ID));
    assert_eq!(
        header.field("schema_version").as_i64(),
        Some(i64::from(ndjson::SCHEMA_VERSION))
    );
    assert_eq!(
        header.field("renderer_version").as_i64(),
        Some(i64::from(ndjson::RENDERER_VERSION))
    );
    assert_eq!(
        header.field("schema_family").as_i64(),
        Some(i64::from(ndjson::MESSAGES_SCHEMA_FAMILY.0))
    );
    assert_eq!(header.field("retention_mode").as_str(), Some("audit"));
    assert_eq!(header.field("input_watermark_seq").as_i64(), Some(99));
    assert_eq!(
        header.field("content_version").as_str(),
        Some(ndjson::content_version_token(99).as_str())
    );
    // Month partition round-trips into the header.
    let partition = header.field("partition");
    assert_eq!(partition.field("kind").as_str(), Some("month"));
    assert_eq!(partition.field("year").as_i64(), Some(2026));
    assert_eq!(partition.field("month").as_i64(), Some(7));
}

#[test]
fn every_message_record_carries_the_full_field_set() {
    let messages = corpus();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Audit,
        input_watermark_seq: 13,
        messages: &messages,
    };
    let document = ndjson::render_messages(&input);
    for record in parse_lines(&document).iter().skip(1) {
        // Every message line has the same key set, in the same order — the
        // acceptance criterion that every field is represented or explicitly
        // null (SYNC-030, stable field order).
        assert_eq!(record.keys(), MESSAGE_FIELD_ORDER, "record: {record:?}");
    }
}

#[test]
fn mirror_shows_current_state_and_purges_deletions() {
    let messages = corpus();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Mirror,
        input_watermark_seq: 13,
        messages: &messages,
    };
    let records: Vec<JsonValue> = parse_lines(&ndjson::render_messages(&input))
        .into_iter()
        .skip(1)
        .collect();

    // The deleted message (102) is absent; every other message appears once.
    assert_eq!(records.len(), 9);
    let ids: Vec<i64> = records
        .iter()
        .map(|record| record.field("message_id").as_i64().unwrap())
        .collect();
    assert!(
        !ids.contains(&102),
        "deleted message must be purged in Mirror"
    );

    // The thrice-edited message (101) shows only its latest revision.
    let edited = records
        .iter()
        .find(|record| record.field("message_id").as_i64() == Some(101))
        .expect("message 101 present");
    assert_eq!(edited.field("state").as_str(), Some("present"));
    assert_eq!(edited.field("revision").as_i64(), Some(2));
    assert_eq!(edited.field("text").as_str(), Some("third"));
    assert!(edited.field("deleted_ms").is_null());
}

#[test]
fn audit_keeps_every_revision_and_a_content_preserving_tombstone() {
    let messages = corpus();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Audit,
        input_watermark_seq: 13,
        messages: &messages,
    };
    let records: Vec<JsonValue> = parse_lines(&ndjson::render_messages(&input))
        .into_iter()
        .skip(1)
        .collect();

    // 101 keeps all three revisions, in event_seq order, states superseded..present.
    let revisions_101: Vec<&JsonValue> = records
        .iter()
        .filter(|record| record.field("message_id").as_i64() == Some(101))
        .collect();
    assert_eq!(revisions_101.len(), 3);
    let states: Vec<&str> = revisions_101
        .iter()
        .map(|record| record.field("state").as_str().unwrap())
        .collect();
    assert_eq!(states, ["superseded", "superseded", "present"]);
    let texts: Vec<&str> = revisions_101
        .iter()
        .map(|record| record.field("text").as_str().unwrap())
        .collect();
    assert_eq!(texts, ["first", "second", "third"]);

    // 102 keeps the superseded original and a content-preserving tombstone.
    let revisions_102: Vec<&JsonValue> = records
        .iter()
        .filter(|record| record.field("message_id").as_i64() == Some(102))
        .collect();
    assert_eq!(revisions_102.len(), 2);
    assert_eq!(revisions_102[0].field("state").as_str(), Some("superseded"));
    let tombstone = revisions_102[1];
    assert_eq!(tombstone.field("state").as_str(), Some("deleted"));
    assert_eq!(
        tombstone.field("deleted_ms").as_i64(),
        Some(1_700_000_400_000)
    );
    // Content is preserved, not blanked (POL-3 Audit).
    assert_eq!(
        tombstone.field("text").as_str(),
        Some("edited then deleted")
    );
}

#[test]
fn missing_sender_renders_null() {
    let messages = corpus();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Mirror,
        input_watermark_seq: 13,
        messages: &messages,
    };
    let records = parse_lines(&ndjson::render_messages(&input));
    let channel_post = records
        .iter()
        .find(|record| record.get("message_id").and_then(JsonValue::as_i64) == Some(109))
        .expect("channel post present");
    assert!(channel_post.field("sender").is_null());
}

#[test]
fn attachment_states_are_explicit_and_content_is_gated_on_availability() {
    let mut body = empty_body();
    body.attachments = vec![
        Attachment {
            index: AttachmentIndex(0),
            media_kind: MediaKind::Photo,
            name: Some("ok.jpg".to_owned()),
            mime_type: Some("image/jpeg".to_owned()),
            size: Some(10),
            availability: Availability::Fetchable,
            content_hash: Some(ContentHash::Sha256([0x01; 32])),
            media_name: Some("ok.jpg".to_owned()),
        },
        Attachment {
            index: AttachmentIndex(1),
            media_kind: MediaKind::Document,
            name: None,
            mime_type: None,
            size: None,
            availability: Availability::Restricted,
            content_hash: None,
            media_name: None,
        },
    ];
    let messages = vec![MessageHistory {
        message_id: MessageId(200),
        sender: Some(Sender { id: 1 }),
        sent_at_ms: 5,
        revisions: vec![Revision {
            event_seq: 1,
            edited_at_ms: None,
            observed_at_ms: 5,
            payload_schema: SchemaFamily(1),
            body,
        }],
        deletion: None,
    }];
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Mirror,
        input_watermark_seq: 1,
        messages: &messages,
    };
    let records = parse_lines(&ndjson::render_messages(&input));
    let attachments = records[1].field("attachments").as_array().unwrap();
    assert_eq!(attachments.len(), 2);

    let downloaded = &attachments[0];
    assert_eq!(downloaded.field("availability").as_str(), Some("fetchable"));
    let content = downloaded.field("content");
    assert_eq!(content.field("hash_algo").as_str(), Some("sha256"));
    assert_eq!(
        content.field("hash_hex").as_str(),
        Some("0101010101010101010101010101010101010101010101010101010101010101")
    );

    let restricted = &attachments[1];
    assert_eq!(
        restricted.field("availability").as_str(),
        Some("restricted")
    );
    assert!(
        restricted.field("content").is_null(),
        "restricted content is never fetched (POL-4)"
    );

    // The attachment's stable link is its AttachmentKey identity (SYNC-032).
    let expected_id = ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
        message: MessageKey {
            chat: fixture_chat(),
            message_id: MessageId(200),
        },
        index: AttachmentIndex(0),
    }))
    .id()
    .text();
    assert_eq!(
        downloaded.field("item_id").as_str(),
        Some(expected_id.as_str())
    );
}

#[test]
fn service_action_with_a_list_renders() {
    let messages = corpus();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Mirror,
        input_watermark_seq: 13,
        messages: &messages,
    };
    let records = parse_lines(&ndjson::render_messages(&input));
    let service_message = records
        .iter()
        .find(|record| record.get("message_id").and_then(JsonValue::as_i64) == Some(108))
        .expect("service message present");
    let service = service_message.field("service");
    assert_eq!(service.field("action").as_str(), Some("members_added"));
    let ids: Vec<i64> = service
        .field("user_ids")
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_i64().unwrap())
        .collect();
    assert_eq!(ids, [777, 888, 999]);
}

#[test]
fn identical_input_is_byte_identical() {
    let messages = corpus();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Audit,
        input_watermark_seq: 13,
        messages: &messages,
    };
    assert_eq!(
        ndjson::render_messages(&input),
        ndjson::render_messages(&input)
    );
}

#[test]
fn revision_input_order_does_not_change_output() {
    // Two histories differing only in the order revisions are supplied.
    let make = |order: [i64; 3]| {
        let text = |seq: i64| match seq {
            2 => "first",
            5 => "second",
            _ => "third",
        };
        let revisions = order
            .into_iter()
            .map(|seq| {
                let mut body = empty_body();
                body.text = Some(text(seq).to_owned());
                Revision {
                    event_seq: seq,
                    edited_at_ms: if seq == 2 { None } else { Some(seq * 1000) },
                    observed_at_ms: seq * 10,
                    payload_schema: SchemaFamily(1),
                    body,
                }
            })
            .collect();
        vec![MessageHistory {
            message_id: MessageId(1),
            sender: Some(Sender { id: 1 }),
            sent_at_ms: 1,
            revisions,
            deletion: None,
        }]
    };
    let ascending = make([2, 5, 9]);
    let shuffled = make([9, 2, 5]);

    let ascending_doc = ndjson::render_messages(&MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Audit,
        input_watermark_seq: 9,
        messages: &ascending,
    });
    let shuffled_doc = ndjson::render_messages(&MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Audit,
        input_watermark_seq: 9,
        messages: &shuffled,
    });
    assert_eq!(ascending_doc, shuffled_doc);
}

#[test]
fn streaming_matches_the_string_form() {
    let messages = corpus();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Audit,
        input_watermark_seq: 13,
        messages: &messages,
    };
    let mut streamed = String::new();
    ndjson::write_messages(&mut streamed, &input).expect("string sink never fails");
    assert_eq!(streamed, ndjson::render_messages(&input));
}

#[test]
fn unknown_kinds_preserve_their_raw_tag() {
    let messages = corpus();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Audit,
        input_watermark_seq: 13,
        messages: &messages,
    };
    let document = ndjson::render_messages(&input);
    // The forward-compat escape hatches keep the record lossless.
    assert!(document.contains("\"raw_kind\":\"future_entity\""));
    assert!(document.contains("\"media_kind\":\"other\",\"media_kind_raw\":\"giveaway\""));
    assert!(document.contains("\"raw_action\":\"boost_applied\""));
    // And the whole thing is still valid NDJSON.
    for line in document.lines() {
        parse(line).expect("valid json line");
    }
}

#[test]
fn control_characters_in_text_never_split_a_record() {
    // A message body with an embedded newline and tab must not break the
    // one-record-per-line invariant that makes it NDJSON at all.
    let messages = vec![simple_message("line one\nline two\ttabbed")];
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Mirror,
        input_watermark_seq: 1,
        messages: &messages,
    };
    let document = ndjson::render_messages(&input);
    // Exactly two physical lines: header + the one message.
    assert_eq!(document.lines().count(), 2);
    let records = parse_lines(&document);
    // The text round-trips through the escape/parse with its controls intact.
    assert_eq!(
        records[1].field("text").as_str(),
        Some("line one\nline two\ttabbed")
    );
}

#[test]
fn deletion_helper_types_are_public() {
    // Compile-time proof the input contract is fully re-exported.
    let _ = Deletion { observed_at_ms: 1 };
    let _ = ServiceAction::MessagePinned {
        message_id: MessageId(1),
    };
}
