//! The durable transfer state machine (TASK-260715-g4k3zm): request
//! validation and coalescing, resume plans from persisted ranges, the
//! promotion gate, deterministic version-race invalidation, the retry
//! budget, parking, two-phase cancellation — and the crash-resume proof
//! over a real file-backed store.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use gramdrive_engine::model::ByteRange;
use gramdrive_engine::model::identity::{
    AccountId, AccountKey, AccountScope, AppearanceKey, CanonicalKey, ChatId, ChatKey,
    ChatListKind, DocFormat, DocPartition, GeneratedDocKey, ItemId, ItemKey, NamespaceVersion,
    SchemaFamily,
};
use gramdrive_engine::model::version::{ContentVersion, MetadataVersion};
use gramdrive_engine::source::SourceError;
use gramdrive_engine::state::repo::{
    AccountRecord, ChatRecord, ChatType, FailureCategory, FileFacts, ItemAvailability, ItemRecord,
    RetentionMode, SourceKind, TransferId, TransferState,
};
use gramdrive_engine::state::{LocalStorage, StateError, StateStore, StorageError, StoredObject};
use gramdrive_engine::transfer::{
    Checkpoint, ClaimOutcome, ClaimedTransfer, CompleteOutcome, EngineError, FailOutcome, Priority,
    Remaining, RequestOutcome, RetryPolicy, StagingDisposal, TransferFault, TransferMachine,
};

const ACCOUNT_ID: i64 = 7;
const NAMESPACE: u32 = 1;
const CHAT: i64 = 100;

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(ACCOUNT_ID),
        },
        namespace_version: NamespaceVersion(NAMESPACE),
    }
}

fn chat_key() -> ChatKey {
    ChatKey {
        scope: scope(),
        chat_id: ChatId(CHAT),
    }
}

fn account_root_id() -> ItemId {
    ItemKey::Canonical(CanonicalKey::Account(scope().account)).id()
}

fn doc_id(year: u16) -> ItemId {
    ItemKey::Appearance(AppearanceKey {
        view: ChatListKind::Main,
        item: CanonicalKey::GeneratedDoc(GeneratedDocKey {
            chat: chat_key(),
            partition: DocPartition::Year { year },
            format: DocFormat::Ndjson,
            schema_family: SchemaFamily(1),
        }),
    })
    .id()
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

fn doc_record(year: u16, facts: Option<FileFacts>) -> ItemRecord {
    ItemRecord {
        aggregate_size: None,
        id: doc_id(year),
        parent: Some(account_root_id()),
        display_name: format!("{year}.ndjson"),
        safe_name: format!("{year}.ndjson"),
        metadata_version: metadata("m1"),
        content: facts,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    }
}

fn v1_facts(size: Option<u64>) -> FileFacts {
    FileFacts {
        mime_type: Some("application/x-ndjson".to_owned()),
        logical_size: size,
        content_version: Some(version("v1")),
    }
}

/// Seeds the scaffold account, chat, root, and one hydratable 64-byte item
/// per requested year, at content version "v1".
fn seed(store: &mut StateStore, years: &[u16]) {
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
        key: chat_key(),
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
        id: account_root_id(),
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
    for year in years {
        tx.upsert_item(&doc_record(*year, Some(v1_facts(Some(64)))))
            .expect("doc");
    }
    tx.commit().expect("commit");
}

fn store_with_docs(years: &[u16]) -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    seed(&mut store, years);
    store
}

