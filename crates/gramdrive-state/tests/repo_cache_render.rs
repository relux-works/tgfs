//! Cache state, pins, and render watermarks (TASK-260715-1opnb2; POL-2,
//! SYNC-050..052, SYNC-024): eviction eligibility lives in the delete
//! itself, accounting comes from the covering index, and publication
//! closes the render/append race through the watermark.

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use common::{account_record, chat_record, revision, scope};
use gramdrive_state::model::identity::{
    AppearanceKey, ChatListKind, ContentHash, DocFormat, DocPartition, ItemId, ItemKey,
};
use gramdrive_state::model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::repo::{
    CacheEntryRecord, CacheKind, CacheTotals, CacheVerification, FileFacts, ItemAvailability,
    ItemRecord, MessageChange, PinOrigin, RenderOutput,
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

/// Account, chat, root, and one file item per year.
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
            content: Some(FileFacts::default()),
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

fn entry(item: &ItemId, size: u64, last_access_at_ms: i64) -> CacheEntryRecord {
    CacheEntryRecord {
        item: item.clone(),
        account: scope().account,
        content_version: content_version("v1"),
        kind: CacheKind::GeneratedDoc,
        size,
        blob_hash: None,
        verification: CacheVerification::Verified,
        pin: None,
        last_access_at_ms,
        materialized_at_ms: 1_000,
        materialization_ref: None,
    }
}

#[test]
fn cache_entries_round_trip_including_blob_links() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    let hash = ContentHash::Sha256([9u8; 32]);

    let tx = store.write_txn().expect("write");
    tx.record_blob(scope().account, &hash, 64, 1_000)
        .expect("blob");
    let mut record = entry(&item, 64, 2_000);
    record.kind = CacheKind::Blob;
    record.blob_hash = Some(hash);
    record.pin = Some(PinOrigin::User);
    record.materialization_ref = Some("bookmark-1".to_owned());
    tx.upsert_cache_entry(&record).expect("entry");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    assert_eq!(read.cache_entry(&item).expect("entry"), Some(record));
    assert_eq!(read.cache_entry(&doc_id(2026 + 1)).expect("entry"), None);
}

#[test]
fn eviction_scans_and_removes_only_eligible_entries() {
    let mut store = store_with_docs(&[2024, 2025, 2026, 2027]);
    let old = doc_id(2024);
    let newer = doc_id(2025);
    let pinned = doc_id(2026);
    let unverified = doc_id(2027);

    let tx = store.write_txn().expect("write");
    let mut old_record = entry(&old, 100, 1_000);
    old_record.kind = CacheKind::Thumbnail;
    tx.upsert_cache_entry(&old_record).expect("entry");
    let mut newer_record = entry(&newer, 200, 5_000);
    newer_record.kind = CacheKind::Thumbnail;
    tx.upsert_cache_entry(&newer_record).expect("entry");
    let mut record = entry(&pinned, 300, 500);
    record.pin = Some(PinOrigin::ArchiveMode);
    tx.upsert_cache_entry(&record).expect("entry");
    let mut record = entry(&unverified, 400, 100);
    record.verification = CacheVerification::Unverified;
    tx.upsert_cache_entry(&record).expect("entry");
    tx.commit().expect("commit");

    // Only eligible rows, oldest access first (SYNC-051/052).
    let read = store.read_txn().expect("read");
    let candidates = read.eviction_candidates(10).expect("candidates");
    assert_eq!(
        candidates
            .iter()
            .map(|c| c.item.clone())
            .collect::<Vec<_>>(),
        vec![old.clone(), newer.clone()]
    );
    assert_eq!(candidates[0].size, 100);
    drop(read);

    let tx = store.write_txn().expect("write");
    // The delete itself refuses ineligible rows, whatever the caller
    // believes (SYNC-051).
    assert!(!tx.evict_cache_entry(&pinned).expect("evict"));
    assert!(!tx.evict_cache_entry(&unverified).expect("evict"));
    assert!(tx.evict_cache_entry(&old).expect("evict"));
    assert!(!tx.evict_cache_entry(&old).expect("evict again"));
    // Unconditional removal is a different, deliberate operation.
    assert!(tx.remove_cache_entry(&unverified).expect("remove"));
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    assert!(read.cache_entry(&pinned).expect("entry").is_some());
    assert!(read.cache_entry(&old).expect("entry").is_none());
    assert!(read.cache_entry(&unverified).expect("entry").is_none());
}

