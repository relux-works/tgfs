//! Behavioural tests for the monthly Markdown renderer: exact-format anchors,
//! the explicit timezone, injection safety, the POL-3 retention projections,
//! attachment links and unavailable states, service messages, and determinism.

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use gramdrive_model::identity::{
    AttachmentIndex, CanonicalKey, ContentHash, DocFormat, DocPartition, ItemKey, MessageId,
    SchemaFamily,
};
use gramdrive_render::markdown::{
    self, Attachment, AttachmentFidelity, Availability, Deletion, DisplayTimeZone, MarkdownInput,
    MediaKind, MessageBody, MessageHistory, RetentionMode, Revision, Sender, ServiceAction,
    TelegramRepresentation, UtcOffset,
};
use support::{corpus, fixture_chat};

/// The corpus reference instant, 2023-11-14T22:13:20Z.
const REFERENCE_MS: i64 = 1_700_000_000_000;

fn november_2023() -> DocPartition {
    DocPartition::Month {
        year: 2023,
        month: 11,
    }
}

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

fn message_with(id: i64, sent_at_ms: i64, body: MessageBody) -> MessageHistory {
    MessageHistory {
        message_id: MessageId(id),
        sender: Some(Sender { id: 555 }),
        sent_at_ms,
        revisions: vec![Revision {
            event_seq: 1,
            edited_at_ms: None,
            observed_at_ms: sent_at_ms,
            payload_schema: SchemaFamily(1),
            body,
        }],
        deletion: None,
    }
}

fn text_message(id: i64, sent_at_ms: i64, text: &str) -> MessageHistory {
    let mut body = empty_body();
    body.text = Some(text.to_owned());
    message_with(id, sent_at_ms, body)
}

fn render(mode: RetentionMode, timezone: UtcOffset, messages: &[MessageHistory]) -> String {
    let timezone = DisplayTimeZone::fixed(timezone);
    markdown::render_transcript(&MarkdownInput {
        chat: fixture_chat(),
        partition: november_2023(),
        retention_mode: mode,
        timezone: &timezone,
        input_watermark_seq: 13,
        render_generation: 0,
        messages,
    })
}

#[test]
fn front_matter_and_title_are_byte_exact() {
    let messages = vec![text_message(1, REFERENCE_MS, "hi")];
    let document = render(RetentionMode::Mirror, UtcOffset::UTC, &messages);
    let doc_id = markdown::document_id(fixture_chat(), november_2023()).text();
    let expected_head = format!(
        "---\n\
schema: gramdrive.transcript\n\
schema_version: 2\n\
renderer_version: 4\n\
schema_family: 1\n\
document_id: {doc_id}\n\
account_id: 7\n\
namespace_version: 2\n\
chat_id: -1001234567890\n\
partition: 2023-11\n\
retention_mode: mirror\n\
timezone: UTC\n\
input_watermark_seq: 13\n\
render_generation: 0\n\
content_version: gramdrive.transcript/s2/r4/w13/g0/retention-mirror/tz-UTC\n\
---\n\n\
# Chat -1001234567890\n\n\
_Transcript for 2023-11 · times in UTC · retention: mirror._\n\n\
## 2023-11-14\n\n\
**22:13:20 · user 555 · #1**\n\n\
hi\n"
    );
    assert_eq!(document, expected_head);
}

#[test]
fn empty_month_still_self_describes() {
    let document = render(RetentionMode::Mirror, UtcOffset::UTC, &[]);
    // Front matter, title, and subtitle, then nothing — and a single trailing
    // newline. A reader can still identify the document.
    assert!(document.starts_with("---\nschema: gramdrive.transcript\n"));
    assert!(document.contains("\nchat_id: -1001234567890\n"));
    assert!(document.ends_with("retention: mirror._\n"));
    assert!(
        !document.contains("## "),
        "no day heading for an empty month"
    );
}

