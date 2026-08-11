//! The ranged fetch coordinator (TASK-260715-22fh09), driven end-to-end
//! against the deterministic fake source: exact range bytes, reader
//! coalescing, aligned chunking, bounded parallelism, the retry taxonomy
//! with source backoff hints, in-attempt locator refresh, version races,
//! prompt cancellation, and crash-resume over durable transfer state
//! (SYNC-041..046).

// clippy.toml exempts test code on the grounds that a panicking test is
// just a failing test. That exemption keys on `#[test]` functions, and the
// shared fixture helpers below sit at module level in an integration-test
// binary. The rationale still applies in full — this file links into no
// product artifact — so the exemption is restated here, matching the
// established test-suite pattern.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::HashMap;
use std::future::Future;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use gramdrive_engine::fetch::{
    AttemptEnd, Clock, FetchConfig, FetchCoordinator, ReaderEnd, ReaderReport, RunOutcome,
    RunReport, Staging, StagingError, StagingHost,
};
use gramdrive_engine::model::ByteRange;
use gramdrive_engine::model::identity::{AccountScope, ChatId, ChatKey, ItemId};
use gramdrive_engine::model::version::{ContentVersion, MetadataVersion};
use gramdrive_engine::source::{
    ContentChunk, ContentSink, DeliveryViolation, DriveSource, SinkControl, SourceError,
    SourceFuture,
};
use gramdrive_engine::state::repo::{
    AccountRecord, ChatRecord, ChatType, FailureCategory, FileFacts, ItemAvailability, ItemRecord,
    RetentionMode, SourceKind, TransferId, TransferRecord, TransferState,
};
use gramdrive_engine::state::{LocalStorage, StateStore, StorageError, StoredObject};
use gramdrive_engine::transfer::{Priority, RequestOutcome, RetryPolicy, TransferMachine};
use gramdrive_testkit::source::{DirectoryKind, FileKind};
use gramdrive_testkit::{
    Call, ChunkPlan, FakeSource, Fault, Occurrence, Operation, RecordingSink, ScriptBuilder,
    SourceScript, exec, fixture,
};

const CHAT: i64 = 100;

// ---------------------------------------------------------------------------
// Identities and content shared by the store and the script
// ---------------------------------------------------------------------------

fn scope() -> AccountScope {
    fixture::scope()
}

fn root_id() -> ItemId {
    fixture::account_root_id(scope())
}

fn chat_dir_id() -> ItemId {
    fixture::chat_id(scope(), CHAT)
}

fn photo_id() -> ItemId {
    fixture::attachment_id(scope(), CHAT, 5, 0)
}

fn doc_id() -> ItemId {
    fixture::attachment_id(scope(), CHAT, 6, 0)
}

fn content_v1() -> Vec<u8> {
    (0..64u8).collect()
}

fn content_v2() -> Vec<u8> {
    (0..64u8).map(|byte| byte | 0x80).collect()
}

fn content_doc() -> Vec<u8> {
    (64..128u8).collect()
}

fn version(text: &str) -> ContentVersion {
    ContentVersion::new(text).expect("valid version")
}

fn metadata(text: &str) -> MetadataVersion {
    MetadataVersion::new(text).expect("valid version")
}

fn range(start: u64, end: u64) -> ByteRange {
    ByteRange::new(start, end).expect("valid range")
}

// ---------------------------------------------------------------------------
// Script and store fixtures
// ---------------------------------------------------------------------------

/// The shared backend script: root/chat directories plus two fetchable
/// 64-byte files at version v1, delivering whole-range chunks so every
/// sub-fetch is one deterministic accept.
fn script_builder(faults: Vec<Fault>) -> ScriptBuilder {
    let mut builder = SourceScript::builder(scope())
        .item(
            fixture::directory(root_id(), None, "Account", "m1", DirectoryKind::Root)
                .expect("root fixture"),
        )
        .item(
            fixture::directory(
                chat_dir_id(),
                Some(root_id()),
                "Team",
                "m2",
                DirectoryKind::Chat,
            )
            .expect("chat fixture"),
        )
        .item(
            fixture::file(
                photo_id(),
                chat_dir_id(),
                "photo.jpg",
                "m3",
                "v1",
                64,
                FileKind::Attachment,
            )
            .expect("photo fixture"),
        )
        .item(
            fixture::file(
                doc_id(),
                chat_dir_id(),
                "doc.bin",
                "m4",
                "v1",
                64,
                FileKind::Attachment,
            )
            .expect("doc fixture"),
        )
        .content(&photo_id(), version("v1"), content_v1())
        .content(&doc_id(), version("v1"), content_doc())
        .chunks(ChunkPlan::Whole);
    for fault in faults {
        builder = builder.fault(fault);
    }
    builder
}

fn script(faults: Vec<Fault>) -> SourceScript {
    script_builder(faults).build().expect("valid script")
}

/// Seeds the account/chat scaffold plus the two file items, with `photo`'s
/// logical size as given (the projection may lag the source, SYNC-042).
fn seed_store(store: &mut StateStore, photo_size: Option<u64>) {
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Test Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        display_timezone: "UTC".to_owned(),
        retention_mode: RetentionMode::Mirror,
        archive_mode: false,
        secret_ref: None,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
    })
    .expect("account");
    tx.upsert_chat(&ChatRecord {
        key: ChatKey {
            scope: scope(),
            chat_id: ChatId(CHAT),
        },
        chat_type: ChatType::Private,
        title: format!("Chat {CHAT}"),
        username: None,
        is_protected: false,
        archive_mode: false,
        metadata_version: metadata("m1"),
        left_at_ms: None,
        deleted_at_ms: None,
        last_update_at_ms: None,
    })
    .expect("chat");
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: root_id(),
        parent: None,
        display_name: "Root".to_owned(),
        safe_name: "Root".to_owned(),
        metadata_version: metadata("m1"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("root");
    tx.upsert_item(&file_record(photo_id(), "photo.jpg", photo_size, "v1"))
        .expect("photo");
    tx.upsert_item(&file_record(doc_id(), "doc.bin", Some(64), "v1"))
        .expect("doc");
    tx.commit().expect("commit");
}

