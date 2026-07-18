//! The ranged download adapter, end to end (TASK-260715-1onbmf): the
//! [`TdDownloader`] driver over the real runtime and the mock tdjson,
//! delivering TDLib-cached bytes into a verifying sink.
//!
//! The mock's responder plays TDLib's side of the download protocol — a
//! synchronous `downloadFile` answered with a `file` object whose local
//! state points at a real temporary file this suite wrote — so every test
//! exercises the whole path: catalog gates, request geometry, per-file
//! serialization, local reads, sink delivery, cancellation, and the
//! `FILE_REFERENCE_*` refresh. The temporary files play the role of TDLib's
//! files directory; per the ownership rule the adapter only ever reads
//! them, which the assertions on their untouched content confirm.

// clippy.toml exempts test code keyed on `#[test]` functions; the fixture
// helpers below sit at module level in an integration-test binary. The
// rationale applies in full — this file links into no product artifact
// (established test-suite pattern, common/mod.rs).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use gramdrive_model::ByteRange;
use gramdrive_model::identity::ItemId;
use gramdrive_model::version::ContentVersion;
use gramdrive_source::{FetchRequest, SourceError};
use gramdrive_source_tdjson::message::AttachmentAvailability;
use gramdrive_source_tdjson::mock::SentRequest;
use gramdrive_source_tdjson::{
    CatalogEntry, DownloadConfig, DownloadPriority, FetchCatalog, FileTarget, TdDownloader,
};
use gramdrive_testkit::RecordingSink;
use gramdrive_testkit::exec;
use gramdrive_testkit::fixture;

use common::block_on;

const FILE_ID: i32 = 700;
const CHAT_ID: i64 = 100;
const MESSAGE_ID: i64 = 5;
const PAYLOAD: &[u8] = b"the exact bytes a tdlib download owes the caller, byte for byte";

fn version(token: &str) -> ContentVersion {
    ContentVersion::new(token).expect("valid token")
}

fn item() -> ItemId {
    fixture::attachment_id(fixture::scope(), CHAT_ID, MESSAGE_ID, 0)
}

fn target() -> FileTarget {
    FileTarget {
        file_id: FILE_ID,
        chat_id: CHAT_ID,
        message_id: MESSAGE_ID,
        availability: AttachmentAvailability::Fetchable,
        remote_unique_id: Some("unique-payload".to_owned()),
        size: Some(PAYLOAD.len() as u64),
        version: version("c1"),
    }
}

fn request(start: u64, end: u64) -> FetchRequest {
    FetchRequest {
        item: item(),
        version: version("c1"),
        range: ByteRange::new(start, end).expect("valid range"),
    }
}

/// A scriptable in-test catalog: entries behind a mutex, so a test can move
/// the world mid-fetch.
#[derive(Debug, Default)]
struct TestCatalog {
    entries: Mutex<HashMap<ItemId, CatalogEntry>>,
}

impl TestCatalog {
    fn with_file(entry: CatalogEntry) -> Arc<TestCatalog> {
        let catalog = TestCatalog::default();
        catalog
            .entries
            .lock()
            .expect("fresh mutex")
            .insert(item(), entry);
        Arc::new(catalog)
    }

    fn set(&self, id: ItemId, entry: CatalogEntry) {
        self.entries.lock().expect("test mutex").insert(id, entry);
    }
}

impl FetchCatalog for TestCatalog {
    fn resolve(&self, item: &ItemId) -> Option<CatalogEntry> {
        self.entries.lock().expect("test mutex").get(item).cloned()
    }
}

/// A unique temporary file holding `bytes`, under the cargo-managed test
/// temp dir — the stand-in for TDLib's files directory.
fn tdlib_file(name: &str, bytes: &[u8]) -> PathBuf {
    static UNIQUE: AtomicUsize = AtomicUsize::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "file-download-{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir creates");
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("payload writes");
    path
}

