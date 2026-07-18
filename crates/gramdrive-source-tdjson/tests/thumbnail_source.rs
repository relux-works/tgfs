//! The eager thumbnail adapter, end to end (TASK-260715-3nl3mu): the
//! [`TdThumbnailer`] driver over the real runtime and the mock tdjson,
//! serving Telegram previews without ever hydrating the full media.
//!
//! The preview facts come from real normalized messages — a `messagePhoto`,
//! `messageVideo`, and `messageDocument` run through
//! [`normalize_message`] and projected with
//! [`ThumbnailTarget::from_descriptor`] — so the photo/video/document classes
//! (POL-2, the story checklist) are exercised through the same extraction the
//! product uses. The mock's responder plays TDLib's side of a whole-file
//! preview download over a temporary file this suite wrote; the assertions on
//! its untouched content confirm the read-only ownership rule, and the
//! assertion that only the *preview* file id is ever downloaded confirms a
//! thumbnail never triggers full-content hydration (the AC).

// clippy.toml exempts test code keyed on `#[test]` functions; the fixture
// helpers below sit at module level in an integration-test binary. The
// rationale applies in full — this file links into no product artifact
// (established test-suite pattern, common/mod.rs).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use gramdrive_model::identity::ItemId;
use gramdrive_source::{SourceError, ThumbnailSpec};
use gramdrive_source_tdjson::message::normalize_message;
use gramdrive_source_tdjson::mock::SentRequest;
use gramdrive_source_tdjson::{
    DownloadPriority, TdThumbnailer, ThumbnailCatalog, ThumbnailConfig, ThumbnailTarget,
};
use gramdrive_testkit::exec;
use gramdrive_testkit::fixture;

use common::block_on;

/// The full-resolution media file id — the one a thumbnail must never
/// download. Every preview lives under [`THUMB_FILE_ID`] instead.
const MEDIA_FILE_ID: i64 = 700;
/// The preview (thumbnail) file id — a small, dedicated file.
const THUMB_FILE_ID: i64 = 701;
const CHAT_ID: i64 = 100;
const MESSAGE_ID: i64 = 5;
/// The bytes of the world's preview file — a stand-in for a real JPEG.
const PREVIEW: &[u8] = b"\xff\xd8\xff\xe0 a small jpeg preview, byte for byte \xff\xd9";

fn item() -> ItemId {
    fixture::attachment_id(fixture::scope(), CHAT_ID, MESSAGE_ID, 0)
}

fn spec(side: u32) -> ThumbnailSpec {
    let side = NonZeroU32::new(side).expect("non-zero");
    ThumbnailSpec {
        max_width_px: side,
        max_height_px: side,
    }
}

// ---------------------------------------------------------------------------
// The wire messages the preview facts come from
// ---------------------------------------------------------------------------

/// A `remote` locator sub-object.
fn remote(unique: &str) -> Value {
    json!({"id": format!("r-{unique}"), "unique_id": unique})
}

/// A TDLib `thumbnail` object over the preview file — the member videos,
/// animations, documents, and stickers carry their preview under.
fn thumbnail_object() -> Value {
    json!({
        "@type": "thumbnail",
        "format": {"@type": "thumbnailFormatJpeg"},
        "width": 320,
        "height": 240,
        "file": {"id": THUMB_FILE_ID, "size": PREVIEW.len(), "remote": remote("thumb")},
    })
}

fn minithumbnail_object(data_base64: &str) -> Value {
    json!({"@type": "minithumbnail", "width": 40, "height": 30, "data": data_base64})
}

fn wire_message(content: Value) -> Value {
    json!({
        "@type": "message",
        "id": MESSAGE_ID,
        "chat_id": CHAT_ID,
        "date": 1_752_800_000,
        "sender_id": {"@type": "messageSenderUser", "user_id": 42},
        "can_be_saved": true,
        "content": content,
    })
}