#[test]
fn accounting_touch_and_state_updates() {
    let mut store = store_with_docs(&[2024, 2025]);
    let doc = doc_id(2024);
    let thumb = doc_id(2025);

    let tx = store.write_txn().expect("write");
    tx.upsert_cache_entry(&entry(&doc, 100, 1_000))
        .expect("entry");
    let mut record = entry(&thumb, 40, 1_000);
    record.kind = CacheKind::Thumbnail;
    tx.upsert_cache_entry(&record).expect("entry");

    assert!(tx.touch_cache_entry(&doc, 9_000).expect("touch"));
    assert!(
        !tx.touch_cache_entry(&doc_id(1999), 9_000)
            .expect("touch missing")
    );
    tx.set_cache_verification(&thumb, CacheVerification::Corrupt)
        .expect("verification");
    tx.set_cache_pin(&doc, Some(PinOrigin::User)).expect("pin");
    match tx.set_cache_pin(&doc_id(1999), None) {
        Err(StateError::RowNotFound {
            entity: "cache entry",
        }) => {}
        other => panic!("expected RowNotFound, got {other:?}"),
    }
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let stored = read.cache_entry(&doc).expect("entry").expect("some");
    assert_eq!(stored.last_access_at_ms, 9_000);
    assert_eq!(stored.pin, Some(PinOrigin::User));
    assert_eq!(
        read.cache_entry(&thumb)
            .expect("entry")
            .expect("some")
            .verification,
        CacheVerification::Corrupt
    );
    // Accounting sums by category (SYNC-050).
    let usage = read.cache_usage(scope().account).expect("usage");
    let by_kind: Vec<(CacheKind, u64)> = usage.iter().map(|u| (u.kind, u.total_bytes)).collect();
    assert!(by_kind.contains(&(CacheKind::GeneratedDoc, 100)));
    assert!(by_kind.contains(&(CacheKind::Thumbnail, 40)));
}

#[test]
fn device_wide_totals_split_pins_and_verification() {
    let mut store = store_with_docs(&[2024, 2025, 2026, 2027]);
    let unpinned = doc_id(2024);
    let pinned = doc_id(2025);
    let unverified = doc_id(2026);
    let thumb = doc_id(2027);

    let tx = store.write_txn().expect("write");
    tx.upsert_cache_entry(&entry(&unpinned, 100, 1_000))
        .expect("entry");
    let mut record = entry(&pinned, 200, 2_000);
    record.pin = Some(PinOrigin::User);
    tx.upsert_cache_entry(&record).expect("entry");
    let mut record = entry(&unverified, 400, 3_000);
    record.verification = CacheVerification::Unverified;
    tx.upsert_cache_entry(&record).expect("entry");
    let mut record = entry(&thumb, 40, 4_000);
    record.kind = CacheKind::Thumbnail;
    tx.upsert_cache_entry(&record).expect("entry");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    // Device-wide totals split the facts the quota engine reads (SYNC-050/054).
    assert_eq!(
        read.cache_totals().expect("totals"),
        CacheTotals {
            total_bytes: 100 + 200 + 400 + 40,
            pinned_bytes: 200,
            // Generated documents are visible in total/by-kind accounting but
            // exempt from the quota-governed unpinned and evictable totals.
            unpinned_bytes: 40,
            evictable_bytes: 40,
        }
    );

    // Global usage by kind sums across accounts (SYNC-050).
    let by_kind: Vec<(CacheKind, u64)> = read
        .cache_usage_by_kind()
        .expect("usage")
        .iter()
        .map(|u| (u.kind, u.total_bytes))
        .collect();
    assert!(by_kind.contains(&(CacheKind::GeneratedDoc, 100 + 200 + 400)));
    assert!(by_kind.contains(&(CacheKind::Thumbnail, 40)));

    // An empty cache totals to zero, never an error.
    let mut empty = StateStore::open_in_memory().expect("open");
    let read = empty.read_txn().expect("read");
    assert_eq!(read.cache_totals().expect("totals"), CacheTotals::default());
}