/// Moves the item's content to `v2` (size 128), simulating a source-side
/// republish observed by the change pipeline.
fn republish(store: &mut StateStore, year: u16, now_ms: i64) {
    let tx = store.write_txn().expect("write");
    tx.update_item_content(
        &doc_id(year),
        Some(&version("v1")),
        &FileFacts {
            mime_type: Some("application/x-ndjson".to_owned()),
            logical_size: Some(128),
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

fn request_created(
    machine: &TransferMachine,
    store: &mut StateStore,
    year: u16,
    requested: &[ByteRange],
    now_ms: i64,
) -> TransferId {
    match machine
        .request(
            store,
            &doc_id(year),
            requested,
            Priority::FOREGROUND,
            now_ms,
        )
        .expect("request")
    {
        RequestOutcome::Created {
            transfer,
            displaced: None,
        } => transfer,
        other => panic!("expected Created without displacement, got {other:?}"),
    }
}

fn claim_some(
    machine: &TransferMachine,
    store: &mut StateStore,
    now_ms: i64,
) -> Box<ClaimedTransfer> {
    match machine.claim(store, now_ms).expect("claim") {
        ClaimOutcome::Claimed(claim) => claim,
        other => panic!("expected a claim, got {other:?}"),
    }
}

fn transfer_state(store: &mut StateStore, id: TransferId) -> TransferState {
    let read = store.read_txn().expect("read");
    read.transfer(id)
        .expect("transfer")
        .expect("row exists")
        .state
}

fn detail() -> String {
    "diagnostic".to_owned()
}

// ---------------------------------------------------------------------------
// Requesting
// ---------------------------------------------------------------------------

#[test]
fn request_refuses_demand_that_can_never_be_served() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    // Unknown item.
    match machine.request(&mut store, &doc_id(1999), &[], Priority::BACKGROUND, 1_000) {
        Err(EngineError::State(StateError::RowNotFound { entity: "item" })) => {}
        other => panic!("expected RowNotFound(item), got {other:?}"),
    }
    // Directories are never hydrated (SYNC-040).
    match machine.request(
        &mut store,
        &account_root_id(),
        &[],
        Priority::BACKGROUND,
        1_000,
    ) {
        Err(EngineError::NotHydratable { .. }) => {}
        other => panic!("expected NotHydratable, got {other:?}"),
    }
    // A range past the known extent is a caller bug caught before the
    // source sees it.
    match machine.request(
        &mut store,
        &doc_id(2026),
        &[range(32, 65)],
        Priority::BACKGROUND,
        1_000,
    ) {
        Err(EngineError::RangeBeyondExtent {
            end: 65,
            extent: 64,
        }) => {}
        other => panic!("expected RangeBeyondExtent, got {other:?}"),
    }

    // Restricted (POL-4), unavailable, versionless, and tombstoned items
    // all refuse.
    let cases: Vec<ItemRecord> = vec![
        ItemRecord {
            availability: ItemAvailability::Restricted,
            ..doc_record(2026, Some(v1_facts(Some(64))))
        },
        ItemRecord {
            availability: ItemAvailability::Unavailable,
            ..doc_record(2026, Some(v1_facts(Some(64))))
        },
        doc_record(
            2026,
            Some(FileFacts {
                content_version: None,
                ..v1_facts(Some(64))
            }),
        ),
        ItemRecord {
            deleted_at_ms: Some(2_000),
            ..doc_record(2026, Some(v1_facts(Some(64))))
        },
    ];
    for record in cases {
        let tx = store.write_txn().expect("write");
        tx.upsert_item(&record).expect("upsert");
        tx.commit().expect("commit");
        match machine.request(&mut store, &doc_id(2026), &[], Priority::BACKGROUND, 3_000) {
            Err(EngineError::NotHydratable { reason }) => {
                assert!(!reason.is_empty());
            }
            other => panic!("expected NotHydratable for {record:?}, got {other:?}"),
        }
    }
}

#[test]
fn request_pins_the_current_version_and_reports_coverage_on_coalesce() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    let id = request_created(&machine, &mut store, 2026, &[range(0, 32)], 1_000);

    // Covered demand attaches; uncovered demand attaches and says so.
    match machine
        .request(
            &mut store,
            &doc_id(2026),
            &[range(8, 16)],
            Priority::BACKGROUND,
            1_100,
        )
        .expect("request")
    {
        RequestOutcome::Attached {
            transfer,
            covers_request: true,
        } => assert_eq!(transfer, id),
        other => panic!("expected covered Attached, got {other:?}"),
    }
    match machine
        .request(
            &mut store,
            &doc_id(2026),
            &[range(16, 48)],
            Priority::BACKGROUND,
            1_200,
        )
        .expect("request")
    {
        RequestOutcome::Attached {
            transfer,
            covers_request: false,
        } => assert_eq!(transfer, id),
        other => panic!("expected uncovered Attached, got {other:?}"),
    }
    // Whole-object demand against a partial live transfer is uncovered.
    match machine
        .request(&mut store, &doc_id(2026), &[], Priority::BACKGROUND, 1_300)
        .expect("request")
    {
        RequestOutcome::Attached {
            covers_request: false,
            ..
        } => {}
        other => panic!("expected uncovered Attached, got {other:?}"),
    }

    // After the source republishes, new demand pins v2 — a fresh transfer,
    // not the v1 one (SYNC-042).
    republish(&mut store, 2026, 2_000);
    let fresh = request_created(&machine, &mut store, 2026, &[], 2_100);
    assert_ne!(fresh, id);
}

#[test]
fn request_acknowledges_an_abandoned_cancel_and_starts_fresh() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    let abandoned = request_created(&machine, &mut store, 2026, &[], 1_000);
    assert!(
        machine
            .request_cancel(&mut store, abandoned, 1_100)
            .expect("cancel")
    );
    // The flagged row is invisible to claims; new demand acknowledges it.
    match machine.claim(&mut store, 1_200).expect("claim") {
        ClaimOutcome::Empty => {}
        other => panic!("expected Empty, got {other:?}"),
    }
    match machine
        .request(&mut store, &doc_id(2026), &[], Priority::FOREGROUND, 1_300)
        .expect("request")
    {
        RequestOutcome::Created {
            transfer,
            displaced: None,
        } => assert_ne!(transfer, abandoned),
        other => panic!("expected Created, got {other:?}"),
    }
    assert_eq!(
        transfer_state(&mut store, abandoned),
        TransferState::Cancelled
    );
}