/// A photo whose smallest stored size is the preview file and whose largest
/// is the media file (the normalizer takes the largest as the attachment and
/// the smallest as its thumbnail).
fn photo_message() -> Value {
    wire_message(json!({
        "@type": "messagePhoto",
        "caption": {"text": ""},
        "photo": {
            "sizes": [
                {"type": "m", "width": 320, "height": 240,
                 "photo": {"id": THUMB_FILE_ID, "size": PREVIEW.len(), "remote": remote("thumb")}},
                {"type": "y", "width": 1280, "height": 960,
                 "photo": {"id": MEDIA_FILE_ID, "size": 200_000, "remote": remote("media")}},
            ],
        },
    }))
}

fn video_message() -> Value {
    wire_message(json!({
        "@type": "messageVideo",
        "caption": {"text": ""},
        "video": {
            "width": 1280, "height": 720, "duration": 12,
            "thumbnail": thumbnail_object(),
            "minithumbnail": minithumbnail_object("aGVsbG8="),
            "video": {"id": MEDIA_FILE_ID, "size": 5_000_000, "remote": remote("media")},
        },
    }))
}

fn document_message() -> Value {
    wire_message(json!({
        "@type": "messageDocument",
        "caption": {"text": ""},
        "document": {
            "file_name": "report.pdf",
            "mime_type": "application/pdf",
            "thumbnail": thumbnail_object(),
            "document": {"id": MEDIA_FILE_ID, "size": 2_048, "remote": remote("media")},
        },
    }))
}

/// Project a wire message's attachment into the target the catalog serves.
fn target_of(message: &Value) -> ThumbnailTarget {
    let record = normalize_message(message).expect("the message normalizes");
    let descriptor = record
        .content
        .attachment()
        .expect("the message carries an attachment");
    ThumbnailTarget::from_descriptor(descriptor)
}

// ---------------------------------------------------------------------------
// The catalog
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct TestCatalog {
    entries: Mutex<HashMap<ItemId, ThumbnailTarget>>,
}

impl TestCatalog {
    fn with(target: ThumbnailTarget) -> Arc<TestCatalog> {
        let catalog = TestCatalog::default();
        catalog
            .entries
            .lock()
            .expect("fresh mutex")
            .insert(item(), target);
        Arc::new(catalog)
    }
}

impl ThumbnailCatalog for TestCatalog {
    fn resolve(&self, item: &ItemId) -> Option<ThumbnailTarget> {
        self.entries.lock().expect("test mutex").get(item).cloned()
    }
}

// ---------------------------------------------------------------------------
// TDLib's side and the driver
// ---------------------------------------------------------------------------

/// A unique temporary file holding `bytes`, under the cargo-managed test
/// temp dir — the stand-in for TDLib's files directory.
fn tdlib_file(bytes: &[u8]) -> PathBuf {
    static UNIQUE: AtomicUsize = AtomicUsize::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "thumbnail-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir creates");
    let path = dir.join("preview.bin");
    std::fs::write(&path, bytes).expect("preview writes");
    path
}

/// The `file` object a covering whole-file preview download answers with.
fn preview_response(extra: u64, client_id: i32, path: &str, size: u64) -> String {
    json!({
        "@type": "file",
        "id": THUMB_FILE_ID,
        "size": size,
        "local": {
            "@type": "localFile",
            "path": path,
            "download_offset": 0,
            "downloaded_prefix_size": size,
            "is_downloading_active": false,
            "is_downloading_completed": true,
        },
        "@extra": extra,
        "@client_id": client_id,
    })
    .to_string()
}

fn error_response(extra: u64, client_id: i32, code: i64, message: &str) -> String {
    json!({
        "@type": "error",
        "code": code,
        "message": message,
        "@extra": extra,
        "@client_id": client_id,
    })
    .to_string()
}