fn file_record(id: ItemId, name: &str, size: Option<u64>, content_version: &str) -> ItemRecord {
    ItemRecord {
        aggregate_size: None,
        id,
        parent: Some(root_id()),
        display_name: name.to_owned(),
        safe_name: name.to_owned(),
        metadata_version: metadata("m1"),
        content: Some(FileFacts {
            mime_type: None,
            logical_size: size,
            content_version: Some(version(content_version)),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    }
}

/// Moves the store's photo projection to `v2` at size 64, simulating the
/// change pipeline observing a source-side republish.
fn republish_photo(store: &mut StateStore, now_ms: i64) {
    let tx = store.write_txn().expect("write");
    tx.update_item_content(
        &photo_id(),
        Some(&version("v1")),
        &FileFacts {
            mime_type: None,
            logical_size: Some(64),
            content_version: Some(version("v2")),
        },
        &metadata("m2"),
        now_ms,
    )
    .expect("republish");
    tx.commit().expect("commit");
}

fn machine() -> TransferMachine {
    TransferMachine::new(RetryPolicy {
        retry_budget: 2,
        base_backoff_ms: 1_000,
        max_backoff_ms: 4_000,
    })
}

fn config(chunk_bytes: u64, fanout: usize, stale_refresh_limit: u32) -> FetchConfig {
    FetchConfig {
        chunk_bytes: NonZeroU64::new(chunk_bytes).expect("non-zero"),
        fanout: NonZeroUsize::new(fanout).expect("non-zero"),
        stale_refresh_limit,
    }
}

// ---------------------------------------------------------------------------
// Host-side test doubles: clock, staging, reader sink
// ---------------------------------------------------------------------------

/// A hand-advanced clock (SYNC-073: time is always the caller's).
#[derive(Debug)]
struct TestClock(AtomicI64);

impl TestClock {
    fn new(now_ms: i64) -> Self {
        Self(AtomicI64::new(now_ms))
    }

    fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }

    fn advance(&self, delta_ms: i64) {
        self.0.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.now()
    }
}

/// In-memory staging keyed by handle; objects survive close/reopen, which
/// is what makes resume and crash tests meaningful. Doubles as the
/// [`LocalStorage`] inventory for reconciliation.
#[derive(Debug, Default)]
struct MemoryStagingHost {
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    fail_writes: Arc<AtomicBool>,
}

impl MemoryStagingHost {
    fn set_fail_writes(&self, fail: bool) {
        self.fail_writes.store(fail, Ordering::SeqCst);
    }

    fn remove(&self, handle: &str) {
        self.objects.lock().expect("lock").remove(handle);
    }

    fn handles(&self) -> Vec<String> {
        let mut handles: Vec<String> = self.objects.lock().expect("lock").keys().cloned().collect();
        handles.sort();
        handles
    }
}

impl StagingHost for MemoryStagingHost {
    fn open(
        &mut self,
        transfer: TransferId,
        existing: Option<&str>,
    ) -> Result<Box<dyn Staging>, StagingError> {
        let handle = existing
            .map(str::to_owned)
            .unwrap_or_else(|| format!("stage-{}", transfer.0));
        self.objects
            .lock()
            .expect("lock")
            .entry(handle.clone())
            .or_default();
        Ok(Box::new(MemoryStaging {
            handle,
            objects: Arc::clone(&self.objects),
            fail_writes: Arc::clone(&self.fail_writes),
        }))
    }
}

#[derive(Debug)]
struct MemoryStaging {
    handle: String,
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    fail_writes: Arc<AtomicBool>,
}

impl Staging for MemoryStaging {
    fn handle(&self) -> &str {
        &self.handle
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), StagingError> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(StagingError::Full {
                detail: "scripted disk-full".to_owned(),
            });
        }
        let mut objects = self.objects.lock().expect("lock");
        let object = objects.get_mut(&self.handle).ok_or(StagingError::Failed {
            detail: "staging object vanished".to_owned(),
        })?;
        let offset = usize::try_from(offset).expect("test offsets fit usize");
        let end = offset + bytes.len();
        if object.len() < end {
            object.resize(end, 0);
        }
        object[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StagingError> {
        let objects = self.objects.lock().expect("lock");
        let object = objects.get(&self.handle).ok_or(StagingError::Failed {
            detail: "staging object vanished".to_owned(),
        })?;
        let offset = usize::try_from(offset).expect("test offsets fit usize");
        let end = offset + buf.len();
        let slice = object.get(offset..end).ok_or(StagingError::Failed {
            detail: "read past written bytes".to_owned(),
        })?;
        buf.copy_from_slice(slice);
        Ok(())
    }
}

impl LocalStorage for MemoryStagingHost {
    fn cache_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        Ok(Vec::new())
    }

    fn staging_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        Ok(self
            .objects
            .lock()
            .expect("lock")
            .iter()
            .map(|(reference, bytes)| StoredObject {
                reference: reference.clone(),
                size: bytes.len() as u64,
            })
            .collect())
    }

    fn remove_cache_object(&self, _reference: &str) -> Result<(), StorageError> {
        Ok(())
    }

    fn remove_staging_object(&self, reference: &str) -> Result<(), StorageError> {
        self.objects.lock().expect("lock").remove(reference);
        Ok(())
    }
}

/// A cloneable handle over the testkit's [`RecordingSink`], so the test
/// keeps inspection access after the coordinator takes the sink.
#[derive(Debug, Clone)]
struct SharedSink(Arc<Mutex<RecordingSink>>);

impl SharedSink {
    fn new(wanted: ByteRange) -> Self {
        Self(Arc::new(Mutex::new(RecordingSink::new(wanted))))
    }

