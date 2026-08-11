//! The attachment-metadata mapping fixture corpus (TASK-260715-23arcu;
//! PRD-030/PRD-032/PRD-033, POL-4): realistic tdjson media `message` objects
//! driven through [`normalize_message`] and [`map_message_attachments`], with
//! the mapped attachment — stable identity, original and safe name, MIME, size,
//! Telegram locator, thumbnail descriptor, availability and saveability —
//! asserted per fixture.
//!
//! The acceptance criterion is two-fold and both halves are proven here:
//! fixtures preserve provenance and capability restrictions (a restricted or
//! view-once item is marked unavailable and never carries fetchable previews),
//! and multiple attachments/albums remain distinct (an album's items map to
//! distinct identities sharing one album id).

// clippy.toml exempts test code keyed on `#[test]` functions; the fixture
// helpers below sit at module level in an integration-test binary. This file
// links into no product artifact (established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use serde_json::{Value, json};

use gramdrive_model::identity::{AccountId, AccountKey, AccountScope, NamespaceVersion};
use gramdrive_source_tdjson::{
    AttachmentAvailability, AttachmentFidelity, AttachmentKind, AttachmentLogicalKind,
    MappedAttachment, TelegramRepresentation, ThumbnailFormat, map_message_attachments,
    normalize_message,
};

const CHAT: i64 = -1_001_234_567_890;
const USER: i64 = 111_222_333;

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(7),
        },
        namespace_version: NamespaceVersion(3),
    }
}

/// A realistic tdjson message envelope around `content`. `overrides` merges
/// into the envelope so a fixture can flip `can_be_saved`, set an album id, or
/// add a `self_destruct_type` without a second template.
fn wire_message(id: i64, overrides: Value, content: Value) -> Value {
    let mut message = json!({
        "@type": "message",
        "id": id,
        "sender_id": {"@type": "messageSenderUser", "user_id": USER},
        "chat_id": CHAT,
        "is_outgoing": false,
        "can_be_saved": true,
        "date": 1_752_800_000,
        "content": content
    });
    let object = message.as_object_mut().expect("message is an object");
    for (key, value) in overrides.as_object().expect("overrides is an object") {
        object.insert(key.clone(), value.clone());
    }
    message
}

/// Normalize and map, asserting the message yields exactly one attachment.
fn map_single(message: &Value) -> MappedAttachment {
    let record = normalize_message(message).expect("fixture normalizes");
    let mut mapped = map_message_attachments(&record, scope());
    assert_eq!(mapped.len(), 1, "fixture must map to one attachment");
    mapped.remove(0)
}

// A `file` sub-object with locators, as the tdjson interface sends them.
fn file(id: i64, size: i64, unique: &str) -> Value {
    json!({
        "@type": "file",
        "id": id,
        "size": size,
        "remote": {"id": format!("remote-{id}"), "unique_id": unique}
    })
}

// A downloadable `thumbnail` object of the given format.
fn thumbnail(format: &str, file_id: i64) -> Value {
    json!({
        "@type": "thumbnail",
        "format": {"@type": format},
        "width": 320,
        "height": 180,
        "file": file(file_id, 5_000, &format!("thumb-{file_id}"))
    })
}

// -- Every PRD-030 kind maps ------------------------------------------------