#[test]
fn protected_latest_revision_suppresses_all_current_and_audit_plaintext() {
    let mut historical = empty_body();
    historical.text = Some("audit-secret-before-protection".to_owned());
    historical.service = Some(ServiceAction::Other {
        kind: "audit-secret-service".to_owned(),
    });
    let mut protected = empty_body();
    protected.protected = true;
    protected.text = Some("current-secret".to_owned());
    protected.reply_to = Some(MessageId(99));
    protected.topic_id = Some(77);
    protected.album_id = Some(88);
    protected.attachments = vec![Attachment {
        index: AttachmentIndex(0),
        media_kind: MediaKind::Document,
        telegram_representation: TelegramRepresentation::OriginalDocument,
        fidelity: AttachmentFidelity::Original,
        source_name: Some("secret-filename.pdf".to_owned()),
        mime_type: Some("secret/mime".to_owned()),
        exact_size: Some(42),
        availability: Availability::Restricted,
        content_hash: None,
        media_name: Some("secret-media-name.pdf".to_owned()),
    }];
    protected.service = Some(ServiceAction::Other {
        kind: "current-secret-service".to_owned(),
    });
    let messages = vec![MessageHistory {
        message_id: MessageId(7),
        sender: Some(Sender { id: 42 }),
        sent_at_ms: REFERENCE_MS,
        revisions: vec![
            Revision {
                event_seq: 1,
                edited_at_ms: None,
                observed_at_ms: REFERENCE_MS,
                payload_schema: SchemaFamily(1),
                body: historical,
            },
            Revision {
                event_seq: 2,
                edited_at_ms: Some(REFERENCE_MS + 1),
                observed_at_ms: REFERENCE_MS + 1,
                payload_schema: SchemaFamily(1),
                body: protected,
            },
        ],
        deletion: None,
    }];
    let document = render(RetentionMode::Audit, UtcOffset::UTC, &messages);

    assert!(document.contains("Telegram forbids saving"));
    for secret in [
        "audit-secret-before-protection",
        "audit-secret-service",
        "current-secret",
        "secret-filename.pdf",
        "secret/mime",
        "secret-media-name.pdf",
        "current-secret-service",
    ] {
        assert!(!document.contains(secret), "leaked {secret}");
    }
}

#[test]
fn timezone_is_explicit_and_shifts_civil_time() {
    let messages = vec![text_message(1, REFERENCE_MS, "hi")];
    let plus3 = UtcOffset::from_seconds(3 * 3_600).expect("valid offset");
    let document = render(RetentionMode::Mirror, plus3, &messages);
    // The header states the offset...
    assert!(document.contains("\ntimezone: UTC+03:00\n"));
    // ...and every civil value is computed in it: 22:13:20Z becomes the next
    // day, 01:13:20, in +03:00.
    assert!(document.contains("\n## 2023-11-15\n"));
    assert!(document.contains("**01:13:20 · user 555 · #1**"));
    assert!(!document.contains("2023-11-14"));
}

#[test]
fn utc_offset_validation_and_labels() {
    // Labels are asserted through the rendered header (the formatter is private).
    let label_in = |offset: UtcOffset| {
        let document = render(RetentionMode::Mirror, offset, &[]);
        document
            .lines()
            .find_map(|line| line.strip_prefix("timezone: "))
            .expect("timezone line")
            .to_owned()
    };
    assert_eq!(label_in(UtcOffset::UTC), "UTC");
    assert_eq!(
        label_in(UtcOffset::from_seconds(3 * 3_600).unwrap()),
        "UTC+03:00"
    );
    assert_eq!(
        label_in(UtcOffset::from_seconds(-(5 * 3_600 + 30 * 60)).unwrap()),
        "UTC-05:30"
    );
    // Nepal's +05:45 via the minute constructor.
    assert_eq!(
        label_in(UtcOffset::from_minutes(5 * 60 + 45).unwrap()),
        "UTC+05:45"
    );

    // A whole-day offset is the accepted boundary; beyond it is rejected.
    assert!(UtcOffset::from_seconds(24 * 3_600).is_ok());
    assert_eq!(
        UtcOffset::from_seconds(24 * 3_600 + 1).unwrap_err().seconds,
        24 * 3_600 + 1
    );
    assert!(UtcOffset::from_minutes(100_000).is_err());
    assert_eq!(UtcOffset::from_seconds(-3_600).unwrap().seconds(), -3_600);
}