    fn stopping_after(wanted: ByteRange, chunks: usize) -> Self {
        Self(Arc::new(Mutex::new(RecordingSink::stopping_after(
            wanted, chunks,
        ))))
    }

    fn bytes(&self) -> Vec<u8> {
        self.0.lock().expect("lock").bytes().to_vec()
    }

    fn is_complete(&self) -> bool {
        self.0.lock().expect("lock").is_complete()
    }

    fn violation(&self) -> Option<DeliveryViolation> {
        self.0.lock().expect("lock").violation()
    }
}

impl ContentSink for SharedSink {
    fn accept(&mut self, chunk: ContentChunk<'_>) -> SinkControl {
        self.0.lock().expect("lock").accept(chunk)
    }
}

// ---------------------------------------------------------------------------
// Rig and driving helpers
// ---------------------------------------------------------------------------

struct Rig {
    store: StateStore,
    source: FakeSource,
    coordinator: FetchCoordinator,
    staging: MemoryStagingHost,
    clock: TestClock,
}

fn rig_with(source_script: SourceScript, fetch_config: FetchConfig) -> Rig {
    rig_with_size(source_script, fetch_config, Some(64))
}

fn rig_with_size(
    source_script: SourceScript,
    fetch_config: FetchConfig,
    photo_size: Option<u64>,
) -> Rig {
    let mut store = StateStore::open_in_memory().expect("open");
    seed_store(&mut store, photo_size);
    Rig {
        store,
        source: FakeSource::new(source_script),
        coordinator: FetchCoordinator::new(machine(), fetch_config),
        staging: MemoryStagingHost::default(),
        clock: TestClock::new(1_000),
    }
}

/// The FFI hosts drive coordinator runs from an async runtime; requiring
/// `Send` here keeps that possible for every test-driven future.
fn require_send<F: Future + Send>(future: F) -> F {
    future
}

fn run(rig: &mut Rig) -> RunOutcome {
    exec::drive(require_send(rig.coordinator.run_next(
        &mut rig.store,
        &rig.source,
        &mut rig.staging,
        &rig.clock,
    )))
    .expect("run_next")
}

fn run_report(rig: &mut Rig) -> RunReport {
    match run(rig) {
        RunOutcome::Ran(report) => report,
        RunOutcome::Idle => panic!("expected a claimable transfer, queue was idle"),
    }
}

fn open_reader(
    rig: &mut Rig,
    item: &ItemId,
    wanted: ByteRange,
    priority: Priority,
    sink: &SharedSink,
) -> gramdrive_engine::fetch::OpenOutcome {
    let now = rig.clock.now();
    rig.coordinator
        .open(
            &mut rig.store,
            item,
            wanted,
            priority,
            Box::new(sink.clone()),
            now,
        )
        .expect("open")
}

fn fetch_ranges(source: &FakeSource) -> Vec<ByteRange> {
    source
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Fetch { request } => Some(request.range),
            _ => None,
        })
        .collect()
}

fn fetch_items(source: &FakeSource) -> Vec<ItemId> {
    source
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            Call::Fetch { request } => Some(request.item),
            _ => None,
        })
        .collect()
}

fn transfer_row(store: &mut StateStore, id: TransferId) -> TransferRecord {
    store
        .read_txn()
        .expect("read")
        .transfer(id)
        .expect("transfer")
        .expect("row exists")
}

/// A unique database path under the OS temp directory, cleaned on drop —
/// the state suite's TempDb pattern (no clock, no randomness).
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gramdrive-fetch-test-{}-{n}.sqlite3",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut name = self.path.as_os_str().to_owned();
            name.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(name));
        }
    }
}

// ---------------------------------------------------------------------------
// Range bytes are correct; chunks align (SYNC-041)
// ---------------------------------------------------------------------------

#[test]
fn single_reader_streams_exact_bytes_and_aligns_chunks() {
    let mut rig = rig_with(script(vec![]), config(16, 1, 1));
    let sink = SharedSink::new(range(5, 23));
    let opened = open_reader(
        &mut rig,
        &photo_id(),
        range(5, 23),
        Priority::FOREGROUND,
        &sink,
    );
    assert!(!opened.coalesced);
    assert!(opened.covers);
    assert_eq!(opened.displaced, None);

    let report = run_report(&mut rig);
    assert_eq!(report.transfer, opened.transfer);
    assert!(
        matches!(&report.end, AttemptEnd::Promoted { staging: Some(_) }),
        "expected promotion with a staging handle, got {:?}",
        report.end
    );
    assert_eq!(
        report.readers,
        vec![ReaderReport {
            reader: opened.reader,
            end: ReaderEnd::Satisfied,
        }]
    );
    assert!(report.disposals.is_empty());

    // AC: range bytes are correct — exactly [5, 23) of the content, in
    // order, through a contract-verifying sink.
    assert_eq!(sink.bytes(), content_v1()[5..23].to_vec());
    assert!(sink.is_complete());
    assert_eq!(sink.violation(), None);

    // SYNC-041: the network saw aligned chunks, each exactly once, and the
    // staged superset is durable.
    assert_eq!(fetch_ranges(&rig.source), vec![range(0, 16), range(16, 32)]);
    let row = transfer_row(&mut rig.store, opened.transfer);
    assert_eq!(row.state, TransferState::Done);
    assert_eq!(row.completed_ranges, vec![range(0, 32)]);
}

#[test]
fn zero_size_object_promotes_without_touching_the_source() {
    let mut rig = rig_with_size(script(vec![]), config(16, 1, 1), Some(0));
    let now = rig.clock.now();
    let outcome = rig
        .coordinator
        .hydrate(&mut rig.store, &photo_id(), &[], Priority::BACKGROUND, now)
        .expect("hydrate");
    let RequestOutcome::Created { transfer, .. } = outcome else {
        panic!("expected Created, got {outcome:?}");
    };

    let report = run_report(&mut rig);
    assert_eq!(report.transfer, transfer);
    assert!(matches!(report.end, AttemptEnd::Promoted { staging: None }));
    assert_eq!(fetch_ranges(&rig.source), vec![]);
    assert_eq!(
        transfer_row(&mut rig.store, transfer).state,
        TransferState::Done
    );
}

