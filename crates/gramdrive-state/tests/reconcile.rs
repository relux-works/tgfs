//! Startup reconciliation and the repair plan (TASK-260715-21clwh;
//! SYNC-070, SYNC-071, NFR-034).
//!
//! The suite is built out of the fixtures the acceptance criteria name:
//! *missing* (a row for bytes that are gone), *extra* (bytes no row claims),
//! and *corruption* (a file whose in-flight state outlived the process that
//! owned it). Each one is asserted to converge, to converge *idempotently*,
//! and never at the cost of a pin.
//!
//! The last test is not a simulation: it re-executes this binary, lets the
//! child die with work in flight, and reconciles what the child actually
//! left on disk.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use std::collections::HashSet;
use std::sync::Mutex;

use common::{TempDb, account_record, chat_record, scope};
use gramdrive_state::model::ByteRange;
use gramdrive_state::model::identity::{
    AppearanceKey, ChatListKind, DocFormat, DocPartition, ItemId, ItemKey,
};
use gramdrive_state::model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::repo::{
    CacheEntryRecord, CacheKind, CacheVerification, FileFacts, ItemAvailability, ItemRecord,
    PinOrigin, TransferState,
};
use gramdrive_state::{
    Finding, LocalStorage, Resolution, StateError, StateStore, StorageError, StoredObject,
};

const CHAT: i64 = 100;
const NOW: i64 = 5_000;

// ---------------------------------------------------------------------------
// A local storage the tests can corrupt on purpose.
// ---------------------------------------------------------------------------

/// A host storage whose whole content is a list of handles. That is exactly
/// the surface reconciliation uses — it never opens an object, only asks who
/// exists — so a fake with no bytes in it is not a weaker test than a
/// directory would be, it is the same test without the filesystem.
#[derive(Debug, Default)]
struct FakeStorage {
    inner: Mutex<FakeInner>,
}

#[derive(Debug, Default)]
struct FakeInner {
    cache: Vec<StoredObject>,
    staging: Vec<StoredObject>,
    /// Handles whose deletion fails, and with what.
    refuse: Vec<String>,
    /// Whether listing itself fails.
    inventory_broken: bool,
    /// Every handle a deletion actually removed, in order.
    removed: Vec<String>,
}

impl FakeStorage {
    fn new() -> Self {
        Self::default()
    }

    fn with_cache(self, objects: &[(&str, u64)]) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .cache
            .extend(objects.iter().map(|(reference, size)| StoredObject {
                reference: (*reference).to_owned(),
                size: *size,
            }));
        self
    }

    fn with_staging(self, objects: &[(&str, u64)]) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .staging
            .extend(objects.iter().map(|(reference, size)| StoredObject {
                reference: (*reference).to_owned(),
                size: *size,
            }));
        self
    }

    fn refusing(self, reference: &str) -> Self {
        self.inner
            .lock()
            .expect("lock")
            .refuse
            .push(reference.to_owned());
        self
    }

    fn with_broken_inventory(self) -> Self {
        self.inner.lock().expect("lock").inventory_broken = true;
        self
    }

    fn removed(&self) -> Vec<String> {
        self.inner.lock().expect("lock").removed.clone()
    }

    fn holds_cache(&self, reference: &str) -> bool {
        self.inner
            .lock()
            .expect("lock")
            .cache
            .iter()
            .any(|object| object.reference == reference)
    }

    fn holds_staging(&self, reference: &str) -> bool {
        self.inner
            .lock()
            .expect("lock")
            .staging
            .iter()
            .any(|object| object.reference == reference)
    }

    fn remove(&self, reference: &str, staging: bool) -> Result<(), StorageError> {
        let mut inner = self.inner.lock().expect("lock");
        if inner.refuse.iter().any(|name| name == reference) {
            return Err(StorageError::new(format!("permission denied: {reference}")));
        }
        let list = if staging {
            &mut inner.staging
        } else {
            &mut inner.cache
        };
        list.retain(|object| object.reference != reference);
        inner.removed.push(reference.to_owned());
        Ok(())
    }
}