/// A responder that answers every preview `downloadFile` over `path`, and
/// asserts it is only ever the *preview* file that is downloaded — a
/// thumbnail must never hydrate the full media (AC).
fn serving_responder(path: PathBuf) -> impl FnMut(&SentRequest) -> Vec<String> + Send + 'static {
    move |sent: &SentRequest| {
        let value: Value = serde_json::from_str(&sent.json).expect("requests are JSON");
        let extra = sent.extra().expect("the runtime injects @extra");
        match value.get("@type").and_then(Value::as_str) {
            Some("downloadFile") => {
                assert_eq!(
                    value.get("file_id").and_then(Value::as_i64),
                    Some(THUMB_FILE_ID),
                    "a thumbnail downloads the preview file, never the media file"
                );
                assert_eq!(
                    value.get("synchronous").and_then(Value::as_bool),
                    Some(true),
                    "the preview is downloaded synchronously"
                );
                assert_eq!(
                    value.get("limit").and_then(Value::as_u64),
                    Some(0),
                    "a preview is a whole-file download"
                );
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                vec![preview_response(
                    extra,
                    sent.client_id,
                    path.to_str().expect("utf-8 temp path"),
                    size,
                )]
            }
            Some("cancelDownloadFile") => {
                vec![format!(
                    r#"{{"@type":"ok","@extra":{extra},"@client_id":{}}}"#,
                    sent.client_id
                )]
            }
            other => panic!("unexpected request {other:?}"),
        }
    }
}

/// A runtime, client, and thumbnailer over a fresh mock.
fn thumbnailer(
    catalog: Arc<dyn ThumbnailCatalog>,
) -> (
    gramdrive_source_tdjson::TdRuntime,
    gramdrive_source_tdjson::mock::MockHandle,
    TdThumbnailer,
) {
    let (runtime, handle) = common::start_runtime(common::test_config());
    let (client, _updates) = runtime.create_client().expect("client registers");
    let config = ThumbnailConfig {
        priority: DownloadPriority::new(9).expect("9 is in range"),
        max_preview_bytes: NonZeroU64::new(1024 * 1024).expect("non-zero"),
    };
    let adapter = TdThumbnailer::new(client, catalog, config);
    (runtime, handle, adapter)
}

// ---------------------------------------------------------------------------
// The photo/video/document classes (POL-2)
// ---------------------------------------------------------------------------

fn assert_serves_preview(message: Value) {
    let path = tdlib_file(PREVIEW);
    let catalog = TestCatalog::with(target_of(&message));
    let (_runtime, handle, adapter) = thumbnailer(catalog);
    handle.set_responder(serving_responder(path.clone()));

    let answer = block_on(adapter.thumbnail(item(), spec(256))).expect("the preview resolves");
    let thumbnail = answer.expect("a thumbnail is served for this class");
    assert_eq!(thumbnail.mime_type(), "image/jpeg");
    assert_eq!(thumbnail.bytes(), PREVIEW);

    // Exactly one preview download, and TDLib's file was only ever read.
    let downloads = handle
        .take_sent()
        .iter()
        .filter(|sent| sent.request_type().as_deref() == Some("downloadFile"))
        .count();
    assert_eq!(downloads, 1, "one preview download");
    assert_eq!(
        std::fs::read(&path).expect("the preview file survives"),
        PREVIEW,
        "the adapter reads TDLib's file in place, never moves or rewrites it"
    );
}

#[test]
fn a_photo_serves_its_smallest_size_as_the_thumbnail() {
    assert_serves_preview(photo_message());
}

#[test]
fn a_video_serves_its_thumbnail_frame() {
    assert_serves_preview(video_message());
}

#[test]
fn a_document_serves_its_thumbnail() {
    assert_serves_preview(document_message());
}

// ---------------------------------------------------------------------------
// POL-4 and the missing-thumbnail fallback
// ---------------------------------------------------------------------------

#[test]
fn restricted_content_is_refused_and_costs_no_request() {
    // The normalizer drops every preview from a restricted attachment; the
    // catalog carries only the availability.
    let target = ThumbnailTarget {
        availability: gramdrive_source_tdjson::message::AttachmentAvailability::Restricted,
        downloadable: None,
        inline: None,
    };
    let catalog = TestCatalog::with(target);
    let (_runtime, handle, adapter) = thumbnailer(catalog);
    // No responder: any request would panic the receive loop's expectations —
    // and none may exist (POL-4).

    let error = block_on(adapter.thumbnail(item(), spec(256)))
        .expect_err("restricted content refuses a thumbnail");
    assert!(
        matches!(error, SourceError::Restricted { .. }),
        "a thumbnail of restricted content is Restricted, got {error:?}"
    );
    assert!(
        handle.take_sent().is_empty(),
        "POL-4: a restricted thumbnail costs zero network requests"
    );
}

