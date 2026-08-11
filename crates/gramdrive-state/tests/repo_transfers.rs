//! The transfer journal (TASK-260715-1opnb2; SYNC-040..046): coalescing,
//! the claim/suspend/fail/retry lifecycle, two-phase cancellation, and the
//! SYNC-042 version check at promotion.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use common::{account_record, chat_record};
use gramdrive_state::model::ByteRange;
use gramdrive_state::model::identity::{
    AppearanceKey, ChatListKind, DocFormat, DocPartition, ItemId, ItemKey,
};
use gramdrive_state::model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::repo::{
    EnqueueOutcome, FailureCategory, FileFacts, ItemAvailability, ItemRecord, TransferFailure,
    TransferState,
};
use gramdrive_state::{StateError, StateStore};

const CHAT: i64 = 100;

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

/// A store with the scaffold account, tree, and one hydratable doc item
/// per requested year, at content version "v1".
fn store_with_docs(years: &[u16]) -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&account_record()).expect("account");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    let root = common::account_root_id();
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
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
            aggregate_size: None,
            id: doc_id(*year),
            parent: Some(root.clone()),
            display_name: format!("{year}.ndjson"),
            safe_name: format!("{year}.ndjson"),
            metadata_version: MetadataVersion::new("m1").expect("version"),
            content: Some(FileFacts {
                mime_type: Some("application/x-ndjson".to_owned()),
                logical_size: Some(64),
                content_version: Some(content_version("v1")),
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: None,
            modified_at_ms: None,
            deleted_at_ms: None,
        })
        .expect("doc");
    }
    tx.commit().expect("commit");
    store
}

fn range(start: u64, end: u64) -> ByteRange {
    ByteRange::new(start, end).expect("range")
}

#[test]
fn enqueue_coalesces_live_work_for_the_same_item_and_version() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);

    let tx = store.write_txn().expect("write");
    let first = tx
        .enqueue_transfer(&item, &content_version("v1"), &[range(0, 32)], 5, 1_000)
        .expect("enqueue");
    let EnqueueOutcome::Created(id) = first else {
        panic!("expected Created, got {first:?}");
    };
    // Same (item, version): coalesced (SYNC-046).
    let second = tx
        .enqueue_transfer(&item, &content_version("v1"), &[range(32, 64)], 9, 2_000)
        .expect("enqueue");
    assert_eq!(second, EnqueueOutcome::Coalesced(id));
    // A different version is separate work (SYNC-042).
    let third = tx
        .enqueue_transfer(&item, &content_version("v2"), &[], 0, 3_000)
        .expect("enqueue");
    assert!(matches!(third, EnqueueOutcome::Created(other) if other != id));
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let record = read.transfer(id).expect("transfer").expect("some");
    assert_eq!(record.state, TransferState::Queued);
    assert_eq!(record.requested_ranges, vec![range(0, 32)]);
    assert_eq!(record.completed_ranges, vec![]);
    assert_eq!(record.priority, 5);
    assert_eq!(record.created_at_ms, 1_000);
    let live = read
        .live_transfer_for(&item, &content_version("v1"))
        .expect("live")
        .expect("some");
    assert_eq!(live.id, id);
}

#[test]
fn enqueue_requires_a_projected_item() {
    let mut store = store_with_docs(&[]);
    let tx = store.write_txn().expect("write");
    match tx.enqueue_transfer(&doc_id(2026), &content_version("v1"), &[], 0, 1_000) {
        Err(StateError::RowNotFound { entity: "item" }) => {}
        other => panic!("expected RowNotFound(item), got {other:?}"),
    }
}

#[test]
fn claims_come_in_priority_order_and_respect_backoff_and_cancel() {
    let mut store = store_with_docs(&[2024, 2025, 2026]);
    let tx = store.write_txn().expect("write");
    let low = tx
        .enqueue_transfer(&doc_id(2024), &content_version("v1"), &[], 1, 1_000)
        .expect("enqueue")
        .transfer_id();
    let high = tx
        .enqueue_transfer(&doc_id(2025), &content_version("v1"), &[], 9, 1_000)
        .expect("enqueue")
        .transfer_id();
    let cancelled = tx
        .enqueue_transfer(&doc_id(2026), &content_version("v1"), &[], 20, 1_000)
        .expect("enqueue")
        .transfer_id();
    assert!(
        tx.request_transfer_cancel(cancelled, 1_500)
            .expect("cancel")
    );
    tx.commit().expect("commit");

    let tx = store.write_txn().expect("write");
    // Highest priority first — but never a cancel-requested row.
    let claimed = tx.claim_next_transfer(2_000).expect("claim").expect("some");
    assert_eq!(claimed.id, high);
    assert_eq!(claimed.state, TransferState::Running);
    // Fail with retry backoff: back to the queue, invisible until due.
    tx.mark_transfer_failed(
        high,
        FailureCategory::RateLimited,
        TransferFailure::Retry {
            next_retry_at_ms: 10_000,
        },
        2_100,
    )
    .expect("fail");
    let claimed = tx.claim_next_transfer(2_200).expect("claim").expect("some");
    assert_eq!(claimed.id, low, "backoff hides the failed transfer");
    assert_eq!(tx.claim_next_transfer(2_300).expect("claim"), None);
    // Past the backoff it comes back, with its category on record.
    let claimed = tx
        .claim_next_transfer(10_000)
        .expect("claim")
        .expect("some");
    assert_eq!(claimed.id, high);
    assert_eq!(claimed.retry_count, 1);
    assert_eq!(claimed.failure_category, Some(FailureCategory::RateLimited));
    tx.commit().expect("commit");
}