impl LocalStorage for FakeStorage {
    fn cache_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        let inner = self.inner.lock().expect("lock");
        if inner.inventory_broken {
            return Err(StorageError::new("cache container is not mounted"));
        }
        Ok(inner.cache.clone())
    }

    fn staging_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        let inner = self.inner.lock().expect("lock");
        if inner.inventory_broken {
            return Err(StorageError::new("staging container is not mounted"));
        }
        Ok(inner.staging.clone())
    }

    fn remove_cache_object(&self, reference: &str) -> Result<(), StorageError> {
        self.remove(reference, false)
    }

    fn remove_staging_object(&self, reference: &str) -> Result<(), StorageError> {
        self.remove(reference, true)
    }
}

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

fn content_version(text: &str) -> ContentVersion {
    ContentVersion::new(text).expect("valid version")
}

fn doc_id(year: u16) -> ItemId {
    ItemKey::Appearance(AppearanceKey {
        view: ChatListKind::Main,
        item: common::doc_key(CHAT, DocPartition::Year { year }, DocFormat::Ndjson),
    })
    .id()
}

fn seed(store: &mut StateStore, years: &[u16]) {
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&account_record()).expect("account");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    let root = common::account_root_id();
    tx.upsert_item(&ItemRecord {
        id: root.clone(),
        parent: None,
        display_name: "Root".to_owned(),
        safe_name: "Root".to_owned(),
        metadata_version: MetadataVersion::new("m1").expect("version"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("root");
    for year in years {
        tx.upsert_item(&ItemRecord {
            id: doc_id(*year),
            parent: Some(root.clone()),
            display_name: format!("{year}.ndjson"),
            safe_name: format!("{year}.ndjson"),
            metadata_version: MetadataVersion::new("m1").expect("version"),
            content: Some(FileFacts {
                content_version: Some(content_version("v1")),
                ..FileFacts::default()
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: None,
            modified_at_ms: None,
            deleted_at_ms: None,
        })
        .expect("doc");
    }
    tx.commit().expect("commit");
}

fn store_with_docs(years: &[u16]) -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    seed(&mut store, years);
    store
}

fn cache_entry(item: &ItemId, reference: Option<&str>, pin: Option<PinOrigin>) -> CacheEntryRecord {
    CacheEntryRecord {
        item: item.clone(),
        account: scope().account,
        content_version: content_version("v1"),
        kind: CacheKind::GeneratedDoc,
        size: 64,
        blob_hash: None,
        verification: CacheVerification::Verified,
        pin,
        last_access_at_ms: 2_000,
        materialized_at_ms: 1_000,
        materialization_ref: reference.map(str::to_owned),
    }
}

/// Puts `item` on disk-and-in-database as a materialized cache entry.
fn materialize(store: &mut StateStore, item: &ItemId, reference: &str, pin: Option<PinOrigin>) {
    let tx = store.write_txn().expect("write");
    if let Some(origin) = pin {
        tx.pin_item(item, origin, 1_000).expect("pin");
    }
    tx.upsert_cache_entry(&cache_entry(item, Some(reference), pin))
        .expect("entry");
    tx.commit().expect("commit");
}

/// A transfer left `running` with durable progress — what a process that
/// died mid-hydration leaves behind.
fn running_transfer(store: &mut StateStore, item: &ItemId, staging: &str) -> i64 {
    let tx = store.write_txn().expect("write");
    let outcome = tx
        .enqueue_transfer(item, &content_version("v1"), &[], 0, 1_000)
        .expect("enqueue");
    let claimed = tx.claim_next_transfer(1_100).expect("claim").expect("row");
    tx.record_transfer_progress(
        claimed.id,
        &[ByteRange::new(0, 32).expect("range")],
        Some(staging),
        1_200,
    )
    .expect("progress");
    tx.commit().expect("commit");
    outcome.transfer_id().0
}

fn kinds(findings: &[Finding]) -> Vec<&'static str> {
    findings
        .iter()
        .map(|finding| match finding {
            Finding::InterruptedTransfer { .. } => "interrupted_transfer",
            Finding::LeakedStaging { .. } => "leaked_staging",
            Finding::MissingCacheObject { .. } => "missing_cache_object",
            Finding::UnlocatableCacheEntry { .. } => "unlocatable_cache_entry",
            Finding::OrphanCacheObject { .. } => "orphan_cache_object",
            Finding::ProjectionRebuildPending { .. } => "projection_rebuild_pending",
            Finding::MigrationInterrupted { .. } => "migration_interrupted",
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The clean case.
// ---------------------------------------------------------------------------

#[test]
fn a_consistent_file_reconciles_to_nothing() {
    let mut store = store_with_docs(&[2026]);
    materialize(&mut store, &doc_id(2026), "object-2026", None);
    let storage = FakeStorage::new().with_cache(&[("object-2026", 64)]);

    let report = store.reconcile(&storage, NOW).expect("reconcile");

    assert!(
        report.plan.is_empty(),
        "findings: {:?}",
        report.plan.findings
    );
    assert!(!report.plan.dirty_shutdown);
    assert!(report.converged());
    assert!(
        storage.removed().is_empty(),
        "a consistent file must cost nothing"
    );
}

// ---------------------------------------------------------------------------
// Corruption fixture: in-flight state that outlived its process.
// ---------------------------------------------------------------------------

#[test]
fn an_interrupted_transfer_is_requeued_with_its_staged_progress_intact() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    let id = running_transfer(&mut store, &item, "staging-1");
    let storage = FakeStorage::new().with_staging(&[("staging-1", 32)]);

    let report = store.reconcile(&storage, NOW).expect("reconcile");

    assert!(
        report.plan.dirty_shutdown,
        "a running row nobody runs is one"
    );
    assert_eq!(kinds(&report.plan.findings), ["interrupted_transfer"]);
    assert_eq!(
        report.plan.findings[0].resolution(),
        Resolution::RequeueTransfer
    );
    assert!(report.converged());

    let read = store.read_txn().expect("read");
    let transfer = read
        .transfer(gramdrive_state::repo::TransferId(id))
        .expect("transfer")
        .expect("row");
    assert_eq!(transfer.state, TransferState::Queued, "back on the queue");
    assert_eq!(
        transfer.completed_ranges,
        [ByteRange::new(0, 32).expect("range")],
        "rolled forward from the checkpoint, not restarted from zero"
    );
    assert_eq!(
        transfer.temp_ref.as_deref(),
        Some("staging-1"),
        "the staging area it resumes into survives"
    );
    assert_eq!(
        transfer.retry_count, 0,
        "a crash is not a failed attempt (SYNC-044)"
    );
    drop(read);

    assert!(
        storage.holds_staging("staging-1"),
        "deleting the staged bytes would be the real data loss here"
    );
}

#[test]
fn a_requeued_transfers_staging_area_is_never_deleted() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    running_transfer(&mut store, &item, "staging-1");
    let storage = FakeStorage::new().with_staging(&[("staging-1", 32)]);

    store.reconcile(&storage, NOW).expect("reconcile");

    assert!(
        storage.holds_staging("staging-1"),
        "the transfer this pass just requeued resumes from exactly these bytes"
    );
}

#[test]
fn a_terminal_transfers_staging_area_is_deleted_and_its_handle_cleared() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    let id = running_transfer(&mut store, &item, "staging-1");

    let tx = store.write_txn().expect("write");
    tx.mark_transfer_done(gramdrive_state::repo::TransferId(id), 2_000)
        .expect("done");
    tx.commit().expect("commit");

    let storage = FakeStorage::new().with_staging(&[("staging-1", 32)]);
    let report = store.reconcile(&storage, NOW).expect("reconcile");

    assert_eq!(kinds(&report.plan.findings), ["leaked_staging"]);
    assert!(report.converged());
    assert_eq!(storage.removed(), ["staging-1"]);

    let read = store.read_txn().expect("read");
    let transfer = read
        .transfer(gramdrive_state::repo::TransferId(id))
        .expect("transfer")
        .expect("row");
    assert_eq!(
        transfer.temp_ref, None,
        "the handle must not outlive the bytes it names"
    );
}

// ---------------------------------------------------------------------------
// Missing fixture: a row for bytes that are gone.
// ---------------------------------------------------------------------------

#[test]
fn a_missing_cache_object_drops_the_entry_and_keeps_the_pin() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    materialize(&mut store, &item, "object-2026", Some(PinOrigin::User));
    // The host lists nothing: the OS purged the container (SYNC-053).
    let storage = FakeStorage::new();

    let report = store.reconcile(&storage, NOW).expect("reconcile");

    assert_eq!(kinds(&report.plan.findings), ["missing_cache_object"]);
    assert!(matches!(
        report.plan.findings[0],
        Finding::MissingCacheObject { pinned: true, .. }
    ));
    assert!(report.converged());

    let read = store.read_txn().expect("read");
    assert_eq!(
        read.cache_entry(&item).expect("entry"),
        None,
        "the row claimed bytes that do not exist"
    );
    let pin = read.pin(&item).expect("pin").expect("the pin must survive");
    assert_eq!(
        pin.origin,
        PinOrigin::User,
        "POL-2 intent is not materialization; losing it would lose the user's choice"
    );
}

#[test]
fn a_missing_generated_document_goes_back_on_the_render_worklist() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    materialize(&mut store, &item, "object-2026", None);

    let tx = store.write_txn().expect("write");
    tx.ensure_render_state(&item, 1, 1).expect("render state");
    tx.publish_render(
        &item,
        &common::chat_key(CHAT),
        0,
        &gramdrive_state::repo::RenderOutput {
            content_version: content_version("v1"),
            content_hash: None,
            logical_size: 64,
        },
        1_500,
    )
    .expect("publish");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    assert!(
        !read.render_state(&item).expect("state").expect("row").dirty,
        "fixture precondition: the document is published and clean"
    );
    drop(read);

    let report = store
        .reconcile(&FakeStorage::new(), NOW)
        .expect("reconcile");
    assert_eq!(kinds(&report.plan.findings), ["missing_cache_object"]);

    let read = store.read_txn().expect("read");
    assert!(
        read.render_state(&item).expect("state").expect("row").dirty,
        "bytes that vanished mean the document is unpublished again (SYNC-024)"
    );
    assert_eq!(
        read.dirty_render_items(10).expect("worklist"),
        [item],
        "and it must be on the worklist that re-renders it"
    );
}