#[test]
fn document_preserves_original_metadata_and_locator() {
    let content = json!({
        "@type": "messageDocument",
        "caption": {"text": "the report"},
        "document": {
            "file_name": "Q3 report.pdf",
            "mime_type": "application/pdf",
            "minithumbnail": {"width": 24, "height": 30, "data": "bWluaQ=="},
            "thumbnail": thumbnail("thumbnailFormatJpeg", 88),
            "document": file(517, 2_048_576, "doc-unique")
        }
    });
    let mapped = map_single(&wire_message(1001, json!({}), content));

    // Identity: chat + message + ordinal zero, under the observing scope.
    assert_eq!(mapped.key.message.chat.scope, scope());
    assert_eq!(mapped.key.message.chat.chat_id.0, CHAT);
    assert_eq!(mapped.key.message.message_id.0, 1001);
    assert_eq!(mapped.key.index.0, 0);

    // Original metadata preserved verbatim (PRD-032).
    let descriptor = &mapped.descriptor;
    assert_eq!(descriptor.kind, AttachmentKind::Document);
    assert_eq!(descriptor.file_name.as_deref(), Some("Q3 report.pdf"));
    assert_eq!(descriptor.mime_type.as_deref(), Some("application/pdf"));
    assert_eq!(descriptor.size, Some(2_048_576));
    assert_eq!(descriptor.file_id, Some(517));
    assert_eq!(descriptor.remote_unique_id.as_deref(), Some("doc-unique"));

    // Safe name derived from the original; spaces are legal, the extension is
    // kept.
    assert_eq!(mapped.safe_name.as_str(), "Q3 report.pdf");
    assert_eq!(mapped.logical_kind, AttachmentLogicalKind::Document);
    assert_eq!(
        mapped.telegram_representation,
        TelegramRepresentation::OriginalDocument
    );
    assert_eq!(mapped.fidelity, AttachmentFidelity::Original);
    assert_eq!(mapped.source_name.as_deref(), Some("Q3 report.pdf"));

    // Fetchable, so both previews come through.
    assert_eq!(descriptor.availability, AttachmentAvailability::Fetchable);
    assert!(mapped.can_be_saved);
    let thumb = descriptor.thumbnail.as_ref().expect("thumbnail present");
    assert_eq!(thumb.format, ThumbnailFormat::Jpeg);
    assert_eq!(thumb.file_id, 88);
    assert_eq!((thumb.width, thumb.height), (Some(320), Some(180)));
    let mini = descriptor
        .minithumbnail
        .as_ref()
        .expect("minithumbnail present");
    assert_eq!(mini.data_base64, "bWluaQ==");
}

#[test]
fn every_downloadable_kind_maps_with_identity_and_safe_name() {
    struct Case {
        content: Value,
        kind: AttachmentKind,
        safe_name: &'static str,
    }

    let cases = vec![
        Case {
            content: json!({
                "@type": "messagePhoto",
                "photo": {"sizes": [
                    {"type": "s", "width": 90, "height": 60, "photo": file(11, 900, "ph-s")},
                    {"type": "x", "width": 1280, "height": 720, "photo": file(12, 120_000, "ph-x")}
                ]}
            }),
            kind: AttachmentKind::Photo,
            safe_name: "photo.jpg",
        },
        Case {
            content: json!({
                "@type": "messageVideo",
                "caption": {"text": ""},
                "video": {
                    "duration": 30, "width": 1920, "height": 1080,
                    "file_name": "trip.mov", "mime_type": "video/quicktime",
                    "thumbnail": thumbnail("thumbnailFormatMpeg4", 21),
                    "video": file(20, 8_000_000, "vid-x")
                }
            }),
            kind: AttachmentKind::Video,
            safe_name: "video.mov",
        },
        Case {
            content: json!({
                "@type": "messageAnimation",
                "caption": {"text": ""},
                "animation": {
                    "duration": 3, "width": 480, "height": 480,
                    "mime_type": "video/mp4",
                    "thumbnail": thumbnail("thumbnailFormatMpeg4", 31),
                    "animation": file(30, 400_000, "anim-x")
                }
            }),
            kind: AttachmentKind::Animation,
            safe_name: "animation.mp4",
        },
        Case {
            content: json!({
                "@type": "messageAudio",
                "caption": {"text": ""},
                "audio": {
                    "duration": 210, "mime_type": "audio/mpeg",
                    "album_cover_thumbnail": thumbnail("thumbnailFormatJpeg", 41),
                    "album_cover_minithumbnail": {"width": 20, "height": 20, "data": "Y292ZXI="},
                    "audio": file(40, 5_000_000, "aud-x")
                }
            }),
            kind: AttachmentKind::Audio,
            safe_name: "audio.mp3",
        },
        Case {
            content: json!({
                "@type": "messageVoiceNote",
                "caption": {"text": ""},
                "voice_note": {
                    "duration": 8, "mime_type": "audio/ogg",
                    "voice": file(50, 40_000, "voice-x")
                }
            }),
            kind: AttachmentKind::VoiceNote,
            safe_name: "voice.ogg",
        },
        Case {
            content: json!({
                "@type": "messageVideoNote",
                "video_note": {
                    "duration": 12, "length": 240,
                    "thumbnail": thumbnail("thumbnailFormatJpeg", 61),
                    "video": file(60, 900_000, "vnote-x")
                }
            }),
            kind: AttachmentKind::VideoNote,
            safe_name: "video_note.mp4",
        },
        Case {
            content: json!({
                "@type": "messageSticker",
                "sticker": {
                    "width": 512, "height": 512, "emoji": "🎉",
                    "mime_type": "image/webp",
                    "thumbnail": thumbnail("thumbnailFormatWebp", 71),
                    "sticker": file(70, 30_000, "stk-x")
                }
            }),
            kind: AttachmentKind::Sticker,
            safe_name: "sticker.webp",
        },
    ];

    for (offset, case) in cases.into_iter().enumerate() {
        let id = 2000 + offset as i64;
        let mapped = map_single(&wire_message(id, json!({}), case.content));
        assert_eq!(mapped.descriptor.kind, case.kind, "kind for {id}");
        assert_eq!(mapped.key.message.message_id.0, id, "identity for {id}");
        assert_eq!(mapped.key.index.0, 0, "ordinal for {id}");
        assert_eq!(
            mapped.safe_name.as_str(),
            case.safe_name,
            "safe name for {id}"
        );
        assert_eq!(
            mapped.descriptor.availability,
            AttachmentAvailability::Fetchable,
            "availability for {id}"
        );
        if case.kind != AttachmentKind::Document {
            assert_eq!(mapped.source_name, None, "source name for {id}");
            assert_eq!(
                mapped.fidelity,
                AttachmentFidelity::TelegramVariant,
                "fidelity for {id}"
            );
        }
    }
}