// ---------------------------------------------------------------------------
// Coalescing and bounded duplicate work (SYNC-046)
// ---------------------------------------------------------------------------

#[test]
fn concurrent_readers_coalesce_onto_one_transfer() {
    let mut rig = rig_with(script(vec![]), config(16, 2, 1));
    let sink1 = SharedSink::new(range(0, 16));
    let sink2 = SharedSink::new(range(8, 32));
    let first_open = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 16),
        Priority::FOREGROUND,
        &sink1,
    );
    let second_open = open_reader(
        &mut rig,
        &photo_id(),
        range(8, 32),
        Priority::FOREGROUND,
        &sink2,
    );

    // SYNC-046: the second open attaches to the live transfer rather than
    // spawning parallel network work for overlapping bytes.
    assert_eq!(second_open.transfer, first_open.transfer);
    assert!(second_open.coalesced);
    assert!(!second_open.covers, "the tail is not on the live plan");

    let first = run_report(&mut rig);
    assert!(matches!(first.end, AttemptEnd::Promoted { .. }));
    assert!(first.readers.contains(&ReaderReport {
        reader: first_open.reader,
        end: ReaderEnd::Satisfied,
    }));
    let tail = first
        .readers
        .iter()
        .find_map(|report| match (&report.reader, &report.end) {
            (reader, ReaderEnd::Reattached { transfer }) if *reader == second_open.reader => {
                Some(*transfer)
            }
            _ => None,
        })
        .expect("the uncovered reader moved to fresh demand");

    let second = run_report(&mut rig);
    assert_eq!(second.transfer, tail);
    assert!(matches!(second.end, AttemptEnd::Promoted { .. }));
    assert!(second.readers.contains(&ReaderReport {
        reader: second_open.reader,
        end: ReaderEnd::Satisfied,
    }));

    assert_eq!(sink1.bytes(), content_v1()[0..16].to_vec());
    assert_eq!(sink2.bytes(), content_v1()[8..32].to_vec());
    // AC: duplicate compatible network work is bounded — the shared bytes
    // crossed the wire exactly once.
    assert_eq!(fetch_ranges(&rig.source), vec![range(0, 16), range(16, 32)]);
}

#[test]
fn parallelism_is_bounded_within_one_item() {
    let source_script = script(vec![Fault::on(Operation::Fetch).delay(6)]);
    let mut rig = rig_with(source_script, config(16, 2, 1));
    let sink = SharedSink::new(range(0, 64));
    let opened = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 64),
        Priority::FOREGROUND,
        &sink,
    );

    let Rig {
        ref mut store,
        ref source,
        ref mut coordinator,
        ref mut staging,
        ref clock,
    } = rig;
    let mut future = Box::pin(coordinator.run_next(store, source, staging, clock));

    assert!(exec::poll_n(future.as_mut(), 1).is_pending());
    assert_eq!(
        fetch_ranges(source).len(),
        2,
        "only `fanout` sub-fetches are in flight at once"
    );

    let outcome = match exec::poll_n(future.as_mut(), 1_000_000) {
        Poll::Ready(result) => result.expect("run_next"),
        Poll::Pending => panic!("run did not finish"),
    };
    drop(future);
    let RunOutcome::Ran(report) = outcome else {
        panic!("expected a run");
    };
    assert_eq!(report.transfer, opened.transfer);
    assert!(matches!(report.end, AttemptEnd::Promoted { .. }));
    assert_eq!(
        fetch_ranges(&rig.source).len(),
        4,
        "all four chunks fetched"
    );
    assert!(sink.is_complete());
    assert_eq!(sink.bytes(), content_v1());
}

#[test]
fn priority_orders_claims_across_items() {
    let mut rig = rig_with(script(vec![]), config(16, 2, 1));
    let sink_background = SharedSink::new(range(0, 16));
    let sink_foreground = SharedSink::new(range(0, 16));
    let background = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 16),
        Priority::BACKGROUND,
        &sink_background,
    );
    let foreground = open_reader(
        &mut rig,
        &doc_id(),
        range(0, 16),
        Priority::FOREGROUND,
        &sink_foreground,
    );

    let first = run_report(&mut rig);
    assert_eq!(
        first.transfer, foreground.transfer,
        "the user-facing read is served first"
    );
    let second = run_report(&mut rig);
    assert_eq!(second.transfer, background.transfer);
    assert_eq!(sink_foreground.bytes(), content_doc()[0..16].to_vec());
    assert_eq!(sink_background.bytes(), content_v1()[0..16].to_vec());
}

// ---------------------------------------------------------------------------
// Retry taxonomy, backoff hints, locator refresh (SYNC-044, SYNC-045)
// ---------------------------------------------------------------------------