#[test]
fn an_entry_without_a_handle_is_reported_rather_than_dropped() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    let tx = store.write_txn().expect("write");
    tx.upsert_cache_entry(&cache_entry(&item, None, None))
        .expect("entry");
    tx.commit().expect("commit");

    let report = store
        .reconcile(&FakeStorage::new(), NOW)
        .expect("reconcile");

    assert_eq!(kinds(&report.plan.findings), ["unlocatable_cache_entry"]);
    assert_eq!(report.plan.findings[0].resolution(), Resolution::ReportOnly);
    assert!(
        !report.converged(),
        "an unrepairable finding must be reported as unresolved, not silently dropped"
    );
    assert!(
        report.unresolved[0]
            .reason
            .contains("no materialization handle"),
        "NFR-034 wants the precise unresolved state, got: {}",
        report.unresolved[0].reason
    );

    let read = store.read_txn().expect("read");
    assert!(
        read.cache_entry(&item).expect("entry").is_some(),
        "an entry we cannot check is not an entry we may delete"
    );
}

// ---------------------------------------------------------------------------
// Extra fixture: bytes no row claims.
// ---------------------------------------------------------------------------

#[test]
fn an_orphan_cache_object_is_deleted_and_claimed_ones_are_not() {
    let mut store = store_with_docs(&[2026]);
    materialize(
        &mut store,
        &doc_id(2026),
        "object-2026",
        Some(PinOrigin::User),
    );
    let storage = FakeStorage::new().with_cache(&[("object-2026", 64), ("object-stale", 4_096)]);

    let report = store.reconcile(&storage, NOW).expect("reconcile");

    assert_eq!(kinds(&report.plan.findings), ["orphan_cache_object"]);
    assert!(matches!(
        report.plan.findings[0],
        Finding::OrphanCacheObject { size: 4_096, .. }
    ));
    assert!(report.converged());
    assert_eq!(storage.removed(), ["object-stale"]);
    assert!(
        storage.holds_cache("object-2026"),
        "the pinned object a row still claims must survive"
    );
}