// ---------------------------------------------------------------------------
// Claiming and progress
// ---------------------------------------------------------------------------

#[test]
fn claims_plan_remaining_work_from_persisted_ranges() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    request_created(&machine, &mut store, 2026, &[], 1_000);
    let mut claim = claim_some(&machine, &mut store, 1_100);
    let id = claim.id();
    assert_eq!(claim.extent(), Some(64));
    assert_eq!(claim.remaining(), Remaining::Ranges(vec![range(0, 64)]));

    // Staged pieces subtract from the plan; adjacent pieces count as one.
    machine
        .record_progress(
            &mut store,
            &mut claim,
            &[range(0, 16), range(16, 32)],
            "stage-1",
            1_200,
        )
        .expect("progress");
    assert_eq!(claim.remaining(), Remaining::Ranges(vec![range(32, 64)]));
    assert_eq!(claim.staging(), Some("stage-1"));

    // The plan survives suspension: resume, re-claim, same remainder.
    machine.suspend(&mut store, *claim, 1_300).expect("suspend");
    machine.resume(&mut store, id, 1_400).expect("resume");
    let claim = claim_some(&machine, &mut store, 1_500);
    assert_eq!(claim.remaining(), Remaining::Ranges(vec![range(32, 64)]));
    assert_eq!(claim.staging(), Some("stage-1"));
}

#[test]
fn progress_is_monotonic_within_extent_under_one_staging_handle() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    request_created(&machine, &mut store, 2026, &[], 1_000);
    let mut claim = claim_some(&machine, &mut store, 1_100);
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 32)], "stage-1", 1_200)
        .expect("progress");

    // Shrinking reports are refused — staged bytes are durable.
    match machine.record_progress(&mut store, &mut claim, &[range(0, 16)], "stage-1", 1_300) {
        Err(EngineError::ProgressRegression) => {}
        other => panic!("expected ProgressRegression, got {other:?}"),
    }
    // The staging handle is fixed for the transfer's life.
    match machine.record_progress(&mut store, &mut claim, &[range(0, 48)], "stage-2", 1_300) {
        Err(EngineError::StagingChanged) => {}
        other => panic!("expected StagingChanged, got {other:?}"),
    }
    // Progress past the known extent is a bug, not data.
    match machine.record_progress(&mut store, &mut claim, &[range(0, 65)], "stage-1", 1_300) {
        Err(EngineError::RangeBeyondExtent {
            end: 65,
            extent: 64,
        }) => {}
        other => panic!("expected RangeBeyondExtent, got {other:?}"),
    }
    // None of the refusals changed the durable row.
    let read = store.read_txn().expect("read");
    let row = read
        .transfer(claim.id())
        .expect("transfer")
        .expect("row exists");
    assert_eq!(row.completed_ranges, vec![range(0, 32)]);
    assert_eq!(row.temp_ref.as_deref(), Some("stage-1"));
    drop(read);

    // A row moved underneath the claim answers with the durable rule, not
    // the token's stale picture: finish the row externally, then try to
    // record more progress through the stale claim.
    let tx = store.write_txn().expect("write");
    assert!(tx.request_transfer_cancel(claim.id(), 1_400).expect("flag"));
    tx.mark_transfer_cancelled(claim.id(), 1_400).expect("ack");
    tx.commit().expect("commit");
    match machine.record_progress(&mut store, &mut claim, &[range(0, 48)], "stage-1", 1_500) {
        Err(EngineError::State(StateError::InvalidTransition {
            entity: "transfer",
            from: "cancelled",
        })) => {}
        other => panic!("expected InvalidTransition, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The promotion gate
// ---------------------------------------------------------------------------

#[test]
fn promotion_refuses_incomplete_content_and_admits_complete_content() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    request_created(&machine, &mut store, 2026, &[], 1_000);
    let mut claim = claim_some(&machine, &mut store, 1_100);
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 32)], "stage-1", 1_200)
        .expect("progress");

    // Incomplete content never promotes; the refusal changes nothing and
    // the claim stays usable (SYNC-042, NFR-012).
    match machine.complete(&mut store, &claim, 1_300) {
        Err(EngineError::IncompleteContent { missing }) => {
            assert_eq!(missing, vec![range(32, 64)]);
        }
        other => panic!("expected IncompleteContent, got {other:?}"),
    }
    assert_eq!(
        transfer_state(&mut store, claim.id()),
        TransferState::Running
    );

    machine
        .record_progress(&mut store, &mut claim, &[range(0, 64)], "stage-1", 1_400)
        .expect("progress");
    match machine
        .complete(&mut store, &claim, 1_500)
        .expect("complete")
    {
        CompleteOutcome::Promoted { staging } => {
            assert_eq!(staging.as_deref(), Some("stage-1"));
        }
        other => panic!("expected Promoted, got {other:?}"),
    }
    // The done row keeps its evidence: the covered ranges and the staging
    // handle the promotion layer consumes.
    let read = store.read_txn().expect("read");
    let row = read
        .transfer(claim.id())
        .expect("transfer")
        .expect("row exists");
    assert_eq!(row.state, TransferState::Done);
    assert_eq!(row.completed_ranges, vec![range(0, 64)]);
    assert_eq!(row.temp_ref.as_deref(), Some("stage-1"));
    drop(read);

    // The spent claim holds no power: the durable rules refuse it.
    match machine.record_progress(&mut store, &mut claim, &[range(0, 64)], "stage-1", 1_600) {
        Err(EngineError::State(StateError::InvalidTransition {
            entity: "transfer",
            from: "done",
        })) => {}
        other => panic!("expected InvalidTransition, got {other:?}"),
    }
}