#[test]
fn view_once_content_is_refused_like_restricted() {
    let target = ThumbnailTarget {
        availability: gramdrive_source_tdjson::message::AttachmentAvailability::ViewOnce,
        downloadable: None,
        inline: None,
    };
    let catalog = TestCatalog::with(target);
    let (_runtime, handle, adapter) = thumbnailer(catalog);

    let error = block_on(adapter.thumbnail(item(), spec(256)))
        .expect_err("view-once content refuses a thumbnail");
    assert!(matches!(error, SourceError::Restricted { .. }));
    assert!(handle.take_sent().is_empty());
}

#[test]
fn an_attachment_with_no_preview_answers_none_without_a_request() {
    // A single-size photo has no separate thumbnail, and no minithumbnail.
    let single_size_photo = wire_message(json!({
        "@type": "messagePhoto",
        "caption": {"text": ""},
        "photo": {
            "sizes": [
                {"type": "y", "width": 1280, "height": 960,
                 "photo": {"id": MEDIA_FILE_ID, "size": 200_000, "remote": remote("media")}},
            ],
        },
    }));
    let catalog = TestCatalog::with(target_of(&single_size_photo));
    let (_runtime, handle, adapter) = thumbnailer(catalog);

    let answer = block_on(adapter.thumbnail(item(), spec(256))).expect("resolves");
    assert_eq!(answer, None, "no preview, no thumbnail — a normal answer");
    assert!(handle.take_sent().is_empty(), "no preview means no request");
}

#[test]
fn an_unknown_item_answers_none() {
    let catalog: Arc<TestCatalog> = Arc::new(TestCatalog::default());
    let (_runtime, handle, adapter) = thumbnailer(catalog);
    let answer = block_on(adapter.thumbnail(item(), spec(256))).expect("resolves");
    assert_eq!(answer, None);
    assert!(handle.take_sent().is_empty());
}

// ---------------------------------------------------------------------------
// The inline minithumbnail (zero-network fallback)
// ---------------------------------------------------------------------------

#[test]
fn the_inline_minithumbnail_is_served_without_a_download() {
    // A voice-message-style attachment carrying only an inline blur: no
    // downloadable preview, so the inline blur answers with no network.
    let target = ThumbnailTarget {
        availability: gramdrive_source_tdjson::message::AttachmentAvailability::Fetchable,
        downloadable: None,
        inline: Some(gramdrive_source_tdjson::message::Minithumbnail {
            width: Some(40),
            height: Some(30),
            data_base64: "aGVsbG8=".to_owned(), // "hello"
        }),
    };
    let catalog = TestCatalog::with(target);
    let (_runtime, handle, adapter) = thumbnailer(catalog);

    let thumbnail = block_on(adapter.thumbnail(item(), spec(256)))
        .expect("resolves")
        .expect("the inline blur is served");
    assert_eq!(thumbnail.mime_type(), "image/jpeg");
    assert_eq!(thumbnail.bytes(), b"hello");
    assert!(
        handle.take_sent().is_empty(),
        "the inline blur needs no network"
    );
}

// ---------------------------------------------------------------------------
// Failure classification and cancellation
// ---------------------------------------------------------------------------