// ---------------------------------------------------------------------------
// Markers: work this crate does not own.
// ---------------------------------------------------------------------------

#[test]
fn a_projection_rebuild_marker_is_reported_and_stays_raised() {
    let mut store = store_with_docs(&[2026]);
    store
        .raise_repair_marker(gramdrive_state::RepairKind::RebuildProjection, "items")
        .expect("marker");

    let report = store
        .reconcile(&FakeStorage::new(), NOW)
        .expect("reconcile");

    assert_eq!(kinds(&report.plan.findings), ["projection_rebuild_pending"]);
    assert!(!report.converged());
    assert!(report.unresolved[0].reason.contains("projection builder"));
    assert_eq!(
        store.repair_markers().expect("markers").len(),
        1,
        "the work is still owed, so the marker must not be cleared by reporting it"
    );
}

// ---------------------------------------------------------------------------
// Convergence.
// ---------------------------------------------------------------------------

#[test]
fn reconciling_a_broken_file_twice_changes_nothing_the_second_time() {
    let mut store = store_with_docs(&[2024, 2025, 2026]);
    // Every fixture at once: a dead claim, a leaked staging area, a row for
    // bytes that are gone, and bytes no row claims.
    running_transfer(&mut store, &doc_id(2024), "staging-live");
    let done = running_transfer(&mut store, &doc_id(2025), "staging-done");
    let tx = store.write_txn().expect("write");
    tx.mark_transfer_done(gramdrive_state::repo::TransferId(done), 2_000)
        .expect("done");
    tx.commit().expect("commit");
    materialize(
        &mut store,
        &doc_id(2026),
        "object-gone",
        Some(PinOrigin::User),
    );

    let storage = FakeStorage::new()
        .with_cache(&[("object-orphan", 128)])
        .with_staging(&[("staging-live", 32), ("staging-done", 16)]);

    let first = store.reconcile(&storage, NOW).expect("first pass");
    assert_eq!(
        kinds(&first.plan.findings)
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([
            "interrupted_transfer",
            "leaked_staging",
            "missing_cache_object",
            "orphan_cache_object",
        ])
    );
    assert!(first.converged(), "unresolved: {:?}", first.unresolved);

    let second = store.reconcile(&storage, NOW).expect("second pass");
    assert!(
        second.plan.is_empty(),
        "the pass must be a fixed point: {:?}",
        second.plan.findings
    );
    assert!(!second.plan.dirty_shutdown);

    // And the pin outlived all of it.
    let read = store.read_txn().expect("read");
    assert!(
        read.pin(&doc_id(2026)).expect("pin").is_some(),
        "no fixture may cost a pin"
    );
}