#[test]
fn whole_object_promotion_fails_closed_without_a_known_extent() {
    let machine = machine();
    let mut store = store_with_docs(&[]);
    let tx = store.write_txn().expect("write");
    tx.upsert_item(&doc_record(2026, Some(v1_facts(None))))
        .expect("doc");
    tx.commit().expect("commit");

    request_created(&machine, &mut store, 2026, &[], 1_000);
    let mut claim = claim_some(&machine, &mut store, 1_100);
    match claim.remaining() {
        Remaining::UnknownExtent { staged } => assert_eq!(staged, vec![]),
        other => panic!("expected UnknownExtent, got {other:?}"),
    }
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 64)], "stage-1", 1_200)
        .expect("progress");

    // Completeness is unprovable: refuse, leave the transfer live.
    match machine.complete(&mut store, &claim, 1_300) {
        Err(EngineError::UnknownExtent) => {}
        other => panic!("expected UnknownExtent, got {other:?}"),
    }
    assert_eq!(
        transfer_state(&mut store, claim.id()),
        TransferState::Running
    );

    // A metadata refresh records the size (same content version — not a
    // drift); the same staged bytes now prove complete.
    let tx = store.write_txn().expect("write");
    tx.update_item_content(
        &doc_id(2026),
        Some(&version("v1")),
        &v1_facts(Some(64)),
        &metadata("m2"),
        1_400,
    )
    .expect("record size");
    tx.commit().expect("commit");
    match machine
        .complete(&mut store, &claim, 1_500)
        .expect("complete")
    {
        CompleteOutcome::Promoted { staging } => {
            assert_eq!(staging.as_deref(), Some("stage-1"));
        }
        other => panic!("expected Promoted, got {other:?}"),
    }
}

