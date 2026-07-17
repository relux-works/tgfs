//! Behavioral suite for the deterministic fake (TASK-260715-3uft8j).
//!
//! An integration test rather than a `#[cfg(test)]` module on purpose: it
//! links the crate the way `gramdrive-engine` and the conformance suite
//! (TASK-260715-3e8q4m) will, through the public API and nothing else. A
//! fake that could only be driven from inside its own crate would not be
//! the shared fixture this crate exists to provide.
//!
//! What is asserted here is the fake's half of the bargain: that every
//! scripted event is reachable, that each is reproducible, and that a test
//! can read back what the source was actually asked.

// The workspace denies `expect_used`/`panic` because a panic in the core is
// an aborted File Provider extension or a lost error category (NFR-030), and
// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions and
// `#[cfg(test)]` modules, and the fixture helpers below are neither: they sit
// at module level in an integration-test binary. The rationale still applies
// in full — this file is test code and links into no product artifact — so
// the exemption is restated here rather than worked around by threading
// `Result` through helpers whose only failure mode is a typo in a literal.
#![allow(clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::num::{NonZeroU32, NonZeroU64};
use std::pin::pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use gramdrive_testkit::model::ByteRange;
use gramdrive_testkit::model::cursor::ChangeCursor;
use gramdrive_testkit::model::identity::{ChatListKind, ItemId};
use gramdrive_testkit::model::version::ContentVersion;
use gramdrive_testkit::source::{
    ChangePage, DirectoryKind, DriveSource, FetchRequest, FileKind, ItemChange, ItemPage,
    PageRequest, PageToken, RetryAdvice, SourceError, SourceItem, Thumbnail, ThumbnailSpec,
};
use gramdrive_testkit::{
    Call, ChunkPlan, FakeSource, Fault, Occurrence, Operation, Outcome, RecordingSink,
    ScriptBuilder, SourceScript, exec, fixture,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const CONTENT: &[u8] = b"hello world, this is scripted content for a fake source.";

fn root_id() -> ItemId {
    fixture::account_root_id(fixture::scope())
}

fn chat_id(chat: i64) -> ItemId {
    fixture::chat_id(fixture::scope(), chat)
}

fn photo_id() -> ItemId {
    fixture::attachment_id(fixture::scope(), 100, 5, 0)
}

fn version(token: &str) -> ContentVersion {
    ContentVersion::new(token).expect("valid version token")
}

fn root_item() -> SourceItem {
    fixture::directory(root_id(), None, "Account", "m1", DirectoryKind::Root)
        .expect("valid fixture")
}

fn chat_item(chat: i64, name: &str, version: &str) -> SourceItem {
    fixture::directory(
        chat_id(chat),
        Some(root_id()),
        name,
        version,
        DirectoryKind::Chat,
    )
    .expect("valid fixture")
}

fn photo_item(metadata: &str, content: &str, size: u64) -> SourceItem {
    fixture::file(
        photo_id(),
        chat_id(100),
        "photo.jpg",
        metadata,
        content,
        size,
        FileKind::Attachment,
    )
    .expect("valid fixture")
}

/// A root, one chat, and one fetchable photo inside it.
fn base_script() -> ScriptBuilder {
    SourceScript::builder(fixture::scope())
        .items([
            root_item(),
            chat_item(100, "Team", "m2"),
            photo_item("m3", "c1", CONTENT.len() as u64),
        ])
        .content(&photo_id(), version("c1"), CONTENT)
}

/// A root with `count` chats and nothing else — for paging.
fn wide_script(count: i64) -> ScriptBuilder {
    let chats = (0..count).map(|index| chat_item(100 + index, &format!("Chat {index}"), "m2"));
    SourceScript::builder(fixture::scope())
        .item(root_item())
        .items(chats)
}

fn fake(builder: ScriptBuilder) -> FakeSource {
    FakeSource::new(builder.build().expect("valid script"))
}

fn page_request(max: u32) -> PageRequest {
    PageRequest::first(NonZeroU32::new(max).expect("non-zero"))
}

fn full_range() -> ByteRange {
    ByteRange::new(0, CONTENT.len() as u64).expect("valid range")
}

fn fetch_request(range: ByteRange) -> FetchRequest {
    FetchRequest {
        item: photo_id(),
        version: version("c1"),
        range,
    }
}

fn enumerate(source: &FakeSource, parent: &ItemId, max: u32) -> Result<Vec<ItemPage>, SourceError> {
    let mut pages = Vec::new();
    let mut request = page_request(max);
    loop {
        let page = exec::drive(source.children(parent.clone(), request.clone()))?;
        let next = page.next.clone();
        pages.push(page);
        match next {
            Some(token) => {
                request = PageRequest {
                    continuation: Some(token),
                    max_items: NonZeroU32::new(max).expect("non-zero"),
                };
            }
            None => return Ok(pages),
        }
    }
}

// ---------------------------------------------------------------------------
// Shape
// ---------------------------------------------------------------------------

#[test]
fn the_fake_is_usable_through_the_trait_object() {
    // DEC-003: the engine selects between implementations at runtime, so
    // the fake has to work through the same `dyn` boundary a real source
    // does — not just through its concrete type.
    let source: Box<dyn DriveSource> = Box::new(fake(base_script()));
    assert_eq!(source.scope(), fixture::scope());

    let root = exec::drive(source.root()).expect("root resolves");
    assert_eq!(root.display_name, "Account");
    assert_eq!(root.parent, None);
    assert!(root.is_directory());
}

/// An executor that polls only when it has actually been woken.
///
/// `exec::drive` passes a noop waker and re-polls regardless, so it cannot
/// tell a future that parks correctly from one that parks forever. A real
/// runtime can, and does — by hanging. This one holds the fake to the real
/// rule: return `Pending` without arranging a wake and the next poll never
/// comes.
fn drive_honoring_wakes<T>(future: impl Future<Output = T>) -> T {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Context, Wake, Waker};

    struct Woken(AtomicBool);
    impl Wake for Woken {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    // Starts set: the first poll is owed to every future.
    let woken = Arc::new(Woken(AtomicBool::new(true)));
    let waker = Waker::from(Arc::clone(&woken));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);

    for _ in 0..10_000 {
        assert!(
            woken.0.swap(false, Ordering::SeqCst),
            "the future returned Pending without arranging a wake; a real \
             runtime would park it forever"
        );
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
    panic!("future did not settle within 10000 woken polls");
}

#[test]
fn every_yield_wakes_itself_so_a_real_runtime_can_drive_the_fake() {
    // The claim `exec`'s noop-waker loop cannot check, and the one the
    // engine's tokio will rely on: every park here is a `yield_now`, not a
    // wait on a timer this crate does not have.
    let source = fake(
        base_script()
            .chunks(ChunkPlan::Fixed(NonZeroU64::new(4).expect("non-zero")))
            .fault(Fault::on(Operation::Fetch).delay(3)),
    );

    let mut sink = RecordingSink::new(full_range());
    drive_honoring_wakes(source.fetch(fetch_request(full_range()), &mut sink))
        .expect("delivery completes under a waker-respecting executor");

    assert_eq!(sink.bytes(), CONTENT);
    assert!(sink.is_complete());
}

#[test]
fn the_source_serves_the_scripts_scope() {
    let source = fake(base_script());
    assert_eq!(source.scope(), fixture::scope());
    assert_eq!(source.script().root_id(), &root_id());
    assert_eq!(source.revision(), 0);
}

// ---------------------------------------------------------------------------
// Enumeration and snapshot paging (SYNC-003)
// ---------------------------------------------------------------------------

#[test]
fn enumeration_covers_every_child_exactly_once() {
    let source = fake(wide_script(7));
    let pages = enumerate(&source, &root_id(), 3).expect("enumeration succeeds");

    assert_eq!(pages.len(), 3, "7 children at 3 per page");
    let names: Vec<_> = pages
        .iter()
        .flat_map(|page| page.items.iter().map(|item| item.display_name.clone()))
        .collect();
    assert_eq!(
        names,
        (0..7).map(|i| format!("Chat {i}")).collect::<Vec<_>>(),
        "no duplicate, no gap, stable order"
    );
    assert_eq!(pages[0].items.len(), 3);
    assert_eq!(pages[2].items.len(), 1, "the last page is short");
    assert_eq!(pages[2].next, None, "a complete enumeration ends with None");
}

#[test]
fn every_page_of_one_enumeration_reports_the_same_snapshot() {
    let source = fake(wide_script(7));
    let pages = enumerate(&source, &root_id(), 2).expect("enumeration succeeds");

    let snapshots: Vec<_> = pages.iter().map(|page| page.snapshot.clone()).collect();
    let first = &snapshots[0];
    assert!(
        snapshots.iter().all(|snapshot| snapshot == first),
        "SYNC-003: one enumeration is one snapshot, got {snapshots:?}"
    );
    assert_eq!(first.as_str(), "m1", "the snapshot is the parent's version");
}

#[test]
fn a_page_size_of_one_still_enumerates_completely() {
    let source = fake(wide_script(4));
    let pages = enumerate(&source, &root_id(), 1).expect("enumeration succeeds");
    assert_eq!(pages.len(), 4);
    assert!(pages.iter().all(|page| page.items.len() == 1));
}

#[test]
fn a_page_larger_than_the_listing_completes_in_one_page() {
    let source = fake(wide_script(3));
    let pages = enumerate(&source, &root_id(), 1000).expect("enumeration succeeds");
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].items.len(), 3);
    assert_eq!(pages[0].next, None);
}