#[test]
fn eviction_candidates_page_by_keyset_cursor() {
    let mut store = store_with_docs(&[2024, 2025, 2026, 2027]);
    // Distinct access times so the LRU order is unambiguous.
    let years = [2024, 2025, 2026, 2027];
    let tx = store.write_txn().expect("write");
    for (offset, year) in years.iter().enumerate() {
        let access = 1_000 + offset as i64;
        let mut record = entry(&doc_id(*year), 10, access);
        record.kind = CacheKind::Thumbnail;
        tx.upsert_cache_entry(&record).expect("entry");
    }
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    // First page of two, oldest first.
    let first = read.eviction_candidates_after(None, 2).expect("page");
    assert_eq!(
        first.iter().map(|c| c.item.clone()).collect::<Vec<_>>(),
        vec![doc_id(2024), doc_id(2025)]
    );
    // The next page continues strictly past the cursor.
    let cursor = first.last().expect("last");
    let second = read
        .eviction_candidates_after(Some((cursor.last_access_at_ms, &cursor.item)), 2)
        .expect("page");
    assert_eq!(
        second.iter().map(|c| c.item.clone()).collect::<Vec<_>>(),
        vec![doc_id(2026), doc_id(2027)]
    );
    // Past the end is empty.
    let last = second.last().expect("last");
    assert!(
        read.eviction_candidates_after(Some((last.last_access_at_ms, &last.item)), 2)
            .expect("page")
            .is_empty()
    );
}

#[test]
fn materialization_ref_reference_tracks_shared_objects() {
    let mut store = store_with_docs(&[2024, 2025]);
    let a = doc_id(2024);
    let b = doc_id(2025);
    // Two entries naming one content-addressed object (dedup, SYNC-052).
    let tx = store.write_txn().expect("write");
    let mut record = entry(&a, 100, 1_000);
    record.materialization_ref = Some("object-shared".to_owned());
    tx.upsert_cache_entry(&record).expect("entry");
    let mut record = entry(&b, 100, 2_000);
    record.materialization_ref = Some("object-shared".to_owned());
    tx.upsert_cache_entry(&record).expect("entry");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    assert!(
        read.materialization_ref_referenced("object-shared")
            .expect("ref")
    );
    assert!(
        !read
            .materialization_ref_referenced("object-absent")
            .expect("ref")
    );
    drop(read);

    // Evicting one referrer leaves the object still referenced by the other.
    let tx = store.write_txn().expect("write");
    assert!(tx.evict_cache_entry(&a).expect("evict"));
    tx.commit().expect("commit");
    let read = store.read_txn().expect("read");
    assert!(
        read.materialization_ref_referenced("object-shared")
            .expect("ref")
    );
    drop(read);

    // Evicting the last referrer orphans the object.
    let tx = store.write_txn().expect("write");
    assert!(tx.evict_cache_entry(&b).expect("evict"));
    tx.commit().expect("commit");
    let read = store.read_txn().expect("read");
    assert!(
        !read
            .materialization_ref_referenced("object-shared")
            .expect("ref")
    );
}

#[test]
fn pins_are_durable_intent_with_origin_semantics() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);

    let tx = store.write_txn().expect("write");
    tx.pin_item(&item, PinOrigin::ArchiveMode, 1_000)
        .expect("pin");
    // A user pin over Archive-Mode coverage upgrades the origin and keeps
    // the original time (POL-2).
    tx.pin_item(&item, PinOrigin::User, 5_000).expect("re-pin");
    tx.commit().expect("commit");

    let read = store.read_txn().expect("read");
    let pin = read.pin(&item).expect("pin").expect("some");
    assert_eq!(pin.origin, PinOrigin::User);
    assert_eq!(pin.created_at_ms, 1_000);
    assert_eq!(read.pins(None).expect("pins").len(), 1);
    assert_eq!(read.pins(Some(PinOrigin::User)).expect("pins").len(), 1);
    assert!(
        read.pins(Some(PinOrigin::ArchiveMode))
            .expect("pins")
            .is_empty()
    );
    drop(read);

    let tx = store.write_txn().expect("write");
    assert!(tx.unpin_item(&item).expect("unpin"));
    assert!(!tx.unpin_item(&item).expect("unpin again"));
    tx.commit().expect("commit");
}