/// The `file` object a covering synchronous download answers with.
fn file_response(extra: u64, client_id: i32, path: &str, offset: u64, prefix: u64) -> String {
    json!({
        "@type": "file",
        "id": FILE_ID,
        "size": PAYLOAD.len(),
        "local": {
            "@type": "localFile",
            "path": path,
            "download_offset": offset,
            "downloaded_prefix_size": prefix,
            "is_downloading_active": false,
            "is_downloading_completed": offset == 0 && prefix >= PAYLOAD.len() as u64,
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

/// The message the `getMessage` refresh answers with: the same attachment,
/// the same locators — a pure reference refresh.
fn refreshed_message(extra: u64, client_id: i32, unique_id: &str) -> String {
    json!({
        "@type": "message",
        "id": MESSAGE_ID,
        "chat_id": CHAT_ID,
        "date": 1_752_800_000,
        "sender_id": {"@type": "messageSenderUser", "user_id": 42},
        "can_be_saved": true,
        "content": {
            "@type": "messageDocument",
            "caption": {"text": ""},
            "document": {
                "file_name": "payload.bin",
                "mime_type": "application/octet-stream",
                "document": {
                    "id": FILE_ID,
                    "size": PAYLOAD.len(),
                    "remote": {"id": "r-1", "unique_id": unique_id},
                },
            },
        },
        "@extra": extra,
        "@client_id": client_id,
    })
    .to_string()
}

/// A responder that answers every `downloadFile` for [`FILE_ID`] with a
/// covering response over `path`, echoing the requested offset/limit.
fn serving_responder(path: PathBuf) -> impl FnMut(&SentRequest) -> Vec<String> + Send + 'static {
    move |sent: &SentRequest| {
        let value: Value = serde_json::from_str(&sent.json).expect("requests are JSON");
        let extra = sent.extra().expect("the runtime injects @extra");
        match value.get("@type").and_then(Value::as_str) {
            Some("downloadFile") => {
                assert_eq!(value.get("file_id").and_then(Value::as_i64), Some(700));
                assert_eq!(
                    value.get("synchronous").and_then(Value::as_bool),
                    Some(true),
                    "the adapter downloads synchronously per range"
                );
                let offset = value.get("offset").and_then(Value::as_u64).expect("offset");
                let limit = value.get("limit").and_then(Value::as_u64).expect("limit");
                vec![file_response(
                    extra,
                    sent.client_id,
                    path.to_str().expect("utf-8 temp path"),
                    offset,
                    limit,
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

/// A runtime, client, and downloader over a fresh mock.
fn downloader(
    catalog: Arc<dyn FetchCatalog>,
    read_chunk_bytes: u64,
) -> (
    gramdrive_source_tdjson::TdRuntime,
    gramdrive_source_tdjson::mock::MockHandle,
    TdDownloader,
) {
    let (runtime, handle) = common::start_runtime(common::test_config());
    let (client, _updates) = runtime.create_client().expect("client registers");
    let config = DownloadConfig {
        priority: DownloadPriority::new(9).expect("9 is in range"),
        read_chunk_bytes: NonZeroU64::new(read_chunk_bytes).expect("non-zero"),
    };
    let adapter = TdDownloader::new(client, catalog, config);
    (runtime, handle, adapter)
}

fn fetch_bytes(
    adapter: &TdDownloader,
    request: FetchRequest,
) -> (Result<(), SourceError>, RecordingSink) {
    let mut sink = RecordingSink::new(request.range);
    let result = block_on(adapter.fetch(request, &mut sink));
    (result, sink)
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

#[test]
fn a_ranged_fetch_delivers_exactly_the_requested_bytes() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 16);
    handle.set_responder(serving_responder(path.clone()));

    let (result, sink) = fetch_bytes(&adapter, request(4, 27));
    result.expect("the ranged fetch succeeds");
    assert_eq!(sink.violation(), None, "delivery honors the contract");
    assert_eq!(sink.bytes(), &PAYLOAD[4..27]);
    assert!(sink.is_complete());

    // The one download request carried the range and the priority through.
    let sent = handle.take_sent();
    let downloads: Vec<Value> = sent
        .iter()
        .filter(|sent| sent.request_type().as_deref() == Some("downloadFile"))
        .map(|sent| serde_json::from_str(&sent.json).expect("JSON"))
        .collect();
    assert_eq!(downloads.len(), 1);
    assert_eq!(downloads[0]["offset"], json!(4));
    assert_eq!(downloads[0]["limit"], json!(23));
    assert_eq!(downloads[0]["priority"], json!(9), "priority passthrough");

    // Temporary-file ownership: TDLib's file was only ever read.
    assert_eq!(
        std::fs::read(&path).expect("the payload file survives"),
        PAYLOAD,
        "the adapter reads TDLib's file in place, never moves or rewrites it"
    );
}

#[test]
fn a_whole_file_fetch_streams_in_bounded_slices() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 8);
    handle.set_responder(serving_responder(path));

    let (result, sink) = fetch_bytes(&adapter, request(0, PAYLOAD.len() as u64));
    result.expect("the whole-extent fetch succeeds");
    assert_eq!(sink.bytes(), PAYLOAD);
    assert!(
        sink.chunks().len() >= PAYLOAD.len() / 8,
        "delivery is sliced to the read cap ({} chunks for {} bytes), \
         never one whole-file buffer",
        sink.chunks().len(),
        PAYLOAD.len()
    );
}

#[test]
fn concurrent_fetches_of_one_file_serialize_and_stay_intact() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 8);
    handle.set_responder(serving_responder(path));

    let extent = PAYLOAD.len() as u64;
    let left = ByteRange::new(0, extent - 4).expect("valid");
    let right = ByteRange::new(4, extent).expect("valid");
    let mut left_sink = RecordingSink::new(left);
    let mut right_sink = RecordingSink::new(right);

    let (left_result, right_result) = block_on(join(
        adapter.fetch(request(0, extent - 4), &mut left_sink),
        adapter.fetch(request(4, extent), &mut right_sink),
    ));

    left_result.expect("the first overlapping fetch succeeds");
    right_result.expect("the second overlapping fetch succeeds");
    assert_eq!(left_sink.bytes(), &PAYLOAD[..PAYLOAD.len() - 4]);
    assert_eq!(right_sink.bytes(), &PAYLOAD[4..]);

    // One download conversation per file: the per-file lock kept the two
    // fetches from displacing each other's synchronous downloads.
    let sent = handle.take_sent();
    let downloads = sent
        .iter()
        .filter(|sent| sent.request_type().as_deref() == Some("downloadFile"))
        .count();
    assert_eq!(downloads, 2, "each fetch downloads once");
}

// ---------------------------------------------------------------------------
// The pre-network gates (POL-4 first among them)
// ---------------------------------------------------------------------------

#[test]
fn restricted_and_view_once_are_refused_before_any_network_call() {
    for availability in [
        AttachmentAvailability::Restricted,
        AttachmentAvailability::ViewOnce,
    ] {
        let catalog = TestCatalog::with_file(CatalogEntry::File(FileTarget {
            availability,
            ..target()
        }));
        let (_runtime, handle, adapter) = downloader(catalog, 16);
        // No responder: any request would panic the receive loop's
        // scripting expectations — and none may exist.

        let (result, sink) = fetch_bytes(&adapter, request(0, 8));
        assert!(
            matches!(result, Err(SourceError::Restricted { .. })),
            "{availability:?} must be refused as Restricted (POL-4)"
        );
        assert!(sink.bytes().is_empty());
        assert!(
            handle.take_sent().is_empty(),
            "POL-4: a restricted attachment costs zero network requests"
        );
    }
}

#[test]
fn the_remaining_gates_answer_typed_errors_without_network() {
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(Arc::clone(&catalog) as Arc<dyn FetchCatalog>, 16);

    // An absent item.
    let absent = FetchRequest {
        item: fixture::attachment_id(fixture::scope(), CHAT_ID, MESSAGE_ID + 1, 0),
        version: version("c1"),
        range: ByteRange::new(0, 8).expect("valid"),
    };
    let mut sink = RecordingSink::new(absent.range);
    assert!(matches!(
        block_on(adapter.fetch(absent, &mut sink)),
        Err(SourceError::NotFound { .. })
    ));

    // A directory.
    let directory_id = fixture::chat_id(fixture::scope(), CHAT_ID);
    catalog.set(directory_id.clone(), CatalogEntry::Directory);
    let dir_request = FetchRequest {
        item: directory_id,
        version: version("c1"),
        range: ByteRange::new(0, 8).expect("valid"),
    };
    let mut sink = RecordingSink::new(dir_request.range);
    assert!(matches!(
        block_on(adapter.fetch(dir_request, &mut sink)),
        Err(SourceError::InvalidRequest { .. })
    ));

    // A stale version pin, with the current version reported.
    let stale = FetchRequest {
        item: item(),
        version: version("c0"),
        range: ByteRange::new(0, 8).expect("valid"),
    };
    let mut sink = RecordingSink::new(stale.range);
    match block_on(adapter.fetch(stale, &mut sink)) {
        Err(SourceError::VersionConflict { current, .. }) => {
            assert_eq!(current, Some(version("c1")));
        }
        other => panic!("expected a version conflict, got {other:?}"),
    }

    // A range past the known extent.
    let past = request(0, PAYLOAD.len() as u64 + 64);
    let mut sink = RecordingSink::new(past.range);
    assert!(matches!(
        block_on(adapter.fetch(past, &mut sink)),
        Err(SourceError::InvalidRequest { .. })
    ));

    assert!(
        handle.take_sent().is_empty(),
        "every gate rejects before the network"
    );
}

// ---------------------------------------------------------------------------
// Failure classification through the wire
// ---------------------------------------------------------------------------

fn failing_once_responder(
    code: i64,
    message: &'static str,
    path: PathBuf,
) -> impl FnMut(&SentRequest) -> Vec<String> + Send + 'static {
    let mut failed = false;
    let mut serve = serving_responder(path);
    move |sent: &SentRequest| {
        if sent.request_type().as_deref() == Some("downloadFile") && !failed {
            failed = true;
            let extra = sent.extra().expect("@extra");
            return vec![error_response(extra, sent.client_id, code, message)];
        }
        serve(sent)
    }
}

#[test]
fn a_flood_wait_surfaces_with_its_stated_delay_and_recovers() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 16);
    handle.set_responder(failing_once_responder(
        429,
        "Too Many Requests: retry after 2",
        path,
    ));

    let (result, sink) = fetch_bytes(&adapter, request(0, 16));
    match result {
        Err(SourceError::RateLimited { retry_after, .. }) => {
            assert_eq!(
                retry_after,
                Some(Duration::from_secs(2)),
                "the flood wait's stated delay crosses the boundary intact"
            );
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
    assert!(sink.bytes().is_empty());

    let (retried, sink) = fetch_bytes(&adapter, request(0, 16));
    retried.expect("the identical request succeeds after the wait");
    assert_eq!(sink.bytes(), &PAYLOAD[..16]);
}

#[test]
fn a_transport_failure_is_unavailable_and_recovers() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 16);
    handle.set_responder(failing_once_responder(500, "Failed to connect", path));

    let (result, _) = fetch_bytes(&adapter, request(0, 16));
    assert!(matches!(result, Err(SourceError::Unavailable { .. })));
    let (retried, sink) = fetch_bytes(&adapter, request(0, 16));
    retried.expect("the source came back");
    assert_eq!(sink.bytes(), &PAYLOAD[..16]);
}

#[test]
fn lost_authorization_is_reported_as_such() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 16);
    handle.set_responder(failing_once_responder(401, "Unauthorized", path));

    let (result, _) = fetch_bytes(&adapter, request(0, 16));
    assert!(matches!(result, Err(SourceError::AuthRequired { .. })));
}