#[test]
fn retry_requeues_with_backoff_and_honors_flood_wait() {
    let source_script = script(vec![
        Fault::on(Operation::Fetch)
            .occurrence(Occurrence::Nth(2))
            .fail(SourceError::RateLimited {
                retry_after: Some(Duration::from_millis(30_000)),
                detail: "flood wait".to_owned(),
            }),
    ]);
    let mut rig = rig_with(source_script, config(16, 1, 1));
    let sink = SharedSink::new(range(0, 32));
    let opened = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 32),
        Priority::FOREGROUND,
        &sink,
    );

    let report = run_report(&mut rig);
    match report.end {
        AttemptEnd::Requeued {
            category: FailureCategory::RateLimited,
            next_retry_at_ms,
            retries_used: 1,
            progress_wiped: false,
        } => {
            // NFR-033/SEC-031: the source's mandated wait outranks the
            // policy's own 1s schedule.
            assert_eq!(next_retry_at_ms, rig.clock.now() + 30_000);
        }
        other => panic!("expected a rate-limited requeue, got {other:?}"),
    }
    assert!(
        report.readers.is_empty(),
        "readers stay subscribed across a retry"
    );

    // Not due yet: the queue hides the row until the backoff passes.
    assert!(matches!(run(&mut rig), RunOutcome::Idle));

    rig.clock.advance(30_000);
    let report = run_report(&mut rig);
    assert!(matches!(report.end, AttemptEnd::Promoted { .. }));
    assert!(report.readers.contains(&ReaderReport {
        reader: opened.reader,
        end: ReaderEnd::Satisfied,
    }));
    assert_eq!(sink.bytes(), content_v1()[0..32].to_vec());
    // The staged first chunk was not re-fetched on retry.
    assert_eq!(
        fetch_ranges(&rig.source),
        vec![range(0, 16), range(16, 32), range(16, 32)]
    );
}

#[test]
fn stale_reference_refreshes_within_the_attempt() {
    let source_script = script(vec![
        Fault::on(Operation::Fetch)
            .occurrence(Occurrence::Nth(2))
            .fail(SourceError::StaleReference {
                detail: "file reference expired".to_owned(),
            }),
    ]);
    let mut rig = rig_with(source_script, config(16, 1, 1));
    let sink = SharedSink::new(range(0, 32));
    let opened = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 32),
        Priority::FOREGROUND,
        &sink,
    );

    // One run: the refresh happens inside the attempt, not through the
    // retry queue.
    let report = run_report(&mut rig);
    assert!(matches!(report.end, AttemptEnd::Promoted { .. }));
    assert!(report.readers.contains(&ReaderReport {
        reader: opened.reader,
        end: ReaderEnd::Satisfied,
    }));
    assert_eq!(sink.bytes(), content_v1()[0..32].to_vec());
    assert_eq!(
        fetch_ranges(&rig.source),
        vec![range(0, 16), range(16, 32), range(16, 32)],
        "the stale chunk was re-asked once, nothing else twice"
    );
    // SYNC-045: the refresh never changes the item identity.
    assert!(
        fetch_items(&rig.source)
            .iter()
            .all(|item| *item == photo_id())
    );
}

#[test]
fn persistent_stale_reference_exhausts_refresh_then_retry_budget() {
    let source_script = script(vec![Fault::on(Operation::Fetch).fail(
        SourceError::StaleReference {
            detail: "永 stale".to_owned(),
        },
    )]);
    let mut rig = rig_with(source_script, config(16, 1, 1));
    let sink = SharedSink::new(range(0, 16));
    let opened = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 16),
        Priority::FOREGROUND,
        &sink,
    );

    // Attempt 1: the in-attempt refresh budget (1) is spent, then the
    // failure classifies through the machine.
    let report = run_report(&mut rig);
    assert!(matches!(
        report.end,
        AttemptEnd::Requeued {
            category: FailureCategory::StaleReference,
            retries_used: 1,
            ..
        }
    ));
    assert_eq!(
        fetch_ranges(&rig.source).len(),
        2,
        "one ask plus one refresh"
    );

    rig.clock.advance(1_000);
    let report = run_report(&mut rig);
    assert!(matches!(
        report.end,
        AttemptEnd::Requeued {
            retries_used: 2,
            ..
        }
    ));

    rig.clock.advance(2_000);
    let report = run_report(&mut rig);
    assert!(
        matches!(
            report.end,
            AttemptEnd::Failed {
                category: FailureCategory::StaleReference,
            }
        ),
        "the retry budget (2) makes the next failure terminal, got {:?}",
        report.end
    );
    assert_eq!(
        report.readers,
        vec![ReaderReport {
            reader: opened.reader,
            end: ReaderEnd::Failed {
                category: FailureCategory::StaleReference,
            },
        }]
    );
    assert_eq!(
        transfer_row(&mut rig.store, opened.transfer).state,
        TransferState::Failed
    );
}

#[test]
fn disk_full_parks_and_resumes_after_space_frees() {
    let mut rig = rig_with(script(vec![]), config(16, 1, 1));
    rig.staging.set_fail_writes(true);
    let sink = SharedSink::new(range(0, 16));
    let opened = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 16),
        Priority::FOREGROUND,
        &sink,
    );

    let report = run_report(&mut rig);
    assert!(matches!(
        report.end,
        AttemptEnd::Parked {
            category: FailureCategory::DiskFull,
        }
    ));
    assert!(report.readers.is_empty(), "parked keeps the subscription");
    assert_eq!(
        transfer_row(&mut rig.store, opened.transfer).state,
        TransferState::Suspended
    );

    // Space frees; the host resumes everything suspended.
    rig.staging.set_fail_writes(false);
    let now = rig.clock.now();
    rig.coordinator
        .resume(&mut rig.store, opened.transfer, now)
        .expect("resume");
    let report = run_report(&mut rig);
    assert!(matches!(report.end, AttemptEnd::Promoted { .. }));
    assert_eq!(sink.bytes(), content_v1()[0..16].to_vec());
}

// ---------------------------------------------------------------------------
// Version races: stale bytes never publish (SYNC-042)
// ---------------------------------------------------------------------------