#[test]
fn image_and_video_mime_documents_remain_original_document_representations() {
    for (offset, mime, name, logical_kind) in [
        (0, "image/png", "diagram.png", AttachmentLogicalKind::Photo),
        (1, "video/mp4", "master.mp4", AttachmentLogicalKind::Video),
    ] {
        let content = json!({
            "@type": "messageDocument",
            "document": {
                "file_name": name,
                "mime_type": mime,
                "document": file(600 + offset, 42_000, &format!("doc-{offset}"))
            }
        });
        let mapped = map_single(&wire_message(2_500 + offset, json!({}), content));
        assert_eq!(mapped.logical_kind, logical_kind);
        assert_eq!(
            mapped.telegram_representation,
            TelegramRepresentation::OriginalDocument
        );
        assert_eq!(mapped.fidelity, AttachmentFidelity::Original);
        assert_eq!(mapped.source_name.as_deref(), Some(name));
        assert_eq!(mapped.safe_name.as_str(), name);
    }
}

#[test]
fn expected_size_is_not_persisted_as_an_exact_size_claim() {
    let content = json!({
        "@type": "messageDocument",
        "document": {
            "file_name": "estimate.bin",
            "mime_type": "application/octet-stream",
            "document": {
                "id": 700,
                "size": 0,
                "expected_size": 9_999,
                "remote": {"id": "remote-700", "unique_id": "expected-only"}
            }
        }
    });
    let mapped = map_single(&wire_message(2_600, json!({}), content));
    assert_eq!(mapped.descriptor.size, None);
    assert_eq!(mapped.descriptor.file_id, Some(700));
    assert_eq!(mapped.fidelity, AttachmentFidelity::Original);
}

#[test]
fn expired_and_locatorless_media_map_to_metadata_only_placeholders() {
    let expired = map_single(&wire_message(
        2_700,
        json!({}),
        json!({"@type": "messageExpiredPhoto"}),
    ));
    let locatorless = map_single(&wire_message(
        2_701,
        json!({}),
        json!({"@type": "messagePhoto", "photo": {"sizes": []}}),
    ));
    for mapped in [expired, locatorless] {
        assert_eq!(mapped.descriptor.file_id, None);
        assert_eq!(
            mapped.descriptor.availability,
            AttachmentAvailability::Unavailable
        );
        assert_eq!(mapped.fidelity, AttachmentFidelity::MetadataOnly);
        assert_eq!(mapped.source_name, None);
        assert_eq!(mapped.safe_name.as_str(), "photo.jpg");
    }
}

// -- Capability restrictions (POL-4) ----------------------------------------