// ---------------------------------------------------------------------------
// The reference refresh (SYNC-045, DOM-007)
// ---------------------------------------------------------------------------

/// downloadFile #1 → `FILE_REFERENCE_EXPIRED`; the refresh `getMessage` is
/// answered with `unique_id`; later downloads serve normally.
fn stale_reference_responder(
    path: PathBuf,
    unique_id: &'static str,
) -> impl FnMut(&SentRequest) -> Vec<String> + Send + 'static {
    let mut expired = false;
    let mut serve = serving_responder(path);
    move |sent: &SentRequest| {
        let extra = sent.extra().expect("@extra");
        match sent.request_type().as_deref() {
            Some("downloadFile") if !expired => {
                expired = true;
                vec![error_response(
                    extra,
                    sent.client_id,
                    400,
                    "FILE_REFERENCE_EXPIRED",
                )]
            }
            Some("getMessage") => {
                vec![refreshed_message(extra, sent.client_id, unique_id)]
            }
            _ => serve(sent),
        }
    }
}

#[test]
fn an_expired_reference_refreshes_surfaces_stale_and_then_recovers() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 16);
    handle.set_responder(stale_reference_responder(path, "unique-payload"));

    let (result, sink) = fetch_bytes(&adapter, request(0, 16));
    assert!(
        matches!(result, Err(SourceError::StaleReference { .. })),
        "the expired reference surfaces as the refreshable class, got {result:?}"
    );
    assert!(sink.bytes().is_empty());

    // The refresh ran before the failure surfaced, in one call.
    let sent = handle.take_sent();
    let types: Vec<Option<String>> = sent.iter().map(SentRequest::request_type).collect();
    assert_eq!(
        types,
        vec![
            Some("downloadFile".to_owned()),
            Some("getMessage".to_owned())
        ],
        "the refresh is invisible to the caller: one failed call, \
         locators re-learned, identity untouched"
    );

    // The retry the StaleReference contract promises now succeeds.
    let (retried, sink) = fetch_bytes(&adapter, request(0, 16));
    retried.expect("the refreshed reference serves");
    assert_eq!(sink.bytes(), &PAYLOAD[..16]);
}