#[test]
fn zero_byte_objects_promote_without_bytes() {
    let machine = machine();
    let mut store = store_with_docs(&[]);
    let tx = store.write_txn().expect("write");
    tx.upsert_item(&doc_record(2026, Some(v1_facts(Some(0)))))
        .expect("doc");
    tx.commit().expect("commit");

    request_created(&machine, &mut store, 2026, &[], 1_000);
    let claim = claim_some(&machine, &mut store, 1_100);
    assert_eq!(claim.remaining(), Remaining::Ranges(vec![]));
    match machine
        .complete(&mut store, &claim, 1_200)
        .expect("complete")
    {
        CompleteOutcome::Promoted { staging: None } => {}
        other => panic!("expected Promoted without staging, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Version races
// ---------------------------------------------------------------------------

#[test]
fn version_races_invalidate_partial_data_deterministically() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    let first = request_created(&machine, &mut store, 2026, &[], 1_000);
    let mut claim = claim_some(&machine, &mut store, 1_100);
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 32)], "stage-1", 1_200)
        .expect("progress");

    // The republish lands mid-transfer; the next checkpoint sees it.
    republish(&mut store, 2026, 1_300);
    match machine.checkpoint(&mut store, &claim).expect("checkpoint") {
        Checkpoint::Drifted => {}
        other => panic!("expected Drifted, got {other:?}"),
    }
    let invalidation = machine
        .invalidate(&mut store, *claim, 1_400)
        .expect("invalidate");
    assert_eq!(invalidation.category, FailureCategory::VersionConflict);
    assert_eq!(
        invalidation.disposal,
        Some(StagingDisposal {
            staging: "stage-1".to_owned(),
        })
    );

    // Deterministic residue: terminal failed/version_conflict, partial
    // data un-claimed, staging handed to the host for deletion.
    let read = store.read_txn().expect("read");
    let row = read.transfer(first).expect("transfer").expect("row exists");
    assert_eq!(row.state, TransferState::Failed);
    assert_eq!(row.failure_category, Some(FailureCategory::VersionConflict));
    assert_eq!(row.completed_ranges, vec![]);
    assert_eq!(row.temp_ref, None);
    drop(read);

    // Fresh demand pins v2 and completes cleanly.
    request_created(&machine, &mut store, 2026, &[range(0, 128)], 1_500);
    let mut claim = claim_some(&machine, &mut store, 1_600);
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 128)], "stage-2", 1_700)
        .expect("progress");
    match machine
        .complete(&mut store, &claim, 1_800)
        .expect("complete")
    {
        CompleteOutcome::Promoted { staging } => {
            assert_eq!(staging.as_deref(), Some("stage-2"));
        }
        other => panic!("expected Promoted, got {other:?}"),
    }
}

#[test]
fn a_drifted_transfer_is_discarded_at_claim_and_at_completion() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    // Drift while queued: the claim pass invalidates instead of fetching.
    let queued = request_created(&machine, &mut store, 2026, &[], 1_000);
    republish(&mut store, 2026, 1_100);
    match machine.claim(&mut store, 1_200).expect("claim") {
        ClaimOutcome::Discarded {
            transfer,
            invalidation,
        } => {
            assert_eq!(transfer, queued);
            assert_eq!(invalidation.category, FailureCategory::VersionConflict);
            assert_eq!(invalidation.disposal, None);
        }
        other => panic!("expected Discarded, got {other:?}"),
    }
    assert_eq!(transfer_state(&mut store, queued), TransferState::Failed);
    match machine.claim(&mut store, 1_300).expect("claim") {
        ClaimOutcome::Empty => {}
        other => panic!("expected Empty, got {other:?}"),
    }

    // Drift discovered only at completion: same resolution, staged bytes
    // fetched for v2 are never published as v3 (SYNC-042).
    request_created(&machine, &mut store, 2026, &[range(0, 64)], 2_000);
    let mut claim = claim_some(&machine, &mut store, 2_100);
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 64)], "stage-1", 2_200)
        .expect("progress");
    let tx = store.write_txn().expect("write");
    tx.update_item_content(
        &doc_id(2026),
        Some(&version("v2")),
        &FileFacts {
            mime_type: None,
            logical_size: Some(64),
            content_version: Some(version("v3")),
        },
        &metadata("m3"),
        2_300,
    )
    .expect("republish");
    tx.commit().expect("commit");
    match machine
        .complete(&mut store, &claim, 2_400)
        .expect("complete")
    {
        CompleteOutcome::Invalidated(invalidation) => {
            assert_eq!(invalidation.category, FailureCategory::VersionConflict);
            assert_eq!(
                invalidation.disposal,
                Some(StagingDisposal {
                    staging: "stage-1".to_owned(),
                })
            );
        }
        other => panic!("expected Invalidated, got {other:?}"),
    }
}