#[test]
fn lifecycle_suspend_resume_progress_and_terminal_failure() {
    let mut store = store_with_docs(&[2026]);
    let tx = store.write_txn().expect("write");
    let id = tx
        .enqueue_transfer(
            &doc_id(2026),
            &content_version("v1"),
            &[range(0, 64)],
            0,
            1_000,
        )
        .expect("enqueue")
        .transfer_id();

    // Wrong-state transitions are named, and change nothing.
    match tx.suspend_transfer(id, 1_100) {
        Err(StateError::InvalidTransition {
            entity: "transfer",
            from: "queued",
        }) => {}
        other => panic!("expected InvalidTransition, got {other:?}"),
    }
    match tx.mark_transfer_done(id, 1_100) {
        Err(StateError::InvalidTransition { .. }) => {}
        other => panic!("expected InvalidTransition, got {other:?}"),
    }

    let claimed = tx.claim_next_transfer(1_200).expect("claim").expect("some");
    assert_eq!(claimed.id, id);
    tx.record_transfer_progress(id, &[range(0, 32)], Some("stage-1"), 1_300)
        .expect("progress");
    tx.suspend_transfer(id, 1_400).expect("suspend");
    tx.resume_transfer(id, 1_500).expect("resume");
    let claimed = tx.claim_next_transfer(1_600).expect("claim").expect("some");
    assert_eq!(claimed.id, id);
    assert_eq!(claimed.completed_ranges, vec![range(0, 32)]);
    assert_eq!(claimed.temp_ref.as_deref(), Some("stage-1"));

    tx.mark_transfer_failed(
        id,
        FailureCategory::Integrity,
        TransferFailure::Final,
        1_700,
    )
    .expect("fail");
    let record = tx.read().transfer(id).expect("transfer").expect("some");
    assert_eq!(record.state, TransferState::Failed);
    assert_eq!(record.failure_category, Some(FailureCategory::Integrity));
    // Terminal rows accept no further work.
    match tx.record_transfer_progress(id, &[], None, 1_800) {
        Err(StateError::InvalidTransition { .. }) => {}
        other => panic!("expected InvalidTransition, got {other:?}"),
    }
    match tx.mark_transfer_cancelled(id, 1_800) {
        Err(StateError::InvalidTransition { .. }) => {}
        other => panic!("expected InvalidTransition, got {other:?}"),
    }
    // Unknown ids are named too.
    match tx.suspend_transfer(gramdrive_state::repo::TransferId(9_999), 1_900) {
        Err(StateError::RowNotFound { entity: "transfer" }) => {}
        other => panic!("expected RowNotFound(transfer), got {other:?}"),
    }
    tx.commit().expect("commit");
}

#[test]
fn two_phase_cancel_is_observed_at_a_boundary() {
    let mut store = store_with_docs(&[2026]);
    let tx = store.write_txn().expect("write");
    let id = tx
        .enqueue_transfer(&doc_id(2026), &content_version("v1"), &[], 0, 1_000)
        .expect("enqueue")
        .transfer_id();
    let claimed = tx.claim_next_transfer(1_100).expect("claim").expect("some");
    assert!(!claimed.cancel_requested);
    tx.commit().expect("commit");

    // Phase one, from anywhere: raise the durable flag.
    let tx = store.write_txn().expect("write");
    assert!(tx.request_transfer_cancel(id, 2_000).expect("request"));
    tx.commit().expect("commit");

    // Phase two, at the engine's next boundary: observe and acknowledge.
    let tx = store.write_txn().expect("write");
    let record = tx.read().transfer(id).expect("transfer").expect("some");
    assert!(record.cancel_requested);
    assert_eq!(record.state, TransferState::Running);
    tx.mark_transfer_cancelled(id, 2_100).expect("cancelled");
    let record = tx.read().transfer(id).expect("transfer").expect("some");
    assert_eq!(record.state, TransferState::Cancelled);
    assert_eq!(record.failure_category, Some(FailureCategory::Cancelled));
    // Requesting cancel of finished work is a no-op, not an error.
    assert!(!tx.request_transfer_cancel(id, 2_200).expect("request"));
    tx.commit().expect("commit");
}