#[test]
fn a_refresh_that_reveals_different_content_is_a_version_conflict() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 16);
    handle.set_responder(stale_reference_responder(path, "unique-other"));

    let (result, sink) = fetch_bytes(&adapter, request(0, 16));
    assert!(
        matches!(result, Err(SourceError::VersionConflict { .. })),
        "content that moved under the reference is a conflict, not a refresh; got {result:?}"
    );
    assert!(sink.bytes().is_empty());
}

// ---------------------------------------------------------------------------
// Version verification mid-fetch (SYNC-042)
// ---------------------------------------------------------------------------

#[test]
fn a_version_that_moves_mid_fetch_conflicts_without_publishing_new_bytes() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(Arc::clone(&catalog) as Arc<dyn FetchCatalog>, 8);

    // After the download response is served, move the catalog on: the
    // next pre-slice re-check must see the conflict.
    let mover = Arc::clone(&catalog);
    let mut serve = serving_responder(path);
    let mut moved = false;
    handle.set_responder(move |sent: &SentRequest| {
        let events = serve(sent);
        if !moved && sent.request_type().as_deref() == Some("downloadFile") {
            moved = true;
            mover.set(
                item(),
                CatalogEntry::File(FileTarget {
                    version: version("c2"),
                    ..target()
                }),
            );
        }
        events
    });

    let (result, sink) = fetch_bytes(&adapter, request(0, 32));
    match result {
        Err(SourceError::VersionConflict { current, .. }) => {
            assert_eq!(current, Some(version("c2")));
        }
        other => panic!("expected a mid-fetch conflict, got {other:?}"),
    }
    assert!(
        sink.bytes().is_empty(),
        "the conflict was observed before the first slice was delivered"
    );
    assert!(!sink.is_complete());
}