#[test]
fn a_source_reported_version_conflict_invalidates_the_same_way() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    request_created(&machine, &mut store, 2026, &[], 1_000);
    let mut claim = claim_some(&machine, &mut store, 1_100);
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 32)], "stage-1", 1_200)
        .expect("progress");
    let outcome = machine
        .fail(
            &mut store,
            *claim,
            TransferFault::Source(SourceError::VersionConflict {
                current: Some(version("v2")),
                detail: detail(),
            }),
            1_300,
        )
        .expect("fail");
    match outcome {
        FailOutcome::Invalidated(invalidation) => {
            // The projection still says v1, so the source's report is
            // recorded as the conflict it is.
            assert_eq!(invalidation.category, FailureCategory::VersionConflict);
            assert_eq!(
                invalidation.disposal,
                Some(StagingDisposal {
                    staging: "stage-1".to_owned(),
                })
            );
        }
        other => panic!("expected Invalidated, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Failure classification and the retry budget
// ---------------------------------------------------------------------------

#[test]
fn the_retry_budget_bounds_attempts_and_honors_flood_waits() {
    let machine = machine(); // budget 2, base 1s, cap 4s
    let mut store = store_with_docs(&[2026]);

    let id = request_created(&machine, &mut store, 2026, &[], 1_000);

    // First failure: policy backoff.
    let claim = claim_some(&machine, &mut store, 1_100);
    match machine
        .fail(
            &mut store,
            *claim,
            TransferFault::Source(SourceError::Unavailable { detail: detail() }),
            2_000,
        )
        .expect("fail")
    {
        FailOutcome::Requeued {
            category: FailureCategory::Unavailable,
            next_retry_at_ms: 3_000, // 2_000 + base 1_000
            retries_used: 1,
            progress_wiped: false,
            disposal: None,
        } => {}
        other => panic!("expected first Requeued, got {other:?}"),
    }
    // Invisible until the backoff passes.
    match machine.claim(&mut store, 2_500).expect("claim") {
        ClaimOutcome::Empty => {}
        other => panic!("expected Empty during backoff, got {other:?}"),
    }

    // Second failure: a flood wait outranks the policy schedule
    // (SEC-031 — never a tight retry loop against a rate limit).
    let claim = claim_some(&machine, &mut store, 3_000);
    match machine
        .fail(
            &mut store,
            *claim,
            TransferFault::Source(SourceError::RateLimited {
                retry_after: Some(Duration::from_millis(30_000)),
                detail: detail(),
            }),
            4_000,
        )
        .expect("fail")
    {
        FailOutcome::Requeued {
            category: FailureCategory::RateLimited,
            next_retry_at_ms: 34_000, // source minimum, not policy 2_000
            retries_used: 2,
            ..
        } => {}
        other => panic!("expected second Requeued, got {other:?}"),
    }

    // Third failure: the budget of 2 is spent; the transfer ends.
    let claim = claim_some(&machine, &mut store, 40_000);
    match machine
        .fail(
            &mut store,
            *claim,
            TransferFault::Source(SourceError::Unavailable { detail: detail() }),
            41_000,
        )
        .expect("fail")
    {
        FailOutcome::Failed {
            category: FailureCategory::Unavailable,
            disposal: None,
        } => {}
        other => panic!("expected terminal Failed, got {other:?}"),
    }
    let read = store.read_txn().expect("read");
    let row = read.transfer(id).expect("transfer").expect("row exists");
    assert_eq!(row.state, TransferState::Failed);
    assert_eq!(row.retry_count, 3);
}

#[test]
fn unwinnable_faults_finish_terminal_with_their_category() {
    let cases: Vec<(TransferFault, FailureCategory)> = vec![
        (
            TransferFault::Source(SourceError::NotFound { detail: detail() }),
            FailureCategory::NotFound,
        ),
        (
            TransferFault::Source(SourceError::Restricted { detail: detail() }),
            FailureCategory::Restricted,
        ),
        (
            TransferFault::Source(SourceError::InvalidRequest { detail: detail() }),
            FailureCategory::InvalidRequest,
        ),
        (
            TransferFault::Source(SourceError::Internal { detail: detail() }),
            FailureCategory::Internal,
        ),
    ];
    for (fault, expected) in cases {
        let machine = machine();
        let mut store = store_with_docs(&[2026]);
        request_created(&machine, &mut store, 2026, &[], 1_000);
        let mut claim = claim_some(&machine, &mut store, 1_100);
        machine
            .record_progress(&mut store, &mut claim, &[range(0, 8)], "stage-1", 1_200)
            .expect("progress");
        match machine
            .fail(&mut store, *claim, fault.clone(), 1_300)
            .expect("fail")
        {
            FailOutcome::Failed { category, disposal } => {
                assert_eq!(category, expected, "for {fault:?}");
                // Terminal rows claim no staging: the bytes go back to the
                // host for deletion.
                assert_eq!(
                    disposal,
                    Some(StagingDisposal {
                        staging: "stage-1".to_owned(),
                    })
                );
            }
            other => panic!("expected Failed for {fault:?}, got {other:?}"),
        }
    }
}

#[test]
fn preconditioned_faults_park_with_progress_kept() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    let id = request_created(&machine, &mut store, 2026, &[], 1_000);
    let mut claim = claim_some(&machine, &mut store, 1_100);
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 32)], "stage-1", 1_200)
        .expect("progress");
    match machine
        .fail(
            &mut store,
            *claim,
            TransferFault::Source(SourceError::AuthRequired { detail: detail() }),
            1_300,
        )
        .expect("fail")
    {
        FailOutcome::Parked {
            category: FailureCategory::AuthRequired,
        } => {}
        other => panic!("expected Parked, got {other:?}"),
    }
    assert_eq!(transfer_state(&mut store, id), TransferState::Suspended);
    // Parked rows do not poll the queue.
    match machine.claim(&mut store, 1_400).expect("claim") {
        ClaimOutcome::Empty => {}
        other => panic!("expected Empty, got {other:?}"),
    }

    // The precondition changes (reauthorization): resume, re-claim, and
    // the progress parked with the row is still there — no budget burned.
    machine.resume(&mut store, id, 2_000).expect("resume");
    let claim = claim_some(&machine, &mut store, 2_100);
    assert_eq!(claim.remaining(), Remaining::Ranges(vec![range(32, 64)]));
    assert_eq!(claim.record().retry_count, 0);

    // Disk full parks the same way (SYNC-044 local class).
    match machine
        .fail(
            &mut store,
            *claim,
            TransferFault::DiskFull { detail: detail() },
            2_200,
        )
        .expect("fail")
    {
        FailOutcome::Parked {
            category: FailureCategory::DiskFull,
        } => {}
        other => panic!("expected Parked, got {other:?}"),
    }
}