#[test]
fn untrusted_text_cannot_break_structure() {
    // A message whose text tries every block- and inline-level injection.
    let malicious = "# Fake heading\n\
> injected quote\n\
- injected item\n\
---\n\
```\ncode fence\n```\n\
<script>alert(1)</script>\n\
[click](javascript:alert(1))\n\
| a | b |\n\
plain & <b>ampersand</b>";
    let messages = vec![text_message(1, REFERENCE_MS, malicious)];
    let document = render(RetentionMode::Mirror, UtcOffset::UTC, &messages);

    // Exactly one H1 (the title) and one H2 (the day) survive: the injected
    // `#` heading did not create another.
    assert_eq!(
        document
            .lines()
            .filter(|line| line.starts_with("# "))
            .count(),
        1
    );
    assert_eq!(
        document
            .lines()
            .filter(|line| line.starts_with("## "))
            .count(),
        1
    );
    // No body line is a blockquote or code fence, because every trigger
    // character is escaped.
    for line in document.lines() {
        assert!(!line.starts_with("> "), "raw blockquote leaked: {line:?}");
        assert!(!line.starts_with("```"), "raw code fence leaked: {line:?}");
    }
    // The only bare `---` lines are the two front-matter fences; the injected
    // thematic break was escaped to `\-\-\-`.
    assert_eq!(document.lines().filter(|line| *line == "---").count(), 2);
    assert!(document.contains("\\-\\-\\-"));
    // HTML is neutralized to entities; the raw tag never appears.
    assert!(!document.contains("<script>"));
    assert!(!document.contains("<b>"));
    assert!(document.contains("&lt;script&gt;alert\\(1\\)&lt;/script&gt;"));
    assert!(document.contains("plain &amp; &lt;b&gt;ampersand&lt;/b&gt;"));
    // The escaped heading and link are literal text, not structure.
    assert!(document.contains("\\# Fake heading"));
    assert!(document.contains("\\[click\\]\\(javascript:alert\\(1\\)\\)"));
    // Table pipes are escaped.
    assert!(document.contains("\\| a \\| b \\|"));
}

#[test]
fn indented_and_multiline_text_stays_one_paragraph() {
    // A blank line and a 4-space-indented line would, unescaped, split the body
    // and open an indented code block. The hard-break join keeps it one
    // paragraph, so neither happens.
    let messages = vec![text_message(1, REFERENCE_MS, "first\n\n    indented line")];
    let document = render(RetentionMode::Mirror, UtcOffset::UTC, &messages);
    assert!(document.contains("first\\\n\\\n    indented line"));
    // No blank line inside the body: the message is followed by a single blank
    // line then end-of-document, never a `first`/`indented` split.
    assert!(!document.contains("first\n\n    indented"));
}

#[test]
fn mirror_shows_current_state_and_purges_deletions() {
    let messages = corpus();
    let document = render(RetentionMode::Mirror, UtcOffset::UTC, &messages);
    // The deleted message (102) is gone entirely.
    assert!(!document.contains("#102"));
    assert!(!document.contains("edited then deleted"));
    // The thrice-edited message shows only its latest text, no history.
    assert!(document.contains("**22:15:00 · user 555 · #101** · edited 22:18:20"));
    assert!(document.contains("\nthird\n"));
    assert!(!document.contains("Earlier revisions"));
}

#[test]
fn audit_keeps_tombstone_and_revision_history() {
    let messages = corpus();
    let document = render(RetentionMode::Audit, UtcOffset::UTC, &messages);
    // 102 survives as a content-preserving tombstone.
    assert!(document.contains("· #102** · edited 22:17:30 · deleted"));
    assert!(document.contains("edited then deleted"));
    assert!(document.contains("_Deleted 22:20:00._"));
    // 101 keeps its earlier revisions in event_seq order.
    assert!(document.contains("_Earlier revisions:_\n\n- 22:15:00: first\n- 22:16:40: second"));
}