#[test]
fn a_race_lost_between_slices_delivers_only_the_pinned_versions_prefix() {
    // The version moves after the fetch has resolved the catalog three
    // times: the gate, the first slice's re-check, the second slice's
    // re-check. With an 8-byte cap that is exactly one delivered slice —
    // pinning the adapter's per-slice verification cadence (SYNC-042).
    #[derive(Debug)]
    struct FlippingCatalog {
        resolves: Mutex<u32>,
    }
    impl FetchCatalog for FlippingCatalog {
        fn resolve(&self, resolved: &ItemId) -> Option<CatalogEntry> {
            assert_eq!(resolved, &item());
            let mut resolves = self.resolves.lock().expect("test mutex");
            *resolves += 1;
            Some(CatalogEntry::File(if *resolves > 2 {
                FileTarget {
                    version: version("c2"),
                    ..target()
                }
            } else {
                target()
            }))
        }
    }

    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = Arc::new(FlippingCatalog {
        resolves: Mutex::new(0),
    });
    let (_runtime, handle, adapter) = downloader(catalog, 8);
    handle.set_responder(serving_responder(path));

    let (result, sink) = fetch_bytes(&adapter, request(0, 32));
    assert!(matches!(result, Err(SourceError::VersionConflict { .. })));
    assert_eq!(
        sink.bytes(),
        &PAYLOAD[..8],
        "exactly the slices delivered under the pin arrived — never a byte \
         observed after the version moved"
    );
    assert!(!sink.is_complete());
    assert_eq!(sink.violation(), None);
}