#[test]
fn version_race_invalidates_and_never_publishes_stale_bytes() {
    let source_script = script_builder(vec![
        Fault::on(Operation::Fetch)
            .occurrence(Occurrence::Nth(1))
            .version_race(10, Some(version("v2"))),
    ])
    .batch([gramdrive_testkit::source::ItemChange::Upserted(
        fixture::file(
            photo_id(),
            chat_dir_id(),
            "photo.jpg",
            "m5",
            "v2",
            64,
            FileKind::Attachment,
        )
        .expect("photo v2"),
    )])
    .content(&photo_id(), version("v2"), content_v2())
    .build()
    .expect("valid script");
    let mut rig = rig_with(source_script, config(16, 1, 1));
    let sink = SharedSink::new(range(0, 16));
    let opened = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 16),
        Priority::FOREGROUND,
        &sink,
    );

    let report = run_report(&mut rig);
    assert!(matches!(
        report.end,
        AttemptEnd::Invalidated {
            category: FailureCategory::VersionConflict,
        }
    ));
    assert_eq!(
        report.readers,
        vec![ReaderReport {
            reader: opened.reader,
            end: ReaderEnd::Failed {
                category: FailureCategory::VersionConflict,
            },
        }]
    );
    // The partial prefix that streamed before the conflict was genuinely
    // v1's bytes; what matters is that it can never *publish*.
    assert_eq!(sink.bytes(), content_v1()[0..10].to_vec());

    // AC: stale version cannot publish — the row is terminal, staged
    // progress is wiped, and the staging area came back as a disposal.
    let row = transfer_row(&mut rig.store, opened.transfer);
    assert_eq!(row.state, TransferState::Failed);
    assert_eq!(row.failure_category, Some(FailureCategory::VersionConflict));
    assert_eq!(row.completed_ranges, vec![]);
    assert_eq!(row.temp_ref, None);
    assert_eq!(report.disposals.len(), 1);
    for disposal in &report.disposals {
        rig.staging.remove(&disposal.staging);
    }
    assert!(rig.staging.handles().is_empty());

    // The projection observes the republish; fresh demand pins v2 and
    // fetches clean bytes.
    let now = rig.clock.now();
    republish_photo(&mut rig.store, now);
    rig.source.advance();
    let sink2 = SharedSink::new(range(0, 16));
    let reopened = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 16),
        Priority::FOREGROUND,
        &sink2,
    );
    assert_ne!(reopened.transfer, opened.transfer);
    let report = run_report(&mut rig);
    assert!(matches!(report.end, AttemptEnd::Promoted { .. }));
    assert_eq!(sink2.bytes(), content_v2()[0..16].to_vec());
    assert_ne!(sink2.bytes(), content_v1()[0..16].to_vec());
}

// ---------------------------------------------------------------------------
// Cancellation is prompt (SYNC-043, SYNC-005)
// ---------------------------------------------------------------------------

#[test]
fn queued_cancel_prevents_any_network_and_new_demand_displaces() {
    let mut rig = rig_with(script(vec![]), config(16, 1, 1));
    let now = rig.clock.now();
    let outcome = rig
        .coordinator
        .hydrate(
            &mut rig.store,
            &photo_id(),
            &[range(0, 16)],
            Priority::BACKGROUND,
            now,
        )
        .expect("hydrate");
    let RequestOutcome::Created {
        transfer: cancelled,
        ..
    } = outcome
    else {
        panic!("expected Created, got {outcome:?}");
    };
    assert!(
        rig.coordinator
            .request_cancel(&mut rig.store, cancelled, now)
            .expect("cancel")
    );

    // A cancel-requested row is invisible to claims: no network ever runs.
    assert!(matches!(run(&mut rig), RunOutcome::Idle));
    assert_eq!(fetch_ranges(&rig.source), vec![]);

    // Fresh demand for the same item acknowledges the abandoned cancel and
    // starts a new transfer.
    let sink = SharedSink::new(range(0, 16));
    let reopened = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 16),
        Priority::FOREGROUND,
        &sink,
    );
    assert_ne!(reopened.transfer, cancelled);
    assert_eq!(reopened.displaced, None, "nothing was staged to displace");
    assert_eq!(
        transfer_row(&mut rig.store, cancelled).state,
        TransferState::Cancelled
    );
    let report = run_report(&mut rig);
    assert!(matches!(report.end, AttemptEnd::Promoted { .. }));
    assert_eq!(sink.bytes(), content_v1()[0..16].to_vec());
}

#[test]
fn durable_cancel_mid_transfer_stops_promptly_and_disposes_staging() {
    let db = TempDb::new();
    let mut store = StateStore::open(&db.path).expect("open");
    seed_store(&mut store, Some(64));
    let source = FakeSource::new(script(vec![Fault::on(Operation::Fetch).delay(4)]));
    let coordinator = FetchCoordinator::new(machine(), config(16, 1, 1));
    let mut staging = MemoryStagingHost::default();
    let clock = TestClock::new(1_000);

    let sink = SharedSink::new(range(0, 64));
    let opened = coordinator
        .open(
            &mut store,
            &photo_id(),
            range(0, 64),
            Priority::FOREGROUND,
            Box::new(sink.clone()),
            1_000,
        )
        .expect("open");

    // A second connection to the same database — the "anywhere" a durable
    // cancel may be requested from.
    let mut observer = StateStore::open(&db.path).expect("second connection");

    let report = {
        let mut future = Box::pin(require_send(coordinator.run_next(
            &mut store,
            &source,
            &mut staging,
            &clock,
        )));
        // Drive until the first chunk is durably staged.
        let mut budget = 1_000_000usize;
        while transfer_row(&mut observer, opened.transfer)
            .completed_ranges
            .is_empty()
        {
            assert!(budget > 0, "the first chunk never staged");
            budget -= 1;
            assert!(
                exec::poll_n(future.as_mut(), 1).is_pending(),
                "the run finished before the cancel landed"
            );
        }
        // Phase one lands durably, from outside the running attempt.
        assert!(
            TransferMachine::default()
                .request_cancel(&mut observer, opened.transfer, 2_000)
                .expect("request cancel")
        );
        match exec::poll_n(future.as_mut(), 1_000_000) {
            Poll::Ready(result) => match result.expect("run_next") {
                RunOutcome::Ran(report) => report,
                RunOutcome::Idle => panic!("expected the claimed run"),
            },
            Poll::Pending => panic!("the cancel was never observed"),
        }
    };

    assert!(matches!(report.end, AttemptEnd::Cancelled));
    assert_eq!(
        report.readers,
        vec![ReaderReport {
            reader: opened.reader,
            end: ReaderEnd::Failed {
                category: FailureCategory::Cancelled,
            },
        }]
    );
    // AC: cancellation is prompt — the checkpoint after the in-flight
    // chunk observed the flag; the remaining two chunks never hit the
    // network.
    assert_eq!(fetch_ranges(&source).len(), 2);

    // SYNC-043: what remains is safely disposable, and the disposal names
    // it exactly.
    assert_eq!(report.disposals.len(), 1);
    let row = transfer_row(&mut store, opened.transfer);
    assert_eq!(row.state, TransferState::Cancelled);
    assert_eq!(row.completed_ranges, vec![]);
    assert_eq!(row.temp_ref, None);
}