#[test]
fn restricted_content_is_unavailable_and_never_fetchable() {
    // A protected-content chat (can_be_saved = false): the item is visible but
    // marked restricted and carries no preview bytes or locators.
    let content = json!({
        "@type": "messageDocument",
        "caption": {"text": "leaked"},
        "document": {
            "file_name": "protected.pdf",
            "mime_type": "application/pdf",
            "minithumbnail": {"width": 24, "height": 30, "data": "c2hvdWxkLWRyb3A="},
            "thumbnail": thumbnail("thumbnailFormatJpeg", 90),
            "document": file(80, 2_048, "prot-unique")
        }
    });
    let mapped = map_single(&wire_message(3001, json!({"can_be_saved": false}), content));

    // Provenance is preserved — the metadata is fully mapped, only the bytes
    // are withheld.
    assert_eq!(
        mapped.descriptor.file_name.as_deref(),
        Some("protected.pdf")
    );
    assert_eq!(mapped.safe_name.as_str(), "protected.pdf");
    assert!(!mapped.can_be_saved, "can_be_saved carried verbatim");
    assert_eq!(
        mapped.descriptor.availability,
        AttachmentAvailability::Restricted
    );
    assert_eq!(
        mapped.descriptor.thumbnail, None,
        "no restricted preview locator"
    );
    assert_eq!(
        mapped.descriptor.minithumbnail, None,
        "no restricted preview bytes"
    );
}

#[test]
fn view_once_media_is_unavailable_and_never_fetchable() {
    // Self-destructing media: never persisted, shown as unavailable, and — even
    // though can_be_saved is true — carries no preview.
    let content = json!({
        "@type": "messagePhoto",
        "photo": {
            "minithumbnail": {"width": 24, "height": 30, "data": "c2hvdWxkLWRyb3A="},
            "sizes": [
                {"type": "s", "width": 90, "height": 60, "photo": file(101, 900, "vo-s")},
                {"type": "x", "width": 1280, "height": 720, "photo": file(102, 120_000, "vo-x")}
            ]
        }
    });
    let overrides = json!({
        "self_destruct_type": {"@type": "messageSelfDestructTypeImmediately"}
    });
    let mapped = map_single(&wire_message(3002, overrides, content));

    assert!(
        mapped.can_be_saved,
        "can_be_saved is still true for view-once"
    );
    assert_eq!(
        mapped.descriptor.availability,
        AttachmentAvailability::ViewOnce
    );
    assert_eq!(mapped.descriptor.thumbnail, None);
    assert_eq!(mapped.descriptor.minithumbnail, None);
}

// -- Albums remain distinct (PRD-033) ---------------------------------------

#[test]
fn album_items_are_distinct_identities_sharing_one_album_id() {
    // Telegram models an album as consecutive one-attachment messages sharing a
    // media_album_id; each maps to its own identity, all carrying that id as
    // provenance and none merged.
    let album = json!({"media_album_id": "9182736450"});
    let photo = |unique: &str, file_id: i64| {
        json!({
            "@type": "messagePhoto",
            "photo": {"sizes": [
                {"type": "x", "width": 800, "height": 600, "photo": file(file_id, 90_000, unique)}
            ]}
        })
    };

    let mut mapped: Vec<MappedAttachment> = (0..3)
        .map(|i| {
            let message = wire_message(
                4000 + i,
                album.clone(),
                photo(&format!("album-{i}"), 200 + i),
            );
            map_single(&message)
        })
        .collect();

    // All three carry the same album id and distinct identities.
    for item in &mapped {
        assert_eq!(item.album_id, Some(9_182_736_450));
        assert_eq!(item.key.index.0, 0);
        assert_eq!(item.safe_name.as_str(), "photo.jpg");
    }
    let keys: std::collections::HashSet<_> = mapped.iter().map(|item| item.key).collect();
    assert_eq!(keys.len(), 3, "three distinct attachment identities");

    // Distinct Telegram dedup keys survive — dedup must not merge them.
    mapped.sort_by_key(|item| item.key.message.message_id.0);
    let uniques: Vec<_> = mapped
        .iter()
        .map(|item| item.descriptor.remote_unique_id.as_deref())
        .collect();
    assert_eq!(
        uniques,
        vec![Some("album-0"), Some("album-1"), Some("album-2")]
    );
}

#[test]
fn a_non_album_attachment_has_no_album_provenance() {
    let content = json!({
        "@type": "messageDocument",
        "caption": {"text": ""},
        "document": {
            "file_name": "solo.txt",
            "mime_type": "text/plain",
            "document": file(300, 10, "solo-unique")
        }
    });
    let mapped = map_single(&wire_message(5000, json!({}), content));
    assert_eq!(mapped.album_id, None);
}