// ---------------------------------------------------------------------------
// Cancellation (SYNC-005, SYNC-043)
// ---------------------------------------------------------------------------

#[test]
fn a_sink_that_stops_cancels_the_fetch() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 8);
    handle.set_responder(serving_responder(path));

    let fetch = request(0, 32);
    let mut sink = RecordingSink::stopping_after(fetch.range, 0);
    let result = block_on(adapter.fetch(fetch, &mut sink));
    assert!(
        matches!(result, Err(SourceError::Cancelled { .. })),
        "a stopping sink ends the fetch as Cancelled, got {result:?}"
    );
    assert_eq!(sink.violation(), None);
}

#[test]
fn an_abandoned_download_fires_the_network_cancel() {
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 16);
    // No responder: the download never answers, so the fetch is still
    // awaiting TDLib when the caller lets go of it.

    let fetch = request(0, 16);
    let mut sink = RecordingSink::new(fetch.range);
    {
        // The boxed future is itself a (sized) future; pin it by reference
        // to poll it partway.
        let mut future = adapter.fetch(fetch, &mut sink);
        assert!(
            exec::poll_n(std::pin::Pin::new(&mut future), 8).is_pending(),
            "the download is in flight"
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
        "abandoning the fetch tells TDLib to stop the network work (SYNC-043)"
    );
}

// ---------------------------------------------------------------------------
// Local-file breakage stays typed
// ---------------------------------------------------------------------------

#[test]
fn a_missing_local_file_is_unavailable_not_a_crash() {
    let path = tdlib_file("payload.bin", PAYLOAD);
    std::fs::remove_file(&path).expect("the file is removed before the fetch reads it");
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 16);
    handle.set_responder(serving_responder(path));

    let (result, sink) = fetch_bytes(&adapter, request(0, 16));
    assert!(
        matches!(result, Err(SourceError::Unavailable { .. })),
        "a vanished cache file is a retryable source failure, got {result:?}"
    );
    assert!(sink.bytes().is_empty());
}

#[test]
fn a_truncated_local_file_is_unavailable() {
    // The response will claim coverage of 32 bytes; the file holds 10.
    let path = tdlib_file("payload.bin", &PAYLOAD[..10]);
    let catalog = TestCatalog::with_file(CatalogEntry::File(target()));
    let (_runtime, handle, adapter) = downloader(catalog, 16);
    handle.set_responder(serving_responder(path));

    let (result, _) = fetch_bytes(&adapter, request(0, 32));
    assert!(matches!(result, Err(SourceError::Unavailable { .. })));
}

// ---------------------------------------------------------------------------
// A local two-way join (the testkit's `both` is crate-private)
// ---------------------------------------------------------------------------

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