#[test]
fn missing_sender_is_labeled() {
    let messages = corpus();
    let document = render(RetentionMode::Mirror, UtcOffset::UTC, &messages);
    assert!(document.contains("**22:16:50 · unknown sender · #109**"));
}

#[test]
fn attachments_link_to_direct_month_siblings_and_state_is_explicit() {
    let mut body = empty_body();
    body.attachments = vec![
        Attachment {
            index: AttachmentIndex(0),
            media_kind: MediaKind::Photo,
            telegram_representation: TelegramRepresentation::OriginalDocument,
            fidelity: AttachmentFidelity::Original,
            source_name: Some("holiday photo.jpg".to_owned()),
            mime_type: Some("image/jpeg".to_owned()),
            exact_size: Some(2_048),
            availability: Availability::Fetchable,
            content_hash: Some(ContentHash::Sha256([0x11; 32])),
            media_name: Some("holiday photo.jpg".to_owned()),
        },
        Attachment {
            index: AttachmentIndex(1),
            media_kind: MediaKind::Document,
            telegram_representation: TelegramRepresentation::OriginalDocument,
            fidelity: AttachmentFidelity::Original,
            source_name: Some("secret.pdf".to_owned()),
            mime_type: None,
            exact_size: None,
            availability: Availability::Restricted,
            content_hash: None,
            media_name: None,
        },
    ];
    let messages = vec![message_with(200, REFERENCE_MS, body)];
    let document = render(RetentionMode::Mirror, UtcOffset::UTC, &messages);

    // The downloaded photo links directly to its month sibling, percent-encoded
    // in the destination and Markdown-escaped in the visible text.
    assert!(
        document.contains("- **photo** — [holiday photo\\.jpg](holiday%20photo.jpg) (2048 bytes)")
    );
    // The restricted document is described but not linked, with an explicit note.
    assert!(
        document.contains("- **document** — secret\\.pdf — _restricted by Telegram; not fetched_")
    );
    assert!(!document.contains("](secret.pdf)"));
}

#[test]
fn malformed_processed_attachment_is_rejected_before_markdown_output() {
    let mut messages = corpus();
    let attachment = messages
        .iter_mut()
        .flat_map(|message| &mut message.revisions)
        .flat_map(|revision| &mut revision.body.attachments)
        .next()
        .expect("fixture attachment");
    attachment.telegram_representation = TelegramRepresentation::Video;
    attachment.fidelity = AttachmentFidelity::TelegramVariant;
    attachment.source_name = Some("claimed-original.mp4".to_owned());
    let timezone = DisplayTimeZone::fixed(UtcOffset::UTC);
    let input = MarkdownInput {
        chat: fixture_chat(),
        partition: DocPartition::Month {
            year: 2023,
            month: 11,
        },
        retention_mode: RetentionMode::Mirror,
        timezone: &timezone,
        input_watermark_seq: 13,
        render_generation: 0,
        messages: &messages,
    };
    let mut document = String::new();
    assert!(markdown::write_transcript(&mut document, &input).is_err());
    assert!(document.is_empty());
    assert!(markdown::render_transcript(&input).is_empty());
}

#[test]
fn service_messages_render_as_notes() {
    let make_service = |id: i64, action: ServiceAction| {
        let mut body = empty_body();
        body.service = Some(action);
        message_with(id, REFERENCE_MS + id, body)
    };
    let messages = vec![
        make_service(
            1,
            ServiceAction::ChatTitleChanged {
                title: "Team **Rocket**".to_owned(),
            },
        ),
        make_service(
            2,
            ServiceAction::MembersAdded {
                user_ids: vec![7, 8],
            },
        ),
        make_service(3, ServiceAction::AutoDeleteTimerChanged { seconds: 0 }),
    ];
    let document = render(RetentionMode::Mirror, UtcOffset::UTC, &messages);
    // The untrusted title is escaped inside the service note.
    assert!(document.contains("_Renamed to “Team \\*\\*Rocket\\*\\*”._"));
    assert!(document.contains("_Added 7, 8._"));
    assert!(document.contains("_Auto-delete timer disabled._"));
}