#[test]
fn reader_stop_unsubscribes_without_stopping_the_transfer() {
    let mut rig = rig_with(script(vec![]), config(16, 1, 1));
    let full = SharedSink::new(range(0, 32));
    let stopping = SharedSink::stopping_after(range(0, 32), 0);
    let full_open = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 32),
        Priority::FOREGROUND,
        &full,
    );
    let stopping_open = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 32),
        Priority::FOREGROUND,
        &stopping,
    );

    let report = run_report(&mut rig);
    assert!(matches!(report.end, AttemptEnd::Promoted { .. }));
    assert!(report.readers.contains(&ReaderReport {
        reader: full_open.reader,
        end: ReaderEnd::Satisfied,
    }));
    assert!(report.readers.contains(&ReaderReport {
        reader: stopping_open.reader,
        end: ReaderEnd::Stopped,
    }));
    assert_eq!(full.bytes(), content_v1()[0..32].to_vec());
    assert_eq!(
        stopping.bytes(),
        content_v1()[0..16].to_vec(),
        "the stopping reader took the first chunk and no more"
    );
}

#[test]
fn close_unsubscribes_and_reports_remaining_readers() {
    let mut rig = rig_with(script(vec![]), config(16, 1, 1));
    let sink1 = SharedSink::new(range(0, 16));
    let sink2 = SharedSink::new(range(0, 16));
    let first = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 16),
        Priority::FOREGROUND,
        &sink1,
    );
    let second = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 16),
        Priority::FOREGROUND,
        &sink2,
    );
    assert_eq!(second.transfer, first.transfer);

    let closed = rig.coordinator.close(first.reader).expect("close");
    assert_eq!(closed.transfer, first.transfer);
    assert_eq!(closed.remaining_readers, 1);
    let closed = rig.coordinator.close(second.reader).expect("close");
    assert_eq!(closed.remaining_readers, 0);
    assert_eq!(rig.coordinator.close(second.reader), None, "already closed");

    // The transfer itself keeps running — cancelling it is the host's
    // separate, durable decision.
    let report = run_report(&mut rig);
    assert!(matches!(report.end, AttemptEnd::Promoted { .. }));
    assert!(report.readers.is_empty());
}

#[test]
fn sinkless_subscribers_share_cancellation_ownership() {
    let mut rig = rig_with(script(vec![]), config(16, 1, 1));
    let now = rig.clock.now();
    let first = rig
        .coordinator
        .subscribe(&mut rig.store, &photo_id(), &[], Priority::FOREGROUND, now)
        .expect("first subscription");
    let second = rig
        .coordinator
        .subscribe(&mut rig.store, &photo_id(), &[], Priority::FOREGROUND, now)
        .expect("second subscription");
    assert_eq!(second.transfer, first.transfer);
    assert!(second.coalesced);
    assert_eq!(rig.coordinator.reader_count(first.transfer), 2);

    let closed = rig.coordinator.close(first.reader).expect("close first");
    assert_eq!(closed.remaining_readers, 1);
    assert_eq!(rig.coordinator.reader_count(first.transfer), 1);
    let closed = rig.coordinator.close(second.reader).expect("close second");
    assert_eq!(closed.remaining_readers, 0);
    assert_eq!(rig.coordinator.reader_count(first.transfer), 0);
}

// ---------------------------------------------------------------------------
// Crash-resume: dropped run futures leave resumable state (SYNC-042)
// ---------------------------------------------------------------------------

#[test]
fn dropped_run_future_leaves_resumable_state() {
    let source_script = script(vec![
        Fault::on(Operation::Fetch)
            .occurrence(Occurrence::Nth(2))
            .delay(1_000_000),
    ]);
    let mut rig = rig_with(source_script, config(16, 1, 1));
    let sink = SharedSink::new(range(0, 64));
    let opened = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 64),
        Priority::FOREGROUND,
        &sink,
    );

    {
        let Rig {
            ref mut store,
            ref source,
            ref mut coordinator,
            ref mut staging,
            ref clock,
        } = rig;
        let mut future = Box::pin(coordinator.run_next(store, source, staging, clock));
        // Chunk 1 completes and is recorded; chunk 2 hangs in its scripted
        // delay. Dropping here is the local crash/cancel (SYNC-005).
        assert!(exec::poll_n(future.as_mut(), 50).is_pending());
    }

    let row = transfer_row(&mut rig.store, opened.transfer);
    assert_eq!(
        row.state,
        TransferState::Running,
        "the claim died in flight"
    );
    assert_eq!(row.completed_ranges, vec![range(0, 16)]);
    let staging_handle = row.temp_ref.clone().expect("staging recorded");

    // Startup reconciliation returns the interrupted row to the queue with
    // its progress intact (SYNC-070).
    let now = rig.clock.now();
    let report = rig.store.reconcile(&rig.staging, now).expect("reconcile");
    assert!(!report.repaired.is_empty());
    let row = transfer_row(&mut rig.store, opened.transfer);
    assert_eq!(row.state, TransferState::Queued);
    assert_eq!(row.completed_ranges, vec![range(0, 16)]);
    assert_eq!(row.temp_ref.as_deref(), Some(staging_handle.as_str()));

    // The next run resumes from the staged bytes: [0, 16) never crosses
    // the network again.
    let report = run_report(&mut rig);
    assert!(matches!(report.end, AttemptEnd::Promoted { .. }));
    assert!(report.readers.contains(&ReaderReport {
        reader: opened.reader,
        end: ReaderEnd::Satisfied,
    }));
    assert!(sink.is_complete());
    assert_eq!(sink.bytes(), content_v1());
    let ranges = fetch_ranges(&rig.source);
    assert_eq!(
        ranges
            .iter()
            .filter(|fetched| **fetched == range(0, 16))
            .count(),
        1,
        "staged bytes are never re-fetched after the crash"
    );
}