#[test]
fn integrity_failures_discard_staged_bytes_and_refetch() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    request_created(&machine, &mut store, 2026, &[], 1_000);
    let mut claim = claim_some(&machine, &mut store, 1_100);
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 64)], "stage-1", 1_200)
        .expect("progress");
    match machine
        .fail(
            &mut store,
            *claim,
            TransferFault::Integrity { detail: detail() },
            2_000,
        )
        .expect("fail")
    {
        FailOutcome::Requeued {
            category: FailureCategory::Integrity,
            progress_wiped: true,
            disposal: Some(StagingDisposal { staging }),
            ..
        } => assert_eq!(staging, "stage-1"),
        other => panic!("expected wiped Requeued, got {other:?}"),
    }
    // The retry starts from scratch: corrupt staged bytes are gone.
    let claim = claim_some(&machine, &mut store, 4_000);
    assert_eq!(claim.remaining(), Remaining::Ranges(vec![range(0, 64)]));
    assert_eq!(claim.staging(), None);
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[test]
fn two_phase_cancellation_stops_work_and_disposes_staging() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    request_created(&machine, &mut store, 2026, &[], 1_000);
    let mut claim = claim_some(&machine, &mut store, 1_100);
    let id = claim.id();
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 32)], "stage-1", 1_200)
        .expect("progress");
    match machine.checkpoint(&mut store, &claim).expect("checkpoint") {
        Checkpoint::Continue => {}
        other => panic!("expected Continue, got {other:?}"),
    }

    // Phase one, from anywhere; phase two at the claim holder's next
    // boundary.
    assert!(machine.request_cancel(&mut store, id, 2_000).expect("flag"));
    match machine.checkpoint(&mut store, &claim).expect("checkpoint") {
        Checkpoint::CancelRequested => {}
        other => panic!("expected CancelRequested, got {other:?}"),
    }
    let disposal = machine
        .acknowledge_cancel(&mut store, *claim, 2_100)
        .expect("acknowledge");
    assert_eq!(
        disposal,
        Some(StagingDisposal {
            staging: "stage-1".to_owned(),
        })
    );
    let read = store.read_txn().expect("read");
    let row = read.transfer(id).expect("transfer").expect("row exists");
    assert_eq!(row.state, TransferState::Cancelled);
    assert_eq!(row.temp_ref, None);
    assert_eq!(row.completed_ranges, vec![]);
}

#[test]
fn cancel_outranks_drift_and_source_observed_cancels_resolve_against_the_flag() {
    let machine = machine();
    let mut store = store_with_docs(&[2026]);

    // Both signals up: the durable cancel wins at the checkpoint.
    request_created(&machine, &mut store, 2026, &[], 1_000);
    let claim = claim_some(&machine, &mut store, 1_100);
    republish(&mut store, 2026, 1_200);
    assert!(
        machine
            .request_cancel(&mut store, claim.id(), 1_300)
            .expect("flag")
    );
    match machine.checkpoint(&mut store, &claim).expect("checkpoint") {
        Checkpoint::CancelRequested => {}
        other => panic!("expected CancelRequested, got {other:?}"),
    }
    // A source that observed the stop resolves to terminal cancelled.
    match machine
        .fail(
            &mut store,
            *claim,
            TransferFault::Source(SourceError::Cancelled { detail: detail() }),
            1_400,
        )
        .expect("fail")
    {
        FailOutcome::Cancelled { disposal: None } => {}
        other => panic!("expected Cancelled, got {other:?}"),
    }

    // A local stop with no durable request behind it parks instead: the
    // work is not condemned, just not running.
    let fresh = request_created(&machine, &mut store, 2026, &[], 2_000);
    let claim = claim_some(&machine, &mut store, 2_100);
    match machine
        .fail(
            &mut store,
            *claim,
            TransferFault::Source(SourceError::Cancelled { detail: detail() }),
            2_200,
        )
        .expect("fail")
    {
        FailOutcome::Parked {
            category: FailureCategory::Cancelled,
        } => {}
        other => panic!("expected Parked, got {other:?}"),
    }
    assert_eq!(transfer_state(&mut store, fresh), TransferState::Suspended);
}