#[test]
fn rtl_and_emoji_and_long_text_survive_intact() {
    let long = "слово ".repeat(400);
    let text = format!("مرحبا 👨‍👩‍👧 {long}");
    let messages = vec![text_message(1, REFERENCE_MS, &text)];
    let document = render(RetentionMode::Mirror, UtcOffset::UTC, &messages);
    // RTL text and the ZWJ emoji sequence pass through byte-for-byte.
    assert!(document.contains("مرحبا 👨‍👩‍👧 слово"));
    // The whole long line is present (bounded output does not truncate content).
    assert!(document.contains(&long.trim_end().to_owned()));
}

#[test]
fn identical_input_is_byte_identical_and_revision_order_independent() {
    let messages = corpus();
    let first = render(RetentionMode::Audit, UtcOffset::UTC, &messages);
    let second = render(RetentionMode::Audit, UtcOffset::UTC, &messages);
    assert_eq!(first, second);

    // A message whose revisions are supplied in two different orders renders
    // identically: the renderer sorts by event_seq.
    let build = |order: [i64; 3]| {
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
                    edited_at_ms: if seq == 2 {
                        None
                    } else {
                        Some(REFERENCE_MS + seq * 1000)
                    },
                    observed_at_ms: REFERENCE_MS + seq,
                    payload_schema: SchemaFamily(1),
                    body,
                }
            })
            .collect();
        vec![MessageHistory {
            message_id: MessageId(1),
            sender: Some(Sender { id: 1 }),
            sent_at_ms: REFERENCE_MS,
            revisions,
            deletion: None,
        }]
    };
    let ascending = render(RetentionMode::Audit, UtcOffset::UTC, &build([2, 5, 9]));
    let shuffled = render(RetentionMode::Audit, UtcOffset::UTC, &build([9, 2, 5]));
    assert_eq!(ascending, shuffled);
}

#[test]
fn streaming_matches_the_string_form() {
    let messages = corpus();
    let timezone = DisplayTimeZone::fixed(UtcOffset::from_seconds(2 * 3_600).expect("valid"));
    let input = MarkdownInput {
        chat: fixture_chat(),
        partition: november_2023(),
        retention_mode: RetentionMode::Audit,
        timezone: &timezone,
        input_watermark_seq: 13,
        render_generation: 0,
        messages: &messages,
    };
    let mut streamed = String::new();
    markdown::write_transcript(&mut streamed, &input).expect("string sink never fails");
    assert_eq!(streamed, markdown::render_transcript(&input));
}

#[test]
fn document_id_is_a_markdown_generated_doc() {
    let markdown_id = markdown::document_id(fixture_chat(), november_2023());
    // It decodes to a GeneratedDoc key in the Markdown format.
    match markdown_id.key() {
        ItemKey::Canonical(CanonicalKey::GeneratedDoc(doc)) => {
            assert_eq!(doc.format, DocFormat::Markdown);
            assert_eq!(doc.partition, november_2023());
        }
        other => panic!("unexpected key: {other:?}"),
    }
    // And it is distinct from the NDJSON document for the same partition.
    let ndjson_id = gramdrive_render::ndjson::document_id(fixture_chat(), november_2023()).text();
    assert_ne!(markdown_id.text(), ndjson_id);
}

#[test]
fn content_version_token_folds_schema_renderer_watermark_and_policy() {
    let timezone = DisplayTimeZone::fixed(UtcOffset::UTC);
    assert_eq!(
        markdown::content_version_token(42, 7, RetentionMode::Audit, timezone.label()),
        "gramdrive.transcript/s2/r4/w42/g7/retention-audit/tz-UTC"
    );
}

#[test]
fn helper_types_are_public() {
    // Compile-time proof the input contract is fully re-exported from markdown.
    let _ = Deletion { observed_at_ms: 1 };
    let _ = ServiceAction::MessagePinned {
        message_id: MessageId(1),
    };
}