#[test]
fn promotion_rechecks_the_content_version_it_pinned() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    let tx = store.write_txn().expect("write");
    let id = tx
        .enqueue_transfer(&item, &content_version("v1"), &[], 0, 1_000)
        .expect("enqueue")
        .transfer_id();
    tx.claim_next_transfer(1_100).expect("claim").expect("some");

    // The source republished while we were fetching: the item moves to v2.
    tx.update_item_content(
        &item,
        Some(&content_version("v1")),
        &FileFacts {
            mime_type: Some("application/x-ndjson".to_owned()),
            logical_size: Some(128),
            content_version: Some(content_version("v2")),
        },
        &MetadataVersion::new("m2").expect("version"),
        1_200,
    )
    .expect("republish");

    // Bytes fetched for v1 must not be published as v2 (SYNC-042).
    match tx.mark_transfer_done(id, 1_300) {
        Err(StateError::VersionConflict {
            entity: "transfer content",
            expected: Some(expected),
            found: Some(found),
        }) => {
            assert_eq!(expected, "v1");
            assert_eq!(found, "v2");
        }
        other => panic!("expected VersionConflict, got {other:?}"),
    }
    // The journal records the conflict; the transfer is still live and the
    // caller decides (typically: final failure plus a fresh enqueue).
    tx.mark_transfer_failed(
        id,
        FailureCategory::VersionConflict,
        TransferFailure::Final,
        1_400,
    )
    .expect("fail");
    let fresh = tx
        .enqueue_transfer(&item, &content_version("v2"), &[], 0, 1_500)
        .expect("enqueue")
        .transfer_id();
    tx.claim_next_transfer(1_600).expect("claim").expect("some");
    tx.mark_transfer_done(fresh, 1_700).expect("done");
    let record = tx.read().transfer(fresh).expect("transfer").expect("some");
    assert_eq!(record.state, TransferState::Done);
    assert_eq!(record.failure_category, None);
    tx.commit().expect("commit");
}

#[test]
fn has_live_transfer_reflects_only_live_states() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);

    let tx = store.write_txn().expect("write");
    // No transfer yet.
    assert!(!tx.read().has_live_transfer(&item).expect("live"));
    // Queued is live.
    let id = tx
        .enqueue_transfer(&item, &content_version("v1"), &[range(0, 64)], 0, 1_000)
        .expect("enqueue")
        .transfer_id();
    assert!(tx.read().has_live_transfer(&item).expect("live"));
    // Running is live.
    tx.claim_next_transfer(1_100).expect("claim").expect("some");
    assert!(tx.read().has_live_transfer(&item).expect("live"));
    // A terminal transfer is not live — eviction may proceed.
    tx.record_transfer_progress(id, &[range(0, 64)], Some("stage-1"), 1_200)
        .expect("progress");
    tx.mark_transfer_done(id, 1_300).expect("done");
    assert!(!tx.read().has_live_transfer(&item).expect("live"));
    tx.commit().expect("commit");
}

#[test]
fn staged_transfer_bytes_sums_live_completed_ranges_only() {
    let mut store = store_with_docs(&[2024, 2025, 2026]);
    let tx = store.write_txn().expect("write");
    // Nothing staged yet.
    assert_eq!(tx.read().staged_transfer_bytes().expect("staged"), 0);

    // A running transfer with 48 staged bytes across two ranges.
    let live = tx
        .enqueue_transfer(
            &doc_id(2024),
            &content_version("v1"),
            &[range(0, 64)],
            0,
            1_000,
        )
        .expect("enqueue")
        .transfer_id();
    tx.claim_next_transfer(1_100).expect("claim").expect("some");
    tx.record_transfer_progress(live, &[range(0, 32), range(40, 56)], Some("stage-a"), 1_200)
        .expect("progress");

    // A suspended transfer with 10 staged bytes still counts (still live).
    let suspended = tx
        .enqueue_transfer(
            &doc_id(2025),
            &content_version("v1"),
            &[range(0, 64)],
            0,
            1_300,
        )
        .expect("enqueue")
        .transfer_id();
    tx.claim_next_transfer(1_400).expect("claim").expect("some");
    tx.record_transfer_progress(suspended, &[range(0, 10)], Some("stage-b"), 1_500)
        .expect("progress");
    tx.suspend_transfer(suspended, 1_600).expect("suspend");

    // A terminal transfer's staged bytes are not counted — its staging is
    // disposed, not cache.
    let done = tx
        .enqueue_transfer(
            &doc_id(2026),
            &content_version("v1"),
            &[range(0, 64)],
            0,
            1_700,
        )
        .expect("enqueue")
        .transfer_id();
    tx.claim_next_transfer(1_800).expect("claim").expect("some");
    tx.record_transfer_progress(done, &[range(0, 64)], Some("stage-c"), 1_900)
        .expect("progress");
    tx.mark_transfer_done(done, 2_000).expect("done");

    // 48 (running) + 10 (suspended); the done transfer's 64 excluded.
    assert_eq!(tx.read().staged_transfer_bytes().expect("staged"), 58);
    tx.commit().expect("commit");
}