#[test]
fn an_empty_directory_enumerates_to_an_empty_page() {
    let source = fake(base_script());
    let pages = enumerate(&source, &photo_id(), 10);
    assert!(pages.is_err(), "a file is not enumerable");

    // A directory with no children, on the other hand, is a normal answer.
    let source = fake(wide_script(0));
    let pages = enumerate(&source, &root_id(), 10).expect("an empty listing is not an error");
    assert_eq!(pages.len(), 1);
    assert!(pages[0].items.is_empty());
    assert_eq!(pages[0].next, None);
}

#[test]
fn advancing_mid_enumeration_rejects_the_continuation() {
    // SYNC-003: a source that can no longer serve a snapshot must reject
    // the continuation rather than splice two states into one listing.
    let source =
        fake(wide_script(7).batch([ItemChange::Upserted(chat_item(200, "Newcomer", "m9"))]));

    let first = exec::drive(source.children(root_id(), page_request(3))).expect("first page");
    let token = first.next.clone().expect("more pages remain");

    assert!(source.advance(), "the source moves on mid-enumeration");

    let error = exec::drive(source.children(
        root_id(),
        PageRequest {
            continuation: Some(token),
            max_items: NonZeroU32::new(3).expect("non-zero"),
        },
    ))
    .expect_err("the snapshot is gone");
    assert!(
        matches!(error, SourceError::CursorRejected { .. }),
        "got {error:?}"
    );
    assert_eq!(
        error.retry_advice(),
        RetryAdvice::AfterRebaseline,
        "SYNC-004: recovery is a fresh baseline"
    );
}

#[test]
fn a_re_enumeration_after_a_change_sees_the_new_state() {
    let source =
        fake(wide_script(2).batch([ItemChange::Upserted(chat_item(200, "Newcomer", "m9"))]));
    assert_eq!(
        enumerate(&source, &root_id(), 10).expect("before")[0]
            .items
            .len(),
        2
    );

    source.advance();
    let after = enumerate(&source, &root_id(), 10).expect("after");
    assert_eq!(after[0].items.len(), 3);
    assert_eq!(after[0].items[2].display_name, "Newcomer");
}

#[test]
fn enumerating_a_file_is_an_invalid_request() {
    let source = fake(base_script());
    let error = exec::drive(source.children(photo_id(), page_request(10)))
        .expect_err("a file has no children");
    assert!(
        matches!(error, SourceError::InvalidRequest { .. }),
        "got {error:?}"
    );
    assert_eq!(error.retry_advice(), RetryAdvice::Never);
}

#[test]
fn enumerating_an_unknown_item_is_not_found() {
    let source = fake(base_script());
    let error =
        exec::drive(source.children(chat_id(999), page_request(10))).expect_err("no such item");
    assert!(
        matches!(error, SourceError::NotFound { .. }),
        "got {error:?}"
    );
}