// ---------------------------------------------------------------------------
// Failure handling.
// ---------------------------------------------------------------------------

#[test]
fn a_storage_failure_on_one_object_still_repairs_every_other_finding() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    materialize(&mut store, &item, "object-gone", None);
    let storage = FakeStorage::new()
        .with_cache(&[("object-stubborn", 128), ("object-orphan", 64)])
        .refusing("object-stubborn");

    let report = store.reconcile(&storage, NOW).expect("reconcile");

    assert_eq!(report.unresolved.len(), 1);
    assert!(
        report.unresolved[0]
            .reason
            .contains("permission denied: object-stubborn"),
        "the host's own words survive to the report: {}",
        report.unresolved[0].reason
    );
    assert_eq!(
        report.repaired.len(),
        2,
        "one stubborn object must not strand the rest of the file"
    );
    assert_eq!(storage.removed(), ["object-orphan"]);
    let read = store.read_txn().expect("read");
    assert_eq!(read.cache_entry(&item).expect("entry"), None);
}

#[test]
fn an_uninventoriable_storage_aborts_the_pass_without_writing() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    materialize(&mut store, &item, "object-2026", Some(PinOrigin::User));
    let storage = FakeStorage::new().with_broken_inventory();

    let error = store
        .reconcile(&storage, NOW)
        .expect_err("an unlistable container must not be read as an empty one");
    assert!(
        matches!(error, StateError::LocalStorage { .. }),
        "got: {error:?}"
    );

    let read = store.read_txn().expect("read");
    assert!(
        read.cache_entry(&item).expect("entry").is_some(),
        "an empty listing from a broken container would have dropped every entry in the file"
    );
}