// ---------------------------------------------------------------------------
// Fail-closed edges
// ---------------------------------------------------------------------------

#[test]
fn unknown_extent_suspends_until_metadata_records_size() {
    let mut rig = rig_with_size(script(vec![]), config(16, 1, 1), None);
    let now = rig.clock.now();
    let outcome = rig
        .coordinator
        .hydrate(&mut rig.store, &photo_id(), &[], Priority::BACKGROUND, now)
        .expect("hydrate");
    let RequestOutcome::Created { transfer, .. } = outcome else {
        panic!("expected Created, got {outcome:?}");
    };

    let report = run_report(&mut rig);
    assert!(matches!(report.end, AttemptEnd::ExtentUnknown));
    assert_eq!(
        fetch_ranges(&rig.source),
        vec![],
        "nothing was fetched blind"
    );
    assert_eq!(
        transfer_row(&mut rig.store, transfer).state,
        TransferState::Suspended
    );

    // A metadata refresh records the size; the host resumes.
    {
        let tx = rig.store.write_txn().expect("write");
        tx.update_item_content(
            &photo_id(),
            Some(&version("v1")),
            &FileFacts {
                mime_type: None,
                logical_size: Some(64),
                content_version: Some(version("v1")),
            },
            &metadata("m2"),
            now,
        )
        .expect("record size");
        tx.commit().expect("commit");
    }
    rig.coordinator
        .resume(&mut rig.store, transfer, now)
        .expect("resume");
    let report = run_report(&mut rig);
    assert!(matches!(report.end, AttemptEnd::Promoted { .. }));
    assert_eq!(
        fetch_ranges(&rig.source).len(),
        4,
        "the whole object, chunked"
    );
}

/// A backend that delivers at the wrong offset — the contract failure the
/// verified sink must catch before it corrupts range accounting
/// (SYNC-046).
#[derive(Debug)]
struct MisdeliveringSource {
    inner: FakeSource,
}

impl DriveSource for MisdeliveringSource {
    fn scope(&self) -> AccountScope {
        self.inner.scope()
    }

    fn root(&self) -> SourceFuture<'_, gramdrive_engine::source::SourceItem> {
        self.inner.root()
    }

    fn children(
        &self,
        parent: ItemId,
        request: gramdrive_engine::source::PageRequest,
    ) -> SourceFuture<'_, gramdrive_engine::source::ItemPage> {
        self.inner.children(parent, request)
    }

    fn latest_cursor(&self) -> SourceFuture<'_, gramdrive_engine::model::cursor::ChangeCursor> {
        self.inner.latest_cursor()
    }

    fn changes(
        &self,
        cursor: gramdrive_engine::model::cursor::ChangeCursor,
    ) -> SourceFuture<'_, gramdrive_engine::source::ChangePage> {
        self.inner.changes(cursor)
    }

    fn fetch<'a>(
        &'a self,
        request: gramdrive_engine::source::FetchRequest,
        sink: &'a mut dyn ContentSink,
    ) -> SourceFuture<'a, ()> {
        Box::pin(async move {
            let bytes = [0u8; 4];
            let chunk = ContentChunk::new(request.range.start() + 1, &bytes)
                .expect("test chunk is valid in isolation");
            match sink.accept(chunk) {
                SinkControl::Continue => Ok(()),
                SinkControl::Stop => Err(SourceError::Cancelled {
                    detail: "sink stopped delivery".to_owned(),
                }),
            }
        })
    }

    fn thumbnail(
        &self,
        item: ItemId,
        spec: gramdrive_engine::source::ThumbnailSpec,
    ) -> SourceFuture<'_, Option<gramdrive_engine::source::Thumbnail>> {
        self.inner.thumbnail(item, spec)
    }
}

#[test]
fn contract_violating_source_fails_terminal() {
    let mut rig = rig_with(script(vec![]), config(16, 1, 1));
    let broken = MisdeliveringSource {
        inner: FakeSource::new(script(vec![])),
    };
    let sink = SharedSink::new(range(0, 16));
    let opened = open_reader(
        &mut rig,
        &photo_id(),
        range(0, 16),
        Priority::FOREGROUND,
        &sink,
    );

    let outcome = exec::drive(rig.coordinator.run_next(
        &mut rig.store,
        &broken,
        &mut rig.staging,
        &rig.clock,
    ))
    .expect("run_next");
    let RunOutcome::Ran(report) = outcome else {
        panic!("expected a run");
    };
    assert!(
        matches!(
            report.end,
            AttemptEnd::Failed {
                category: FailureCategory::Internal,
            }
        ),
        "a contract-violating source is terminal, got {:?}",
        report.end
    );
    assert_eq!(
        report.readers,
        vec![ReaderReport {
            reader: opened.reader,
            end: ReaderEnd::Failed {
                category: FailureCategory::Internal,
            },
        }]
    );
    assert_eq!(
        sink.bytes(),
        Vec::<u8>::new(),
        "no violating byte reached the reader"
    );
    assert_eq!(
        transfer_row(&mut rig.store, opened.transfer).state,
        TransferState::Failed
    );
}