#[test]
fn a_foreign_page_token_is_rejected() {
    let source = fake(wide_script(4));
    let error = exec::drive(source.children(
        root_id(),
        PageRequest {
            continuation: Some(PageToken::new("some-other-sources-token").expect("valid token")),
            max_items: NonZeroU32::new(2).expect("non-zero"),
        },
    ))
    .expect_err("this source did not mint that");
    assert!(
        matches!(error, SourceError::CursorRejected { .. }),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// The change feed (SYNC-004, SYNC-022)
// ---------------------------------------------------------------------------

fn changes(source: &FakeSource, cursor: ChangeCursor) -> Result<ChangePage, SourceError> {
    exec::drive(source.changes(cursor))
}

#[test]
fn a_drained_feed_reports_no_changes() {
    let source = fake(base_script());
    let cursor = exec::drive(source.latest_cursor()).expect("cursor resolves");
    assert_eq!(cursor.scope(), fixture::scope());

    let page = changes(&source, cursor.clone()).expect("a level cursor is served");
    assert!(page.changes.is_empty());
    assert!(!page.more_available);
    assert_eq!(page.next, cursor, "a drained feed does not move the cursor");
}

#[test]
fn the_feed_serves_one_batch_per_page_in_order() {
    let source = fake(
        base_script()
            .batch([ItemChange::Upserted(chat_item(200, "Second", "m9"))])
            .batch([ItemChange::Removed(photo_id())]),
    );

    let baseline = exec::drive(source.latest_cursor()).expect("cursor resolves");
    assert_eq!(source.advance_all(), 2, "both batches land");

    let first = changes(&source, baseline).expect("first page");
    assert_eq!(first.changes.len(), 1);
    assert!(
        matches!(&first.changes[0], ItemChange::Upserted(item) if item.display_name == "Second")
    );
    assert!(first.more_available, "one batch still pending");

    let second = changes(&source, first.next.clone()).expect("second page");
    assert_eq!(second.changes, vec![ItemChange::Removed(photo_id())]);
    assert!(!second.more_available, "the feed is drained");

    let third = changes(&source, second.next.clone()).expect("third page");
    assert!(third.changes.is_empty());
    assert_eq!(third.next, second.next);
}

#[test]
fn a_cursor_survives_its_durable_round_trip() {
    // SYNC-022: the cursor is persisted and restored, so what the state
    // store writes must be what the source accepts back.
    let source = fake(base_script().batch([ItemChange::Removed(photo_id())]));
    let cursor = exec::drive(source.latest_cursor()).expect("cursor resolves");
    source.advance();

    let encoded = cursor.encode();
    let restored = ChangeCursor::decode(&encoded).expect("a minted cursor decodes");
    assert_eq!(restored, cursor);

    let page = changes(&source, restored).expect("a restored cursor is served");
    assert_eq!(page.changes.len(), 1);
}

#[test]
fn an_empty_payload_cursor_reads_the_feed_from_its_start() {
    let source = fake(base_script().batch([ItemChange::Removed(photo_id())]));
    source.advance();

    let from_nothing =
        ChangeCursor::new(fixture::scope(), Vec::new()).expect("an empty payload is valid");
    let page = changes(&source, from_nothing).expect("'nothing observed yet' is position zero");
    assert_eq!(page.changes.len(), 1);
}

#[test]
fn a_foreign_scope_cursor_is_rejected() {
    let source = fake(base_script());
    let foreign =
        ChangeCursor::new(fixture::foreign_scope(), b"rev:0".to_vec()).expect("valid payload");
    let error = changes(&source, foreign).expect_err("SYNC-004: another account's cursor");
    assert!(
        matches!(error, SourceError::CursorRejected { .. }),
        "got {error:?}"
    );
}

#[test]
fn a_retired_namespace_cursor_is_rejected() {
    let source = fake(base_script());
    let retired =
        ChangeCursor::new(fixture::retired_scope(), b"rev:0".to_vec()).expect("valid payload");
    let error = changes(&source, retired).expect_err("SYNC-004: a retired identity namespace");
    assert!(
        matches!(error, SourceError::CursorRejected { .. }),
        "got {error:?}"
    );
    assert_eq!(error.retry_advice(), RetryAdvice::AfterRebaseline);
}

#[test]
fn a_malformed_cursor_payload_is_rejected() {
    let source = fake(base_script());
    let malformed = ChangeCursor::new(fixture::scope(), b"not-a-position".to_vec())
        .expect("valid payload, wrong format");
    let error = changes(&source, malformed).expect_err("this source did not mint that");
    assert!(
        matches!(error, SourceError::CursorRejected { .. }),
        "got {error:?}"
    );
}

#[test]
fn a_cursor_ahead_of_the_source_is_rejected() {
    let source = fake(base_script().batch([ItemChange::Removed(photo_id())]));
    source.advance_all();
    let ahead = exec::drive(source.latest_cursor()).expect("cursor at revision 1");

    let rewound = FakeSource::new(base_script().build().expect("valid script"));
    let error = changes(&rewound, ahead).expect_err("a position this source has not reached");
    assert!(
        matches!(error, SourceError::CursorRejected { .. }),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Ranged fetch (SYNC-040..046)
// ---------------------------------------------------------------------------

#[test]
fn a_full_range_fetch_delivers_exactly_the_content() {
    let source = fake(base_script());
    let mut sink = RecordingSink::new(full_range());
    exec::drive(source.fetch(fetch_request(full_range()), &mut sink)).expect("delivery completes");

    assert_eq!(sink.bytes(), CONTENT);
    assert!(sink.is_complete());
    assert_eq!(
        sink.violation(),
        None,
        "SYNC-046: the fake honors its own contract"
    );
    assert_eq!(sink.progress().delivered(), CONTENT.len() as u64);
    assert_eq!(sink.progress().remaining(), 0);
}

#[test]
fn a_partial_range_fetch_delivers_exactly_that_slice() {
    let source = fake(base_script());
    let range = ByteRange::new(6, 11).expect("valid range");
    let mut sink = RecordingSink::new(range);
    exec::drive(source.fetch(fetch_request(range), &mut sink)).expect("delivery completes");

    assert_eq!(sink.bytes(), b"world");
    assert!(sink.is_complete());
    assert_eq!(
        sink.chunks().first().map(|chunk| chunk.start()),
        Some(6),
        "delivery starts at the range, not at zero"
    );
}

#[test]
fn a_range_past_the_extent_is_an_invalid_request() {
    let source = fake(base_script());
    let beyond = ByteRange::new(0, CONTENT.len() as u64 + 1).expect("valid range");
    let mut sink = RecordingSink::new(beyond);
    let error = exec::drive(source.fetch(fetch_request(beyond), &mut sink))
        .expect_err("the item cannot satisfy it");

    assert!(
        matches!(error, SourceError::InvalidRequest { .. }),
        "got {error:?}"
    );
    assert!(sink.bytes().is_empty(), "a rejected fetch delivers nothing");
}

#[test]
fn fetching_a_directory_is_an_invalid_request() {
    let source = fake(base_script());
    let range = ByteRange::new(0, 4).expect("valid range");
    let mut sink = RecordingSink::new(range);
    let error = exec::drive(source.fetch(
        FetchRequest {
            item: chat_id(100),
            version: version("c1"),
            range,
        },
        &mut sink,
    ))
    .expect_err("a directory has no bytes");
    assert!(
        matches!(error, SourceError::InvalidRequest { .. }),
        "got {error:?}"
    );
}

#[test]
fn fetching_an_unknown_item_is_not_found() {
    let source = fake(base_script());
    let range = ByteRange::new(0, 4).expect("valid range");
    let mut sink = RecordingSink::new(range);
    let error = exec::drive(source.fetch(
        FetchRequest {
            item: fixture::attachment_id(fixture::scope(), 100, 999, 0),
            version: version("c1"),
            range,
        },
        &mut sink,
    ))
    .expect_err("no such item");
    assert!(
        matches!(error, SourceError::NotFound { .. }),
        "got {error:?}"
    );
}

#[test]
fn fetching_restricted_content_is_refused_and_delivers_nothing() {
    // POL-4: the item stays visible; its bytes never enter the archive.
    let restricted = fixture::restricted_file(
        photo_id(),
        chat_id(100),
        "protected.jpg",
        "m3",
        "c1",
        16,
        FileKind::Attachment,
    )
    .expect("valid fixture");
    let source = fake(SourceScript::builder(fixture::scope()).items([
        root_item(),
        chat_item(100, "Team", "m2"),
        restricted,
    ]));

    let range = ByteRange::new(0, 4).expect("valid range");
    let mut sink = RecordingSink::new(range);
    let error = exec::drive(source.fetch(fetch_request(range), &mut sink))
        .expect_err("protected content is never served");

    assert!(
        matches!(error, SourceError::Restricted { .. }),
        "got {error:?}"
    );
    assert_eq!(
        error.retry_advice(),
        RetryAdvice::Never,
        "no retry changes it"
    );
    assert!(sink.bytes().is_empty());
}

#[test]
fn a_stale_version_pin_conflicts_before_any_byte_moves() {
    let source = fake(
        base_script()
            .content(&photo_id(), version("c2"), b"replaced content".to_vec())
            .batch([ItemChange::Upserted(photo_item("m4", "c2", 16))]),
    );
    source.advance();

    let mut sink = RecordingSink::new(ByteRange::new(0, 4).expect("valid range"));
    let error = exec::drive(source.fetch(
        FetchRequest {
            item: photo_id(),
            version: version("c1"),
            range: ByteRange::new(0, 4).expect("valid range"),
        },
        &mut sink,
    ))
    .expect_err("the pinned version is gone");

    match error {
        SourceError::VersionConflict { current, .. } => {
            assert_eq!(
                current,
                Some(version("c2")),
                "the source names what it now serves"
            );
        }
        other => panic!("expected a version conflict, got {other:?}"),
    }
    assert!(
        sink.bytes().is_empty(),
        "SYNC-042: no byte of version c2 is delivered under a c1 pin"
    );
}

// `a_version_conflict_asks_for_a_refresh` lived here and tested nothing in
// this crate: it built a `SourceError` by hand and asserted its retry
// advice, which is `gramdrive-source`'s own test (`src/error.rs`), verbatim.
// The fake's obligation is to *produce* that error where the script says so,
// and the tests above assert exactly that.

// ---------------------------------------------------------------------------
// Chunking and seed reproducibility
// ---------------------------------------------------------------------------

#[test]
fn whole_chunking_delivers_one_chunk() {
    let source = fake(base_script().chunks(ChunkPlan::Whole));
    let mut sink = RecordingSink::new(full_range());
    exec::drive(source.fetch(fetch_request(full_range()), &mut sink)).expect("delivery completes");

    assert_eq!(sink.chunks().len(), 1);
    assert_eq!(sink.bytes(), CONTENT);
}

#[test]
fn fixed_chunking_cuts_at_stated_boundaries() {
    let source =
        fake(base_script().chunks(ChunkPlan::Fixed(NonZeroU64::new(10).expect("non-zero"))));
    let mut sink = RecordingSink::new(full_range());
    exec::drive(source.fetch(fetch_request(full_range()), &mut sink)).expect("delivery completes");

    // The cut is fully determined, so state it outright rather than
    // describing it: 56 bytes at 10 apiece is five tens and a six.
    let sizes: Vec<u64> = sink.chunks().iter().map(|chunk| chunk.len()).collect();
    assert_eq!(
        sizes,
        vec![10, 10, 10, 10, 10, 6],
        "every chunk but the last is the stated size, and a short one closes the range"
    );
    assert_eq!(sizes.iter().sum::<u64>(), CONTENT.len() as u64);
    assert_eq!(sink.bytes(), CONTENT);
}

#[test]
fn seeded_chunking_replays_identically_for_one_seed() {
    let boundaries = |seed: u64| {
        let source = fake(base_script().seed(seed).chunks(ChunkPlan::Seeded {
            max: NonZeroU64::new(8).expect("non-zero"),
        }));
        let mut sink = RecordingSink::new(full_range());
        exec::drive(source.fetch(fetch_request(full_range()), &mut sink)).expect("completes");
        assert_eq!(sink.bytes(), CONTENT, "however it is cut, it reassembles");
        assert_eq!(sink.violation(), None);
        sink.chunks().to_vec()
    };

    assert_eq!(boundaries(42), boundaries(42), "one seed, one cut");
    assert_ne!(
        boundaries(42),
        boundaries(43),
        "a different seed cuts differently, or the seed does nothing"
    );
    assert!(
        boundaries(42).len() > 1,
        "an 8-byte cap over {} bytes must produce several chunks",
        CONTENT.len()
    );
}

#[test]
fn seeded_chunks_never_exceed_their_cap() {
    let source = fake(base_script().chunks(ChunkPlan::Seeded {
        max: NonZeroU64::new(4).expect("non-zero"),
    }));
    let mut sink = RecordingSink::new(full_range());
    exec::drive(source.fetch(fetch_request(full_range()), &mut sink)).expect("completes");

    assert!(
        sink.chunks()
            .iter()
            .all(|chunk| (1..=4).contains(&chunk.len())),
        "chunks are within 1..=4, got {:?}",
        sink.chunks()
    );
    assert!(sink.is_complete());
}

#[test]
fn the_same_range_chunks_the_same_way_on_every_fetch() {
    // The chunk seed folds the request, not running generator state: a
    // caller that retries a range must see the same delivery, whatever ran
    // in between.
    let source = fake(base_script().chunks(ChunkPlan::Seeded {
        max: NonZeroU64::new(8).expect("non-zero"),
    }));

    let fetch_once = || {
        let mut sink = RecordingSink::new(full_range());
        exec::drive(source.fetch(fetch_request(full_range()), &mut sink)).expect("completes");
        sink.chunks().to_vec()
    };

    let first = fetch_once();
    // An unrelated fetch of a different range in between.
    let other = ByteRange::new(0, 5).expect("valid range");
    let mut scratch = RecordingSink::new(other);
    exec::drive(source.fetch(fetch_request(other), &mut scratch)).expect("completes");

    assert_eq!(first, fetch_once(), "a retried range chunks identically");
}

// ---------------------------------------------------------------------------
// Version races (SYNC-042)
// ---------------------------------------------------------------------------

#[test]
fn a_version_race_cuts_delivery_and_conflicts() {
    let source = fake(
        base_script()
            .chunks(ChunkPlan::Fixed(NonZeroU64::new(4).expect("non-zero")))
            .fault(Fault::on(Operation::Fetch).version_race(12, Some(version("c2")))),
    );

    let mut sink = RecordingSink::new(full_range());
    let error = exec::drive(source.fetch(fetch_request(full_range()), &mut sink))
        .expect_err("the content moved under the fetch");

    match error {
        SourceError::VersionConflict { current, .. } => {
            assert_eq!(current, Some(version("c2")));
        }
        other => panic!("expected a version conflict, got {other:?}"),
    }
    assert_eq!(
        sink.bytes(),
        &CONTENT[..12],
        "exactly the scripted prefix arrived"
    );
    assert!(
        !sink.is_complete(),
        "SYNC-042: a partial delivery must never look complete"
    );
    assert_eq!(
        sink.violation(),
        None,
        "a cut delivery is still a valid one"
    );
}

#[test]
fn a_race_at_zero_bytes_conflicts_before_delivering() {
    let source = fake(base_script().fault(Fault::on(Operation::Fetch).version_race(0, None)));
    let mut sink = RecordingSink::new(full_range());
    let error = exec::drive(source.fetch(fetch_request(full_range()), &mut sink))
        .expect_err("conflict observed immediately");

    assert!(
        matches!(error, SourceError::VersionConflict { current: None, .. }),
        "got {error:?}"
    );
    assert!(sink.bytes().is_empty());
}

#[test]
fn a_race_records_the_bytes_it_delivered() {
    let source = fake(
        base_script()
            .chunks(ChunkPlan::Fixed(NonZeroU64::new(4).expect("non-zero")))
            .fault(Fault::on(Operation::Fetch).version_race(8, None)),
    );
    let mut sink = RecordingSink::new(full_range());
    let _ = exec::drive(source.fetch(fetch_request(full_range()), &mut sink));

    let interactions = source.interactions();
    assert_eq!(interactions.len(), 1);
    match &interactions[0].outcome {
        Outcome::Failed {
            error: SourceError::VersionConflict { .. },
            delivered,
        } => assert_eq!(
            *delivered, 8,
            "the record names the bytes the race got out before it conflicted"
        ),
        other => panic!("expected a recorded version conflict, got {other:?}"),
    }
    assert_eq!(
        sink.progress().delivered(),
        8,
        "and the sink agrees with the record"
    );
}

// ---------------------------------------------------------------------------
// Scripted failures (SYNC-044)
// ---------------------------------------------------------------------------

#[test]
fn a_first_attempt_failure_recovers_on_retry() {
    let source = fake(
        base_script().fault(
            Fault::on(Operation::Fetch)
                .occurrence(Occurrence::Nth(1))
                .fail(SourceError::Unavailable {
                    detail: "link dropped".to_owned(),
                }),
        ),
    );

    let mut sink = RecordingSink::new(full_range());
    let error = exec::drive(source.fetch(fetch_request(full_range()), &mut sink))
        .expect_err("the first attempt fails");
    assert!(
        matches!(error, SourceError::Unavailable { .. }),
        "got {error:?}"
    );
    assert_eq!(
        error.retry_advice(),
        RetryAdvice::AfterBackoff { minimum: None }
    );

    let mut retry = RecordingSink::new(full_range());
    exec::drive(source.fetch(fetch_request(full_range()), &mut retry)).expect("the retry succeeds");
    assert_eq!(retry.bytes(), CONTENT);
}

#[test]
fn an_always_fault_never_recovers() {
    let source = fake(base_script().fault(Fault::on(Operation::Root).fail(
        SourceError::AuthRequired {
            detail: "session expired".to_owned(),
        },
    )));

    for attempt in 1..=3 {
        let error = exec::drive(source.root()).expect_err("attempt {attempt} fails");
        assert!(
            matches!(error, SourceError::AuthRequired { .. }),
            "attempt {attempt}: got {error:?}"
        );
        assert_eq!(error.retry_advice(), RetryAdvice::AfterReauth);
    }
}

#[test]
fn a_bounded_fault_recovers_after_its_run() {
    let source = fake(
        base_script().fault(
            Fault::on(Operation::LatestCursor)
                .occurrence(Occurrence::FirstN(2))
                .fail(SourceError::Unavailable {
                    detail: "warming up".to_owned(),
                }),
        ),
    );

    for attempt in 1..=2 {
        let error = exec::drive(source.latest_cursor()).expect_err("within the fault's run");
        assert!(
            matches!(error, SourceError::Unavailable { .. }),
            "attempt {attempt} fails with the scripted error, got {error:?}"
        );
    }
    assert!(
        exec::drive(source.latest_cursor()).is_ok(),
        "the third recovers"
    );
}

#[test]
fn a_source_can_break_and_stay_broken() {
    let source = fake(
        base_script().fault(
            Fault::on(Operation::LatestCursor)
                .occurrence(Occurrence::FromNth(2))
                .fail(SourceError::Internal {
                    detail: "it fell over".to_owned(),
                }),
        ),
    );

    assert!(
        exec::drive(source.latest_cursor()).is_ok(),
        "the first call works"
    );
    for attempt in 2..=4 {
        let error = exec::drive(source.latest_cursor()).expect_err("then it never does again");
        assert!(
            matches!(error, SourceError::Internal { .. }),
            "attempt {attempt} fails with the scripted error, got {error:?}"
        );
    }
}

#[test]
fn a_rate_limit_carries_its_backoff_to_the_caller() {
    // SYNC-044/NFR-033: a flood wait's minimum must survive the boundary,
    // or a retry loop has nothing to honor.
    let source = fake(base_script().fault(Fault::on(Operation::Children).fail(
        SourceError::RateLimited {
            retry_after: Some(Duration::from_millis(1500)),
            detail: "FLOOD_WAIT_2".to_owned(),
        },
    )));

    let error = exec::drive(source.children(root_id(), page_request(10)))
        .expect_err("the source is throttling");
    assert_eq!(
        error.retry_advice(),
        RetryAdvice::AfterBackoff {
            minimum: Some(Duration::from_millis(1500))
        }
    );
}

#[test]
fn an_item_filter_targets_only_that_item() {
    let source = fake(
        wide_script(3).fault(Fault::on(Operation::Children).for_item(chat_id(101)).fail(
            SourceError::Restricted {
                detail: "this chat only".to_owned(),
            },
        )),
    );

    assert!(
        exec::drive(source.children(root_id(), page_request(10))).is_ok(),
        "an unfiltered parent is unaffected"
    );
    assert!(
        exec::drive(source.children(chat_id(100), page_request(10))).is_ok(),
        "a sibling is unaffected"
    );
    let error =
        exec::drive(source.children(chat_id(101), page_request(10))).expect_err("the target fails");
    assert!(
        matches!(error, SourceError::Restricted { .. }),
        "the targeted parent fails with the scripted error, got {error:?}"
    );
}

#[test]
fn faults_on_different_occurrences_compose() {
    // Each fault counts the calls that match it, independently of what
    // other faults the script carries.
    let source = fake(
        base_script()
            .fault(
                Fault::on(Operation::Root)
                    .occurrence(Occurrence::Nth(1))
                    .fail(SourceError::Unavailable {
                        detail: "first".to_owned(),
                    }),
            )
            .fault(
                Fault::on(Operation::Root)
                    .occurrence(Occurrence::Nth(2))
                    .fail(SourceError::AuthRequired {
                        detail: "second".to_owned(),
                    }),
            ),
    );

    assert!(matches!(
        exec::drive(source.root()).expect_err("first"),
        SourceError::Unavailable { .. }
    ));
    assert!(matches!(
        exec::drive(source.root()).expect_err("second"),
        SourceError::AuthRequired { .. }
    ));
    assert!(exec::drive(source.root()).is_ok(), "the third is clean");
}

#[test]
fn a_fault_can_break_the_change_feed() {
    // `changes` is where a sync loop lives or dies, and a feed that fails
    // once and recovers is the case the loop has to survive without losing
    // its place — so the cursor it was given must still work on the retry.
    let source = fake(
        base_script()
            .batch([ItemChange::Upserted(chat_item(200, "Newcomer", "m9"))])
            .fault(
                Fault::on(Operation::Changes)
                    .occurrence(Occurrence::Nth(1))
                    .fail(SourceError::Unavailable {
                        detail: "feed dropped".to_owned(),
                    }),
            ),
    );
    let cursor = exec::drive(source.latest_cursor()).expect("cursor resolves");
    source.advance_all();

    let error = exec::drive(source.changes(cursor.clone())).expect_err("the first read fails");
    assert!(
        matches!(error, SourceError::Unavailable { .. }),
        "got {error:?}"
    );

    let page = exec::drive(source.changes(cursor)).expect("the retry reads the feed");
    assert_eq!(page.changes.len(), 1, "the same cursor still serves");

    assert!(
        matches!(
            source.interactions()[1].outcome,
            Outcome::Failed {
                error: SourceError::Unavailable { .. },
                ..
            }
        ),
        "the failed read is on the record"
    );
}

#[test]
fn a_fault_can_delay_and_break_a_thumbnail() {
    // Thumbnails are the one operation whose absence is not an error, so a
    // scripted failure has to be distinguishable from a scripted `None`.
    let source = fake(
        base_script().fault(
            Fault::on(Operation::Thumbnail)
                .for_item(photo_id())
                .delay(2)
                .fail(SourceError::Restricted {
                    detail: "no preview for this one".to_owned(),
                }),
        ),
    );

    let future = pin!(source.thumbnail(photo_id(), spec(128)));
    assert_eq!(
        exec::poll_n(future, 2),
        Poll::Pending,
        "the scripted delay holds the thumbnail pending"
    );

    let error =
        exec::drive(source.thumbnail(photo_id(), spec(128))).expect_err("the scripted fault fires");
    assert!(
        matches!(error, SourceError::Restricted { .. }),
        "a scripted failure is an error, not an absent thumbnail: got {error:?}"
    );

    // The item filter still applies: the root is a directory, so it has no
    // thumbnail, and it says so rather than failing.
    assert_eq!(
        exec::drive(source.thumbnail(root_id(), spec(128))).expect("an unfiltered item is clean"),
        None,
        "absence is not failure"
    );
}

// ---------------------------------------------------------------------------
// Delays and cancellation (SYNC-005, SYNC-043)
// ---------------------------------------------------------------------------

#[test]
fn a_delay_holds_the_call_pending_for_exactly_its_yields() {
    let source = fake(base_script().fault(Fault::on(Operation::Root).delay(3)));

    let future = pin!(source.root());
    assert_eq!(
        exec::poll_n(future, 3),
        Poll::Pending,
        "three yields are not done in three polls"
    );

    let future = pin!(source.root());
    match exec::poll_n(future, 4) {
        Poll::Ready(result) => {
            result.expect("the fourth poll resolves");
        }
        Poll::Pending => panic!("a delay of 3 must resolve on the fourth poll"),
    }
}

#[test]
fn dropping_a_delayed_call_records_cancellation() {
    let source = fake(base_script().fault(Fault::on(Operation::Root).delay(5)));

    {
        let mut future = Box::pin(source.root());
        assert_eq!(exec::poll_n(future.as_mut(), 2), Poll::Pending);
        drop(future);
    }

    let interactions = source.interactions();
    assert_eq!(interactions.len(), 1);
    assert_eq!(interactions[0].call, Call::Root);
    assert_eq!(
        interactions[0].outcome,
        Outcome::Cancelled { delivered: 0 },
        "a call dropped before delivering anything reports nothing delivered"
    );
}

#[test]
fn dropping_a_fetch_mid_delivery_records_how_far_it_got() {
    // The heart of SYNC-043: a dropped future is cancellation, and the
    // side effect a test needs to see is how many bytes had already left.
    let source =
        fake(base_script().chunks(ChunkPlan::Fixed(NonZeroU64::new(4).expect("non-zero"))));
    let mut sink = RecordingSink::new(full_range());

    {
        let mut future = Box::pin(source.fetch(fetch_request(full_range()), &mut sink));
        // Each chunk costs a poll to deliver and a poll to clear its yield.
        assert_eq!(exec::poll_n(future.as_mut(), 5), Poll::Pending);
        drop(future);
    }

    let interactions = source.interactions();
    assert_eq!(interactions.len(), 1);
    let delivered = match interactions[0].outcome {
        Outcome::Cancelled { delivered } => delivered,
        ref other => panic!("expected cancellation, got {other:?}"),
    };

    // Poll accounting is the premise of this crate, so the count is exact
    // rather than bounded: five polls over 4-byte chunks, each chunk
    // costing a poll to deliver and a poll to clear its yield, is 20 bytes.
    assert_eq!(
        delivered, 20,
        "the source stopped where the polling stopped, not one byte later"
    );
    assert_eq!(
        delivered,
        sink.bytes().len() as u64,
        "the recorded byte count is what the sink actually received"
    );
    assert!(!sink.is_complete());
}

#[test]
fn a_sink_that_stops_cancels_the_fetch_in_band() {
    // The other cancellation path: hosts whose cancellation arrives as a
    // callback rather than a dropped task.
    let source =
        fake(base_script().chunks(ChunkPlan::Fixed(NonZeroU64::new(4).expect("non-zero"))));
    let mut sink = RecordingSink::stopping_after(full_range(), 2);

    let error = exec::drive(source.fetch(fetch_request(full_range()), &mut sink))
        .expect_err("the sink asked to stop");
    assert!(
        matches!(error, SourceError::Cancelled { .. }),
        "got {error:?}"
    );
    assert_eq!(error.retry_advice(), RetryAdvice::Never);

    assert_eq!(
        sink.bytes(),
        &CONTENT[..12],
        "three chunks of four were taken"
    );
    assert!(!sink.is_complete());

    let interactions = source.interactions();
    match &interactions[0].outcome {
        Outcome::Failed {
            error: SourceError::Cancelled { .. },
            delivered,
        } => assert_eq!(
            *delivered, 12,
            "the record names the three chunks that got out before the stop"
        ),
        other => panic!("an in-band stop resolves the call, it does not drop it: {other:?}"),
    }
}

#[test]
fn dropping_an_unpolled_future_still_records_the_call() {
    let source = fake(base_script());
    drop(source.children(root_id(), page_request(10)));

    let interactions = source.interactions();
    assert_eq!(
        interactions.len(),
        1,
        "the call was made, whether or not it was polled"
    );
    assert!(interactions[0].outcome.is_cancelled());
}

// ---------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------

fn spec(size: u32) -> ThumbnailSpec {
    ThumbnailSpec {
        max_width_px: NonZeroU32::new(size).expect("non-zero"),
        max_height_px: NonZeroU32::new(size).expect("non-zero"),
    }
}

#[test]
fn an_item_without_a_thumbnail_answers_none() {
    let source = fake(base_script());
    let answer = exec::drive(source.thumbnail(photo_id(), spec(256))).expect("resolves");
    assert_eq!(answer, None, "'no thumbnail' is an answer, not an error");
}

#[test]
fn a_scripted_thumbnail_is_served() {
    let thumbnail = Thumbnail::new("image/jpeg", vec![0xff, 0xd8, 0xff]).expect("valid thumbnail");
    let source = fake(base_script().thumbnail(&photo_id(), thumbnail.clone()));

    let answer = exec::drive(source.thumbnail(photo_id(), spec(256))).expect("resolves");
    assert_eq!(answer, Some(thumbnail));
}

#[test]
fn a_thumbnail_of_restricted_content_is_refused() {
    // POL-4: restricted content is restricted through every door.
    let restricted = fixture::restricted_file(
        photo_id(),
        chat_id(100),
        "protected.jpg",
        "m3",
        "c1",
        16,
        FileKind::Attachment,
    )
    .expect("valid fixture");
    let source = fake(SourceScript::builder(fixture::scope()).items([
        root_item(),
        chat_item(100, "Team", "m2"),
        restricted,
    ]));

    let error = exec::drive(source.thumbnail(photo_id(), spec(256)))
        .expect_err("no door serves protected bytes");
    assert!(
        matches!(error, SourceError::Restricted { .. }),
        "got {error:?}"
    );
}

#[test]
fn a_thumbnail_of_an_unknown_item_is_not_found() {
    let source = fake(base_script());
    let error = exec::drive(source.thumbnail(chat_id(999), spec(64))).expect_err("no such item");
    assert!(
        matches!(error, SourceError::NotFound { .. }),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Recording (the "assert exact calls" half of the AC)
// ---------------------------------------------------------------------------

#[test]
fn every_call_is_recorded_in_order_with_its_arguments() {
    let source = fake(base_script());
    let range = ByteRange::new(4, 12).expect("valid range");

    let _ = exec::drive(source.root());
    let _ = exec::drive(source.children(root_id(), page_request(5)));
    let cursor = exec::drive(source.latest_cursor()).expect("cursor resolves");
    let _ = exec::drive(source.changes(cursor.clone()));
    let mut sink = RecordingSink::new(range);
    let _ = exec::drive(source.fetch(fetch_request(range), &mut sink));
    let _ = exec::drive(source.thumbnail(photo_id(), spec(128)));

    let calls = source.calls();
    assert_eq!(
        calls,
        vec![
            Call::Root,
            Call::Children {
                parent: root_id(),
                request: page_request(5),
            },
            Call::LatestCursor,
            Call::Changes { cursor },
            // The richest arguments in the log: a fetch names the item, the
            // version the caller pinned, and the byte range it asked for.
            // A caller that silently widened its range or dropped its pin is
            // exactly the bug this record exists to catch.
            Call::Fetch {
                request: FetchRequest {
                    item: photo_id(),
                    version: version("c1"),
                    range,
                },
            },
            Call::Thumbnail {
                item: photo_id(),
                spec: spec(128),
            },
        ]
    );
}

#[test]
fn a_recorded_call_reports_the_page_size_the_caller_asked_for() {
    let source = fake(wide_script(5));
    let _ = enumerate(&source, &root_id(), 2);

    let sizes: Vec<u32> = source
        .calls()
        .iter()
        .filter_map(|call| match call {
            Call::Children { request, .. } => Some(request.max_items.get()),
            _ => None,
        })
        .collect();
    assert_eq!(sizes, vec![2, 2, 2], "three pages, each asked for two");

    let continuations: Vec<bool> = source
        .calls()
        .iter()
        .filter_map(|call| match call {
            Call::Children { request, .. } => Some(request.continuation.is_some()),
            _ => None,
        })
        .collect();
    assert_eq!(
        continuations,
        vec![false, true, true],
        "the first page starts the enumeration; the rest continue it"
    );
}

#[test]
fn outcomes_distinguish_success_failure_and_cancellation() {
    let source = fake(
        base_script().fault(
            Fault::on(Operation::LatestCursor)
                .occurrence(Occurrence::Nth(1))
                .fail(SourceError::Unavailable {
                    detail: "offline".to_owned(),
                }),
        ),
    );

    let _ = exec::drive(source.root());
    let _ = exec::drive(source.latest_cursor());
    drop(source.root());

    let outcomes: Vec<Outcome> = source
        .interactions()
        .into_iter()
        .map(|interaction| interaction.outcome)
        .collect();

    assert!(outcomes[0].is_ok(), "root resolved");
    assert!(
        matches!(
            &outcomes[1],
            Outcome::Failed {
                error: SourceError::Unavailable { .. },
                ..
            }
        ),
        "the scripted failure is recorded as failed, got {:?}",
        outcomes[1]
    );
    assert!(
        outcomes[2].is_cancelled(),
        "the dropped future is recorded as cancelled"
    );
}

#[test]
fn interactions_can_be_cleared_between_phases() {
    let source = fake(base_script());
    let _ = exec::drive(source.root());
    assert_eq!(source.interactions().len(), 1);

    source.clear_interactions();
    assert!(source.interactions().is_empty());

    let _ = exec::drive(source.latest_cursor());
    assert_eq!(source.calls(), vec![Call::LatestCursor]);
}

#[test]
fn a_call_still_in_flight_across_a_clear_cannot_rewrite_a_later_one() {
    // Regression: the setup phase a test clears away may still hold a live
    // future, and its guard used to settle by position into the fresh log —
    // stamping this fetch's `Cancelled` onto the unrelated `root()` that
    // inherited index 0. The wrong answer was a *plausible* one, which is
    // the failure mode a fixture whose product is evidence must not have.
    let source = fake(base_script());
    let mut sink = RecordingSink::new(full_range());
    let pending = source.fetch(fetch_request(full_range()), &mut sink);

    source.clear_interactions();

    let item = exec::drive(source.root()).expect("root resolves");
    drop(pending);

    let interactions = source.interactions();
    assert_eq!(
        interactions.len(),
        1,
        "the cleared fetch does not come back: {interactions:?}"
    );
    assert_eq!(interactions[0].call, Call::Root);
    assert_eq!(
        interactions[0].outcome,
        Outcome::Ok,
        "root() succeeded and the record has to keep saying so"
    );
    assert_eq!(item.display_name, "Account");
}

#[test]
fn a_call_cleared_while_in_flight_is_not_resurrected_by_its_own_drop() {
    // The other half of the same defect: with nothing left to overwrite,
    // the stale outcome must vanish rather than reappear in an empty log.
    let source = fake(base_script());
    let pending = source.root();
    source.clear_interactions();
    drop(pending);

    assert!(
        source.interactions().is_empty(),
        "a cleared call stays cleared: {:?}",
        source.interactions()
    );
}

#[test]
fn two_sources_sharing_a_script_record_and_advance_independently() {
    let script = Arc::new(
        base_script()
            .batch([ItemChange::Upserted(chat_item(200, "Newcomer", "m9"))])
            .build()
            .expect("valid script"),
    );
    let left = FakeSource::from_shared(Arc::clone(&script));
    let right = FakeSource::from_shared(script);

    let _ = exec::drive(left.root());
    left.advance();

    assert_eq!(left.revision(), 1);
    assert_eq!(right.revision(), 0, "advancing one does not move the other");
    assert_eq!(left.calls().len(), 1);
    assert!(right.calls().is_empty(), "recordings are per source");
}

// ---------------------------------------------------------------------------
// Revision control
// ---------------------------------------------------------------------------

#[test]
fn advance_stops_at_the_end_of_the_script() {
    let source = fake(base_script().batch([ItemChange::Removed(photo_id())]));
    assert_eq!(source.revision(), 0);
    assert!(source.advance(), "one batch to apply");
    assert_eq!(source.revision(), 1);
    assert!(!source.advance(), "the feed is drained");
    assert_eq!(source.revision(), 1, "a drained advance changes nothing");
}

#[test]
fn advance_to_moves_forward_only() {
    let source = fake(
        base_script()
            .batch([ItemChange::Upserted(chat_item(200, "A", "m9"))])
            .batch([ItemChange::Upserted(chat_item(201, "B", "m9"))]),
    );

    assert!(source.advance_to(2), "jumping ahead is fine");
    assert_eq!(source.revision(), 2);
    assert!(!source.advance_to(1), "a change feed does not rewind");
    assert!(!source.advance_to(99), "and cannot outrun its script");
    assert_eq!(source.revision(), 2);
}

#[test]
fn advance_all_reaches_the_last_revision() {
    let source = fake(
        base_script()
            .batch([ItemChange::Upserted(chat_item(200, "A", "m9"))])
            .batch([ItemChange::Upserted(chat_item(201, "B", "m9"))]),
    );
    assert_eq!(source.advance_all(), 2);
    assert_eq!(source.script().batch_count(), 2);
}

#[test]
fn a_removed_item_stops_being_served() {
    let source = fake(base_script().batch([ItemChange::Removed(photo_id())]));
    assert!(exec::drive(source.thumbnail(photo_id(), spec(64))).is_ok());

    source.advance();
    let error = exec::drive(source.thumbnail(photo_id(), spec(64)))
        .expect_err("SYNC-025: the source deleted it");
    assert!(
        matches!(error, SourceError::NotFound { .. }),
        "got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// End-to-end determinism
// ---------------------------------------------------------------------------

#[test]
fn an_identical_script_replays_identically() {
    // The claim the whole crate rests on. Two sources, same script, same
    // calls: byte-identical deliveries and byte-identical recordings.
    let run = || {
        let source = fake(
            base_script()
                .seed(0xfeed_face)
                .chunks(ChunkPlan::Seeded {
                    max: NonZeroU64::new(7).expect("non-zero"),
                })
                .batch([ItemChange::Upserted(chat_item(200, "Newcomer", "m9"))])
                .fault(
                    Fault::on(Operation::Root)
                        .occurrence(Occurrence::Nth(2))
                        .delay(2)
                        .fail(SourceError::RateLimited {
                            retry_after: Some(Duration::from_millis(500)),
                            detail: "FLOOD_WAIT_1".to_owned(),
                        }),
                ),
        );

        let _ = exec::drive(source.root());
        let _ = exec::drive(source.root());
        let cursor = exec::drive(source.latest_cursor()).expect("cursor resolves");
        source.advance();
        let _ = exec::drive(source.changes(cursor));
        let _ = enumerate(&source, &root_id(), 2);

        let mut sink = RecordingSink::new(full_range());
        let _ = exec::drive(source.fetch(fetch_request(full_range()), &mut sink));
        drop(source.root());

        (
            source.interactions(),
            sink.bytes().to_vec(),
            sink.chunks().to_vec(),
        )
    };

    let (first_log, first_bytes, first_chunks) = run();
    let (second_log, second_bytes, second_chunks) = run();

    assert_eq!(first_log, second_log, "the interaction log is reproducible");
    assert_eq!(
        first_bytes, second_bytes,
        "the delivered bytes are reproducible"
    );
    assert_eq!(
        first_chunks, second_chunks,
        "the chunk boundaries are reproducible"
    );
    assert_eq!(first_bytes, CONTENT);
}

#[test]
fn a_full_scripted_scenario_reaches_every_configured_event() {
    // The acceptance criterion end to end: snapshots, pages, changes,
    // ranges, delays, failures, version races and cancellation, all from
    // one script, all reachable — and the whole interaction log asserted
    // exactly, arguments included, because "assert requests" is the half a
    // fixture is easiest to be wrong about.
    let source = fake(
        base_script()
            .chunks(ChunkPlan::Fixed(NonZeroU64::new(8).expect("non-zero")))
            .content(
                &photo_id(),
                version("c2"),
                b"the replacement content!".to_vec(),
            )
            .batch([ItemChange::Upserted(chat_item(200, "Newcomer", "m9"))])
            .batch([ItemChange::Upserted(photo_item("m4", "c2", 24))])
            .fault(
                Fault::on(Operation::Children)
                    .for_item(root_id())
                    .occurrence(Occurrence::Nth(1))
                    .delay(1)
                    .fail(SourceError::RateLimited {
                        retry_after: Some(Duration::from_millis(250)),
                        detail: "FLOOD_WAIT_1".to_owned(),
                    }),
            )
            .fault(
                Fault::on(Operation::Fetch)
                    .occurrence(Occurrence::Nth(2))
                    .version_race(8, Some(version("c2"))),
            )
            // A delay with no failure: the cancellation point step 8 drops in.
            .fault(
                Fault::on(Operation::Children)
                    .for_item(chat_id(100))
                    .delay(2),
            ),
    );

    // 1. A flood wait on the first enumeration, with its backoff intact.
    let throttled =
        exec::drive(source.children(root_id(), page_request(10))).expect_err("scripted flood wait");
    assert_eq!(
        throttled.retry_advice(),
        RetryAdvice::AfterBackoff {
            minimum: Some(Duration::from_millis(250))
        }
    );

    // 2. The retry enumerates cleanly, one snapshot across its pages.
    let pages = enumerate(&source, &root_id(), 1).expect("the retry succeeds");
    assert_eq!(pages.len(), 1, "one chat at revision 0");
    assert_eq!(pages[0].snapshot.as_str(), "m1");

    // 3. A clean fetch of the pinned version.
    let mut sink = RecordingSink::new(full_range());
    exec::drive(source.fetch(fetch_request(full_range()), &mut sink)).expect("delivery completes");
    assert_eq!(sink.bytes(), CONTENT);

    // 4. The second fetch races a version change eight bytes in.
    let mut raced = RecordingSink::new(full_range());
    let error = exec::drive(source.fetch(fetch_request(full_range()), &mut raced))
        .expect_err("scripted version race");
    assert!(
        matches!(error, SourceError::VersionConflict { .. }),
        "got {error:?}"
    );
    assert_eq!(raced.bytes(), &CONTENT[..8]);
    assert!(!raced.is_complete());

    // 5. The feed reports both batches, in order, against a durable cursor.
    let cursor = exec::drive(source.latest_cursor()).expect("cursor resolves");
    source.advance_all();
    let first = changes(&source, cursor.clone()).expect("first batch");
    assert!(first.more_available);
    let second = changes(&source, first.next.clone()).expect("second batch");
    assert!(!second.more_available);

    // 6. After the change, the old pin conflicts and the new one works.
    let mut stale = RecordingSink::new(ByteRange::new(0, 4).expect("valid range"));
    let error = exec::drive(source.fetch(
        fetch_request(ByteRange::new(0, 4).expect("valid range")),
        &mut stale,
    ))
    .expect_err("c1 is gone");
    assert!(
        matches!(error, SourceError::VersionConflict { .. }),
        "got {error:?}"
    );

    let fresh_range = ByteRange::new(0, 24).expect("valid range");
    let mut fresh = RecordingSink::new(fresh_range);
    exec::drive(source.fetch(
        FetchRequest {
            item: photo_id(),
            version: version("c2"),
            range: fresh_range,
        },
        &mut fresh,
    ))
    .expect("the current version serves");
    assert_eq!(fresh.bytes(), b"the replacement content!");

    // 7. A delayed enumeration, dropped while its delay still holds it: the
    //    scripted delay is what makes the cancellation point exist at all.
    let mut delayed = Box::pin(source.children(chat_id(100), page_request(10)));
    assert_eq!(
        exec::poll_n(delayed.as_mut(), 2),
        Poll::Pending,
        "two scripted yields hold the call open across two polls"
    );
    drop(delayed);

    // 8. Every call above is on the record, in order, with the arguments it
    //    carried — the whole log, not a sample of it.
    assert_eq!(
        source.calls(),
        vec![
            Call::Children {
                parent: root_id(),
                request: page_request(10),
            },
            Call::Children {
                parent: root_id(),
                request: page_request(1),
            },
            Call::Fetch {
                request: fetch_request(full_range()),
            },
            Call::Fetch {
                request: fetch_request(full_range()),
            },
            Call::LatestCursor,
            Call::Changes { cursor },
            Call::Changes { cursor: first.next },
            Call::Fetch {
                request: fetch_request(ByteRange::new(0, 4).expect("valid range")),
            },
            Call::Fetch {
                request: FetchRequest {
                    item: photo_id(),
                    version: version("c2"),
                    range: fresh_range,
                },
            },
            Call::Children {
                parent: chat_id(100),
                request: page_request(10),
            },
        ]
    );

    // 9. And the outcome each one actually had — every entry, including the
    //    bytes that escaped before the two interrupted deliveries.
    let outcomes: Vec<Outcome> = source
        .interactions()
        .into_iter()
        .map(|entry| entry.outcome)
        .collect();

    assert!(
        matches!(
            &outcomes[0],
            Outcome::Failed {
                error: SourceError::RateLimited { .. },
                delivered: 0,
            }
        ),
        "the flood wait is recorded: {:?}",
        outcomes[0]
    );
    assert!(outcomes[1].is_ok(), "the retry succeeded");
    assert!(outcomes[2].is_ok(), "the clean fetch");
    assert!(
        matches!(
            &outcomes[3],
            Outcome::Failed {
                error: SourceError::VersionConflict { .. },
                delivered: 8,
            }
        ),
        "the race is recorded with the bytes it got out first: {:?}",
        outcomes[3]
    );
    assert!(outcomes[4].is_ok(), "the cursor");
    assert!(outcomes[5].is_ok(), "the first change batch");
    assert!(outcomes[6].is_ok(), "the second change batch");
    assert!(
        matches!(
            &outcomes[7],
            Outcome::Failed {
                error: SourceError::VersionConflict { .. },
                delivered: 0,
            }
        ),
        "the stale pin conflicts before delivering a byte: {:?}",
        outcomes[7]
    );
    assert!(outcomes[8].is_ok(), "the refreshed fetch");
    assert_eq!(
        outcomes[9],
        Outcome::Cancelled { delivered: 0 },
        "the dropped enumeration is cancellation, and it moved nothing"
    );
}

#[test]
fn chat_list_appearances_are_scriptable_as_distinct_items() {
    // DOM-022: the same chat under two views is two items. The fake has to
    // be able to say that, or the tree tests downstream cannot.
    let scope = fixture::scope();
    let main = fixture::chat_list_id(scope, ChatListKind::Main);
    let archive = fixture::chat_list_id(scope, ChatListKind::Archive);
    let in_main = fixture::chat_appearance_id(scope, 100, ChatListKind::Main);
    let in_archive = fixture::chat_appearance_id(scope, 100, ChatListKind::Archive);

    let source = fake(
        SourceScript::builder(scope).items([
            root_item(),
            fixture::directory(
                main.clone(),
                Some(root_id()),
                "Main",
                "m2",
                DirectoryKind::ChatList,
            )
            .expect("valid fixture"),
            fixture::directory(
                archive.clone(),
                Some(root_id()),
                "Archive",
                "m3",
                DirectoryKind::ChatList,
            )
            .expect("valid fixture"),
            fixture::directory(
                in_main,
                Some(main.clone()),
                "Team",
                "m4",
                DirectoryKind::Chat,
            )
            .expect("valid fixture"),
            fixture::directory(
                in_archive,
                Some(archive.clone()),
                "Team",
                "m5",
                DirectoryKind::Chat,
            )
            .expect("valid fixture"),
        ]),
    );

    let main_page = exec::drive(source.children(main, page_request(10))).expect("main lists");
    let archive_page =
        exec::drive(source.children(archive, page_request(10))).expect("archive lists");

    assert_eq!(main_page.items.len(), 1);
    assert_eq!(archive_page.items.len(), 1);
    assert_eq!(
        main_page.items[0].display_name,
        archive_page.items[0].display_name
    );
    assert_ne!(
        main_page.items[0].id, archive_page.items[0].id,
        "one chat, two appearances, two identities"
    );
}