#[test]
fn render_state_tracks_versions_watermarks_and_the_dirty_worklist() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    let chat = common::chat_key(CHAT);

    let tx = store.write_txn().expect("write");
    tx.ensure_render_state(&item, 1, 1).expect("ensure");
    let state = tx.read().render_state(&item).expect("state").expect("some");
    assert!(state.dirty, "a new document starts dirty");
    assert_eq!(state.input_watermark_seq, 0);
    assert_eq!(
        tx.read().dirty_render_items(10).expect("dirty"),
        vec![item.clone()]
    );

    // Events arrive; the renderer reads its inputs at the watermark.
    let changes: Vec<MessageChange> = (1..=3)
        .map(|m| MessageChange::Observed(revision(m, 1_000 * m)))
        .collect();
    tx.apply_message_changes(&chat, &changes).expect("apply");
    let watermark = tx.read().latest_event_seq(&chat).expect("seq");

    // Publication at the current watermark leaves the document clean.
    let publish = tx
        .publish_render(
            &item,
            &chat,
            watermark,
            &RenderOutput {
                content_version: content_version("r1-w3"),
                content_hash: Some(ContentHash::Sha256([3u8; 32])),
                logical_size: 128,
            },
            5_000,
        )
        .expect("publish");
    assert!(publish.clean);
    let state = tx.read().render_state(&item).expect("state").expect("some");
    assert!(!state.dirty);
    assert_eq!(state.input_watermark_seq, watermark);
    assert_eq!(
        state.content_version.as_ref().map(ContentVersion::as_str),
        Some("r1-w3")
    );
    assert_eq!(state.logical_size, Some(128));
    assert_eq!(state.rendered_at_ms, Some(5_000));
    assert!(tx.read().dirty_render_items(10).expect("dirty").is_empty());

    // Same renderer versions: ensure is a no-op. New versions: dirty again
    // (SYNC-030), published facts kept until the re-render.
    tx.ensure_render_state(&item, 1, 1).expect("ensure");
    assert!(
        !tx.read()
            .render_state(&item)
            .expect("state")
            .expect("some")
            .dirty
    );
    tx.ensure_render_state(&item, 2, 1).expect("ensure");
    let state = tx.read().render_state(&item).expect("state").expect("some");
    assert!(state.dirty);
    assert_eq!(state.renderer_version, 2);
    assert_eq!(
        state.content_version.as_ref().map(ContentVersion::as_str),
        Some("r1-w3")
    );
    tx.tombstone_item(
        &item,
        6_000,
        &MetadataVersion::new("m2").expect("tombstone version"),
    )
    .expect("tombstone");
    assert!(
        tx.read().dirty_render_items(10).expect("dirty").is_empty(),
        "tombstoned documents must not consume the bounded live worklist"
    );
    tx.commit().expect("commit");
}

#[test]
fn dirty_render_worklist_prefers_never_published_over_recently_redirtied() {
    let mut store = store_with_docs(&[2024, 2025]);
    let repeatedly_dirty = doc_id(2024);
    let never_published = doc_id(2025);
    let chat = common::chat_key(CHAT);
    let tx = store.write_txn().expect("write");
    tx.ensure_render_state(&repeatedly_dirty, 1, 1)
        .expect("first render state");
    tx.ensure_render_state(&never_published, 1, 1)
        .expect("second render state");
    tx.apply_message_changes(&chat, &[MessageChange::Observed(revision(1, 1_000))])
        .expect("event");
    let watermark = tx.read().latest_event_seq(&chat).expect("watermark");
    tx.publish_render(
        &repeatedly_dirty,
        &chat,
        watermark,
        &RenderOutput {
            content_version: content_version("published-v1"),
            content_hash: Some(ContentHash::Sha256([4u8; 32])),
            logical_size: 64,
        },
        2_000,
    )
    .expect("publish lower-sorting document");
    tx.ensure_render_state(&repeatedly_dirty, 2, 1)
        .expect("redirty after upgrade");

    assert_eq!(
        tx.read().dirty_render_items(1).expect("fair worklist"),
        vec![never_published],
        "a recently refreshed low-sorting chat must rotate behind untouched work"
    );
    tx.commit().expect("commit");
}

