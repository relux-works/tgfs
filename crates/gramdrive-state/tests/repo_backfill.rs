//! Durable backfill-scheduler control state (TASK-260715-mua1ng; POL-2,
//! POL-8, NFR-033): the per-scope pause switch, request spacer, and
//! honored flood-wait deadline survive a store round-trip and default to
//! absent for a never-paced account.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use common::{account_record, scope};
use gramdrive_state::StateStore;
use gramdrive_state::repo::BackfillControlRecord;

fn store() -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    let tx = store.write_txn().expect("write txn");
    tx.upsert_account(&account_record()).expect("account");
    tx.commit().expect("commit");
    store
}

#[test]
fn absent_control_reads_back_as_none() {
    let mut store = store();
    let tx = store.read_txn().expect("read txn");
    let control = tx.backfill_control(scope()).expect("read control");
    assert_eq!(control, None, "a never-paced scope has no control row");
}

#[test]
fn control_round_trips_all_fields() {
    let mut store = store();
    let record = BackfillControlRecord {
        paused: true,
        next_request_at_ms: Some(5_000),
        flood_wait_until_ms: Some(305_000),
        updated_at_ms: 4_000,
    };

    let tx = store.write_txn().expect("write");
    tx.put_backfill_control(scope(), &record).expect("put");
    tx.commit().expect("commit");

    let mut store = store; // rebind for the read borrow
    let tx = store.read_txn().expect("read");
    let read = tx.backfill_control(scope()).expect("read control");
    assert_eq!(read, Some(record));
}

#[test]
fn put_upserts_the_single_scope_row() {
    let mut store = store();

    let first = BackfillControlRecord {
        paused: true,
        next_request_at_ms: Some(1_000),
        flood_wait_until_ms: None,
        updated_at_ms: 1_000,
    };
    let tx = store.write_txn().expect("write");
    tx.put_backfill_control(scope(), &first).expect("put first");
    tx.commit().expect("commit");

    // A later write replaces the row rather than inserting a second: the
    // scope key is unique, so resuming clears the earlier pause.
    let second = BackfillControlRecord {
        paused: false,
        next_request_at_ms: Some(2_000),
        flood_wait_until_ms: Some(9_000),
        updated_at_ms: 2_000,
    };
    let tx = store.write_txn().expect("write");
    tx.put_backfill_control(scope(), &second)
        .expect("put second");
    tx.commit().expect("commit");

    let tx = store.read_txn().expect("read");
    assert_eq!(
        tx.backfill_control(scope()).expect("read control"),
        Some(second),
    );
}

#[test]
fn fresh_is_unpaused_with_no_deadlines() {
    let fresh = BackfillControlRecord::fresh(7);
    assert!(!fresh.paused);
    assert_eq!(fresh.next_request_at_ms, None);
    assert_eq!(fresh.flood_wait_until_ms, None);
    assert_eq!(fresh.updated_at_ms, 7);
}