#[test]
fn a_flood_wait_surfaces_with_its_stated_delay() {
    let catalog = TestCatalog::with(target_of(&video_message()));
    let (_runtime, handle, adapter) = thumbnailer(catalog);
    handle.set_responder(move |sent: &SentRequest| {
        let extra = sent.extra().expect("@extra");
        vec![error_response(
            extra,
            sent.client_id,
            429,
            "Too Many Requests: retry after 3",
        )]
    });

    let error =
        block_on(adapter.thumbnail(item(), spec(256))).expect_err("the preview download floods");
    match error {
        SourceError::RateLimited { retry_after, .. } => {
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(3)));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

#[test]
fn a_stale_preview_reference_surfaces_as_stale_reference() {
    let catalog = TestCatalog::with(target_of(&document_message()));
    let (_runtime, handle, adapter) = thumbnailer(catalog);
    handle.set_responder(move |sent: &SentRequest| {
        let extra = sent.extra().expect("@extra");
        vec![error_response(
            extra,
            sent.client_id,
            400,
            "FILE_REFERENCE_EXPIRED",
        )]
    });

    let error =
        block_on(adapter.thumbnail(item(), spec(256))).expect_err("the preview reference expired");
    assert!(
        matches!(error, SourceError::StaleReference { .. }),
        "a stale preview reference is retryable-after-refresh, got {error:?}"
    );
}

#[test]
fn an_abandoned_preview_download_fires_the_network_cancel() {
    let catalog = TestCatalog::with(target_of(&video_message()));
    let (_runtime, handle, adapter) = thumbnailer(catalog);
    // No responder: the download never answers, so the request is still
    // awaiting TDLib when the caller lets go of it.

    {
        let mut future = adapter.thumbnail(item(), spec(256));
        assert!(
            exec::poll_n(std::pin::Pin::new(&mut future), 8).is_pending(),
            "the preview download is in flight"
        );
        // Dropping the future is the cancellation (SYNC-005).
    }

    let types: Vec<Option<String>> = handle
        .take_sent()
        .iter()
        .map(SentRequest::request_type)
        .collect();
    assert_eq!(
        types,
        vec![
            Some("downloadFile".to_owned()),
            Some("cancelDownloadFile".to_owned())
        ],
        "abandoning the request tells TDLib to stop the preview download (SYNC-043)"
    );
}

// ---------------------------------------------------------------------------
// Per-preview serialization
// ---------------------------------------------------------------------------

#[test]
fn concurrent_requests_for_one_preview_serialize() {
    let path = tdlib_file(PREVIEW);
    let catalog = TestCatalog::with(target_of(&photo_message()));
    let (_runtime, handle, adapter) = thumbnailer(catalog);
    handle.set_responder(serving_responder(path));

    let (left, right) = block_on(join(
        adapter.thumbnail(item(), spec(256)),
        adapter.thumbnail(item(), spec(256)),
    ));
    let left = left
        .expect("the first request resolves")
        .expect("a thumbnail");
    let right = right
        .expect("the second request resolves")
        .expect("a thumbnail");
    assert_eq!(left.bytes(), PREVIEW);
    assert_eq!(right.bytes(), PREVIEW);

    // One download conversation per file: the per-file lock kept the two
    // requests from displacing each other's synchronous downloads.
    let downloads = handle
        .take_sent()
        .iter()
        .filter(|sent| sent.request_type().as_deref() == Some("downloadFile"))
        .count();
    assert_eq!(downloads, 2, "each request downloads once, serialized");
}

/// A local two-way join (the testkit's `both` is crate-private).
fn join<A: Future, B: Future>(first: A, second: B) -> impl Future<Output = (A::Output, B::Output)> {
    let mut first = Box::pin(first);
    let mut second = Box::pin(second);
    let mut first_out: Option<A::Output> = None;
    let mut second_out: Option<B::Output> = None;
    std::future::poll_fn(move |context| {
        use std::task::Poll;
        if first_out.is_none()
            && let Poll::Ready(value) = first.as_mut().poll(context)
        {
            first_out = Some(value);
        }
        if second_out.is_none()
            && let Poll::Ready(value) = second.as_mut().poll(context)
        {
            second_out = Some(value);
        }
        match (first_out.take(), second_out.take()) {
            (Some(a), Some(b)) => Poll::Ready((a, b)),
            (a, b) => {
                first_out = a;
                second_out = b;
                Poll::Pending
            }
        }
    })
}