// ---------------------------------------------------------------------------
// Crash-resume, over a real file
// ---------------------------------------------------------------------------

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
            "gramdrive-engine-test-{}-{n}.sqlite3",
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

/// A host storage inventory for reconciliation: the staging areas that
/// "survived the crash".
#[derive(Debug)]
struct FakeStorage {
    staging: HashMap<String, u64>,
}

impl LocalStorage for FakeStorage {
    fn cache_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        Ok(vec![])
    }

    fn staging_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        Ok(self
            .staging
            .iter()
            .map(|(reference, size)| StoredObject {
                reference: reference.clone(),
                size: *size,
            })
            .collect())
    }

    fn remove_cache_object(&self, _reference: &str) -> Result<(), StorageError> {
        Ok(())
    }

    fn remove_staging_object(&self, _reference: &str) -> Result<(), StorageError> {
        Ok(())
    }
}

#[test]
fn an_interrupted_transfer_resumes_from_persisted_ranges_after_a_crash() {
    let db = TempDb::new();
    let machine = machine();

    // A process claims a transfer, stages half the bytes durably, and dies
    // mid-flight: no suspend, no failure record, the row still `running`.
    let id = {
        let mut store = StateStore::open(&db.path).expect("open");
        seed(&mut store, &[2026]);
        request_created(&machine, &mut store, 2026, &[], 1_000);
        let mut claim = claim_some(&machine, &mut store, 1_100);
        machine
            .record_progress(&mut store, &mut claim, &[range(0, 32)], "stage-1", 1_200)
            .expect("progress");
        claim.id()
        // The store (and the claim token) drop here — the crash.
    };

    // The next process reconciles before any engine work (SYNC-070): the
    // dead claim returns to the queue with its progress intact, and the
    // staging area the journal still owns is not deleted.
    let mut store = StateStore::open(&db.path).expect("reopen");
    let storage = FakeStorage {
        staging: HashMap::from([("stage-1".to_owned(), 32)]),
    };
    let report = store.reconcile(&storage, 2_000).expect("reconcile");
    assert!(report.plan.dirty_shutdown);
    assert!(report.converged());
    assert_eq!(transfer_state(&mut store, id), TransferState::Queued);

    // The interrupted content was never observable as valid: the row is
    // live, its partial ranges intact and unpromoted.
    let read = store.read_txn().expect("read");
    let row = read.transfer(id).expect("transfer").expect("row exists");
    assert!(row.state.is_live());
    assert_eq!(row.completed_ranges, vec![range(0, 32)]);
    assert_eq!(row.temp_ref.as_deref(), Some("stage-1"));
    drop(read);

    // The resumed claim plans exactly the missing suffix; promoting before
    // it is fetched is refused — the gate holds across the crash.
    let mut claim = claim_some(&machine, &mut store, 2_100);
    assert_eq!(claim.id(), id);
    assert_eq!(claim.remaining(), Remaining::Ranges(vec![range(32, 64)]));
    assert_eq!(claim.staging(), Some("stage-1"));
    match machine.complete(&mut store, &claim, 2_200) {
        Err(EngineError::IncompleteContent { missing }) => {
            assert_eq!(missing, vec![range(32, 64)]);
        }
        other => panic!("expected IncompleteContent, got {other:?}"),
    }
    machine
        .record_progress(&mut store, &mut claim, &[range(0, 64)], "stage-1", 2_300)
        .expect("progress");
    match machine
        .complete(&mut store, &claim, 2_400)
        .expect("complete")
    {
        CompleteOutcome::Promoted { staging } => {
            assert_eq!(staging.as_deref(), Some("stage-1"));
        }
        other => panic!("expected Promoted, got {other:?}"),
    }
    assert_eq!(transfer_state(&mut store, id), TransferState::Done);
}