#[test]
fn planning_reports_the_same_findings_and_writes_nothing() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    materialize(&mut store, &item, "object-gone", Some(PinOrigin::User));
    running_transfer(&mut store, &doc_id(2026), "staging-1");
    let storage = FakeStorage::new().with_cache(&[("object-orphan", 64)]);

    let plan = store.plan_reconcile(&storage).expect("plan");
    assert!(plan.dirty_shutdown);
    assert_eq!(
        kinds(&plan.findings).into_iter().collect::<HashSet<_>>(),
        HashSet::from([
            "interrupted_transfer",
            "missing_cache_object",
            "orphan_cache_object"
        ])
    );

    assert!(
        storage.removed().is_empty(),
        "a dry run that deletes is not a dry run"
    );
    let read = store.read_txn().expect("read");
    assert!(read.cache_entry(&item).expect("entry").is_some());
    drop(read);

    // And the plan is what the repair pass then acts on, unchanged.
    let report = store.reconcile(&storage, NOW).expect("reconcile");
    assert_eq!(report.plan, plan);
}

// ---------------------------------------------------------------------------
// A real dead process.
// ---------------------------------------------------------------------------

/// Set on the child; its value is the database path.
const CRASH_CHILD: &str = "GRAMDRIVE_RECONCILE_CRASH_CHILD";

/// The child half: commit work in flight, open one more transaction, and die
/// without unwinding — no destructors, no rollback, no SQLite shutdown. What
/// the parent then opens is a file a killed process left, not a file a test
/// arranged to look like one.
fn crash_with_work_in_flight(path: &str) -> ! {
    let mut store = StateStore::open(path).expect("child: open");
    seed(&mut store, &[2026]);
    let item = doc_id(2026);
    running_transfer(&mut store, &item, "staging-1");
    materialize(&mut store, &item, "object-2026", Some(PinOrigin::User));

    // Uncommitted work: this must be gone entirely when the parent looks.
    let tx = store.write_txn().expect("child: write");
    tx.pin_item(&common::account_root_id(), PinOrigin::ArchiveMode, 3_000)
        .expect("child: pin");
    std::mem::forget(tx);

    std::process::abort();
}

#[test]
fn a_file_left_by_a_killed_process_reconciles_to_a_consistent_state() {
    if let Ok(path) = std::env::var(CRASH_CHILD) {
        crash_with_work_in_flight(&path);
    }

    let db = TempDb::new();
    let exe = std::env::current_exe().expect("test binary");
    let status = std::process::Command::new(exe)
        .args([
            "--exact",
            "a_file_left_by_a_killed_process_reconciles_to_a_consistent_state",
        ])
        .env(CRASH_CHILD, &db.path)
        .output()
        .expect("spawn the child");
    assert!(
        !status.status.success(),
        "the child was supposed to die, not to pass"
    );

    // The parent opens what the dead process actually left on disk.
    let mut store = StateStore::open(&db.path).expect("reopen after the crash");
    let item = doc_id(2026);

    let read = store.read_txn().expect("read");
    assert!(
        read.pin(&common::account_root_id()).expect("pin").is_none(),
        "the child's uncommitted transaction must have died with it"
    );
    assert!(
        read.pin(&item).expect("pin").is_some(),
        "its committed pin must have survived"
    );
    drop(read);

    let storage = FakeStorage::new()
        .with_cache(&[("object-2026", 64)])
        .with_staging(&[("staging-1", 32)]);

    let report = store.reconcile(&storage, NOW).expect("reconcile");
    assert!(
        report.plan.dirty_shutdown,
        "the file carries a claim whose owner is dead"
    );
    assert_eq!(kinds(&report.plan.findings), ["interrupted_transfer"]);
    assert!(report.converged());

    let read = store.read_txn().expect("read");
    let transfer = read
        .live_transfer_for(&item, &content_version("v1"))
        .expect("transfers")
        .expect("the transfer the child claimed");
    assert_eq!(transfer.state, TransferState::Queued);
    assert_eq!(
        transfer.completed_ranges,
        [ByteRange::new(0, 32).expect("range")],
        "the child's durable progress is what makes this a resume and not a restart"
    );
    assert!(read.pin(&item).expect("pin").is_some(), "and the pin held");
    drop(read);

    let again = store.reconcile(&storage, NOW).expect("second reconcile");
    assert!(again.plan.is_empty(), "restart converges idempotently");
}