#[test]
fn policy_skipped_render_state_is_durable_countable_and_requeueable() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    let tx = store.write_txn().expect("write");
    tx.ensure_render_state(&item, 1, 1).expect("ensure");

    assert!(
        tx.skip_render_due_to_policy(&item, 7_000)
            .expect("record policy skip")
    );
    let state = tx.read().render_state(&item).expect("state").expect("row");
    assert!(!state.dirty, "a policy skip cannot consume the worklist");
    assert_eq!(
        state.skip_reason,
        Some(gramdrive_state::repo::RenderSkipReason::PolicyExcluded)
    );
    assert_eq!(state.skipped_at_ms, Some(7_000));
    assert!(
        tx.read()
            .dirty_render_items(10)
            .expect("worklist")
            .is_empty(),
        "the durable skip, not an in-memory cursor, removes the row"
    );

    tx.mark_render_dirty(&item).expect("explicit requeue");
    let state = tx.read().render_state(&item).expect("state").expect("row");
    assert!(state.dirty);
    assert_eq!(state.skip_reason, None);
    assert_eq!(state.skipped_at_ms, None);
    tx.commit().expect("commit");
}

#[test]
fn publication_never_regresses_and_never_hides_late_events() {
    let mut store = store_with_docs(&[2026]);
    let item = doc_id(2026);
    let chat = common::chat_key(CHAT);

    let tx = store.write_txn().expect("write");
    tx.ensure_render_state(&item, 1, 1).expect("ensure");
    tx.apply_message_changes(
        &chat,
        &[
            MessageChange::Observed(revision(1, 1_000)),
            MessageChange::Observed(revision(2, 2_000)),
        ],
    )
    .expect("apply");
    let watermark = tx.read().latest_event_seq(&chat).expect("seq");
    tx.publish_render(
        &item,
        &chat,
        watermark,
        &RenderOutput {
            content_version: content_version("r1-w2"),
            content_hash: None,
            logical_size: 64,
        },
        3_000,
    )
    .expect("publish");

    // A publication claiming to reflect *less* than the published bytes is
    // refused whole.
    match tx.publish_render(
        &item,
        &chat,
        watermark - 1,
        &RenderOutput {
            content_version: content_version("r1-w1"),
            content_hash: None,
            logical_size: 32,
        },
        3_100,
    ) {
        Err(StateError::WatermarkRegression { current, proposed }) => {
            assert_eq!(current, watermark);
            assert_eq!(proposed, watermark - 1);
        }
        other => panic!("expected WatermarkRegression, got {other:?}"),
    }

    // The race: a renderer read its inputs at `watermark`, and events
    // arrived before it published. The publication lands, but the document
    // stays on the worklist — published bytes never hide events they
    // predate (SYNC-024).
    tx.apply_message_changes(&chat, &[MessageChange::Observed(revision(3, 3_000))])
        .expect("late event");
    let publish = tx
        .publish_render(
            &item,
            &chat,
            watermark,
            &RenderOutput {
                content_version: content_version("r1-w2b"),
                content_hash: None,
                logical_size: 64,
            },
            3_200,
        )
        .expect("publish");
    assert!(!publish.clean);
    let state = tx.read().render_state(&item).expect("state").expect("some");
    assert!(state.dirty, "late events keep the document on the worklist");
    assert_eq!(state.input_watermark_seq, watermark);

    // Explicit dirtying and the typed missing-row answer.
    match tx.mark_render_dirty(&doc_id(1999)) {
        Err(StateError::RowNotFound {
            entity: "render state",
        }) => {}
        other => panic!("expected RowNotFound, got {other:?}"),
    }
    match tx.publish_render(
        &doc_id(1999),
        &chat,
        1,
        &RenderOutput {
            content_version: content_version("r1"),
            content_hash: None,
            logical_size: 1,
        },
        1_000,
    ) {
        Err(StateError::RowNotFound {
            entity: "render state",
        }) => {}
        other => panic!("expected RowNotFound, got {other:?}"),
    }
    tx.commit().expect("commit");
}
