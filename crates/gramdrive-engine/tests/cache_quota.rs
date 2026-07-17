//! Cache accounting, quota enforcement, LRU eviction, and pinning
//! (TASK-260715-11abx8), driven end-to-end over a durable state store:
//! per-category accounting including partial transfers (SYNC-050), eviction of
//! eligible unpinned-verified content only with pinned/Archive-Mode content
//! preserved (POL-2, SYNC-051/052), the no-race interlocks against open reads
//! and live transfers, content-addressed dedup that keeps a shared object
//! until its last referrer, deterministic drain on a quota shrink with an
//! actionable status (SYNC-054), storage-pressure reclaim, and directional pin
//! orchestration.
//!
//! The `entry` helper seeds exactly what `Promoter::promote` writes — a
//! `verified` cache row with the pin folded on and an on-disk object under its
//! `materialization_ref` — so these fixtures are the promotion output the real
//! pipeline produces, not a parallel invention.

// A panicking test is a failing test (clippy.toml exempts `#[test]` bodies);
// the shared harness below sits at module level in an integration binary that
// links into no product artifact, so the same exemption is restated here — the
// pattern the other engine integration suites use.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use gramdrive_engine::cache::{self, CacheAccounting, EvictionRequest, Evictor};
use gramdrive_engine::model::ByteRange;
use gramdrive_engine::model::identity::{ContentHash, ItemId};
use gramdrive_engine::model::version::{ContentVersion, MetadataVersion};
use gramdrive_engine::state::repo::{
    AccountRecord, CacheEntryRecord, CacheKind, CacheVerification, ChatRecord, ChatType,
    ItemRecord, PinOrigin, RetentionMode, SourceKind,
};
use gramdrive_engine::state::repo::{FileFacts, ItemAvailability};
use gramdrive_engine::state::{LocalStorage, StateStore, StorageError, StoredObject};
use gramdrive_testkit::fixture;

const CHAT: i64 = 100;

// ---------------------------------------------------------------------------
// Store seeding
// ---------------------------------------------------------------------------

fn scope() -> gramdrive_engine::model::identity::AccountScope {
    fixture::scope()
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

fn open_store() -> StateStore {
    StateStore::open_in_memory().expect("open")
}

/// Account, chat, and the account-root item every cache item hangs under.
fn seed_base(store: &mut StateStore) {
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Test Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        retention_mode: RetentionMode::Mirror,
        archive_mode: false,
        secret_ref: None,
        created_at_ms: 1_000,
        updated_at_ms: 1_000,
    })
    .expect("account");
    tx.upsert_chat(&ChatRecord {
        key: gramdrive_engine::model::identity::ChatKey {
            scope: scope(),
            chat_id: gramdrive_engine::model::identity::ChatId(CHAT),
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
        id: fixture::account_root_id(scope()),
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
    tx.commit().expect("commit");
}

/// The provider item a cache entry materializes: an attachment node under the
/// account root. cache_entries only needs the item (its FK target), the
/// account, and — when it carries a blob — the blob row.
fn seed_item(store: &mut StateStore, message: i64) -> ItemId {
    let id = fixture::attachment_id(scope(), CHAT, message, 0);
    let tx = store.write_txn().expect("write");
    tx.upsert_item(&ItemRecord {
        id: id.clone(),
        parent: Some(fixture::account_root_id(scope())),
        display_name: format!("file-{message}.bin"),
        safe_name: format!("file-{message}.bin"),
        metadata_version: metadata("m1"),
        content: Some(FileFacts {
            mime_type: None,
            logical_size: Some(64),
            content_version: Some(version("v1")),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("item");
    tx.commit().expect("commit");
    id
}

/// One materialized cache entry, as promotion would have written it: the blob
/// row (when it carries one), the `cache_entries` row, and the host's on-disk
/// object under its `materialization_ref`.
#[derive(Clone)]
struct Seed {
    message: i64,
    kind: CacheKind,
    size: u64,
    verification: CacheVerification,
    pin: Option<PinOrigin>,
    last_access_at_ms: i64,
    /// Content identity: entries with the same tag share one blob and one
    /// on-disk object (dedup). `None` gives the entry its own by `message`.
    content: Option<u8>,
}

impl Seed {
    fn new(message: i64, size: u64, last_access_at_ms: i64) -> Self {
        Self {
            message,
            kind: CacheKind::Blob,
            size,
            verification: CacheVerification::Verified,
            pin: None,
            last_access_at_ms,
            content: None,
        }
    }

    fn kind(mut self, kind: CacheKind) -> Self {
        self.kind = kind;
        self
    }

    fn pinned(mut self, origin: PinOrigin) -> Self {
        self.pin = Some(origin);
        self
    }

    fn verification(mut self, verification: CacheVerification) -> Self {
        self.verification = verification;
        self
    }

    fn content(mut self, tag: u8) -> Self {
        self.content = Some(tag);
        self
    }
}

fn seed_entry(store: &mut StateStore, host: &MemoryHost, seed: &Seed) -> ItemId {
    let item = seed_item(store, seed.message);
    let content_tag = seed.content.unwrap_or(seed.message as u8);
    let hash = ContentHash::Sha256([content_tag; 32]);
    let reference = format!("cache-object-{content_tag}");

    let tx = store.write_txn().expect("write");
    tx.record_blob(scope().account, &hash, seed.size, 1_000)
        .expect("blob");
    tx.upsert_cache_entry(&CacheEntryRecord {
        item: item.clone(),
        account: scope().account,
        content_version: version("v1"),
        kind: seed.kind,
        size: seed.size,
        blob_hash: Some(hash),
        verification: seed.verification,
        pin: seed.pin,
        last_access_at_ms: seed.last_access_at_ms,
        materialized_at_ms: seed.last_access_at_ms,
        materialization_ref: Some(reference.clone()),
    })
    .expect("cache entry");
    tx.commit().expect("commit");

    host.put_object(&reference, seed.size);
    item
}

/// Enqueues a live transfer for `item` and records `staged` bytes of progress,
/// so `has_live_transfer` and the partial-transfer accounting see it.
fn seed_live_transfer(store: &mut StateStore, item: &ItemId, staged: &[ByteRange]) {
    let tx = store.write_txn().expect("write");
    tx.enqueue_transfer(item, &version("v1"), &[range(0, 64)], 0, 2_000)
        .expect("enqueue");
    tx.commit().expect("commit");
    let claimed = {
        let tx = store.write_txn().expect("write");
        let claimed = tx.claim_next_transfer(2_100).expect("claim").expect("row");
        tx.commit().expect("commit");
        claimed
    };
    let tx = store.write_txn().expect("write");
    tx.record_transfer_progress(claimed.id, staged, Some("stage-live"), 2_200)
        .expect("progress");
    tx.commit().expect("commit");
}

// ---------------------------------------------------------------------------
// The host: an in-memory content-addressed cache store (LocalStorage).
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct MemoryHost {
    objects: Arc<Mutex<HashMap<String, u64>>>,
}

impl std::fmt::Debug for MemoryHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryHost").finish_non_exhaustive()
    }
}

impl MemoryHost {
    fn put_object(&self, reference: &str, size: u64) {
        self.objects
            .lock()
            .expect("lock")
            .insert(reference.to_owned(), size);
    }

    fn object_refs(&self) -> Vec<String> {
        let mut refs: Vec<String> = self.objects.lock().expect("lock").keys().cloned().collect();
        refs.sort();
        refs
    }

    fn has_object(&self, reference: &str) -> bool {
        self.objects.lock().expect("lock").contains_key(reference)
    }
}

impl LocalStorage for MemoryHost {
    fn cache_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        Ok(self
            .objects
            .lock()
            .expect("lock")
            .iter()
            .map(|(reference, size)| StoredObject {
                reference: reference.clone(),
                size: *size,
            })
            .collect())
    }

    fn staging_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        Ok(Vec::new())
    }

    fn remove_cache_object(&self, reference: &str) -> Result<(), StorageError> {
        self.objects.lock().expect("lock").remove(reference);
        Ok(())
    }

    fn remove_staging_object(&self, _reference: &str) -> Result<(), StorageError> {
        Ok(())
    }
}

fn items(store: &mut StateStore, seeds: &[Seed], host: &MemoryHost) -> Vec<ItemId> {
    seeds
        .iter()
        .map(|seed| seed_entry(store, host, seed))
        .collect()
}

// ---------------------------------------------------------------------------
// Accounting (SYNC-050)
// ---------------------------------------------------------------------------

#[test]
fn accounting_separates_categories_and_splits_pins() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();

    // A blob, a generated doc, a thumbnail; one pinned blob, one unverified
    // blob; plus a live transfer holding staged partial bytes.
    items(
        &mut store,
        &[
            Seed::new(1, 100, 1_000),                         // blob, unpinned, verified
            Seed::new(2, 200, 2_000).pinned(PinOrigin::User), // blob, pinned
            Seed::new(3, 50, 3_000).kind(CacheKind::GeneratedDoc), // generated doc
            Seed::new(4, 40, 4_000).kind(CacheKind::Thumbnail), // thumbnail
            Seed::new(5, 400, 5_000).verification(CacheVerification::Unverified), // not evictable
        ],
        &host,
    );
    let live = seed_item(&mut store, 6);
    seed_live_transfer(&mut store, &live, &[range(0, 32)]);

    let evictor = Evictor::default();
    let a: CacheAccounting = evictor.accounting(&mut store).expect("accounting");

    // Categories counted separately (SYNC-050).
    assert_eq!(a.blob_bytes, 100 + 200 + 400);
    assert_eq!(a.generated_doc_bytes, 50);
    assert_eq!(a.thumbnail_bytes, 40);
    assert_eq!(a.partial_transfer_bytes, 32);

    // Totals and the pin/verification splits the quota reads.
    assert_eq!(a.total_cache_bytes, 100 + 200 + 50 + 40 + 400);
    assert_eq!(a.pinned_bytes, 200);
    assert_eq!(a.unpinned_bytes, 100 + 50 + 40 + 400);
    // Evictable excludes the pinned entry and the unverified one.
    assert_eq!(a.evictable_bytes, 100 + 50 + 40);
    // Invariant: total splits exactly into pinned + unpinned, and into
    // the three materialized categories.
    assert_eq!(a.total_cache_bytes, a.pinned_bytes + a.unpinned_bytes);
    assert_eq!(
        a.total_cache_bytes,
        a.blob_bytes + a.generated_doc_bytes + a.thumbnail_bytes
    );
}

// ---------------------------------------------------------------------------
// Eviction: eligible LRU only, pins and Archive Mode preserved (POL-2)
// ---------------------------------------------------------------------------

#[test]
fn eviction_removes_only_unpinned_verified_content_in_lru_order() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();

    let ids = items(
        &mut store,
        &[
            Seed::new(1, 100, 1_000),                              // oldest, evictable
            Seed::new(2, 100, 2_000),                              // next, evictable
            Seed::new(3, 100, 500).pinned(PinOrigin::User),        // pinned: exempt
            Seed::new(4, 100, 300).pinned(PinOrigin::ArchiveMode), // archive: exempt
            Seed::new(6, 100, 9_000),                              // newest, evictable
        ],
        &host,
    );

    // 300 unpinned bytes (items 1,2,6); quota 150 forces reclaiming the oldest
    // verified-unpinned bytes. over_by = 300 - 150 = 150.
    let evictor = Evictor::with_limit(150);
    let report = evictor
        .enforce(&mut store, &host, &EvictionRequest::none())
        .expect("enforce");

    // Evict the two oldest (items 1 then 2) to reclaim 200 >= 150. Item 6
    // (newest) and the pinned/archive rows survive.
    assert_eq!(report.evicted, vec![ids[0].clone(), ids[1].clone()]);
    assert_eq!(report.reclaimed_bytes, 200);
    assert_eq!(report.skipped, Vec::<ItemId>::new());
    assert!(report.assessment.within_quota());

    // Pinned, Archive-Mode, and the newest verified row remain.
    let read = store.read_txn().expect("read");
    for kept in [&ids[2], &ids[3], &ids[4]] {
        assert!(read.cache_entry(kept).expect("entry").is_some());
    }
    assert!(read.cache_entry(&ids[0]).expect("entry").is_none());
    // Their on-disk objects went with them; the survivors' objects stayed.
    assert!(!host.has_object("cache-object-1"));
    assert!(!host.has_object("cache-object-2"));
    assert!(host.has_object("cache-object-3"));
    assert!(host.has_object("cache-object-6"));
}

#[test]
fn unverified_unpinned_bytes_count_toward_the_quota_but_are_never_evicted() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();

    // 100 verified-unpinned + 300 unverified-unpinned = 400 unpinned bytes.
    // Unverified content occupies the budget (SYNC-050) but is never dropped
    // as space (SYNC-052), so a 200-byte quota can only reclaim the 100
    // verified bytes and reports the rest as an honest residual.
    let ids = items(
        &mut store,
        &[
            Seed::new(1, 100, 1_000),
            Seed::new(2, 300, 500).verification(CacheVerification::Unverified),
        ],
        &host,
    );

    let evictor = Evictor::with_limit(200);

    // Before enforcing: over by 200, of which only the 100 verified bytes can
    // be reclaimed — 100 stays locked in the unverified entry.
    let before = evictor
        .assess(&mut store, &EvictionRequest::none())
        .expect("assess");
    assert_eq!(before.over_by, 200);
    assert_eq!(before.reclaimable_bytes, 100);
    assert_eq!(before.residual_bytes, 100);
    assert!(!before.fully_reclaimable());

    let report = evictor
        .enforce(&mut store, &host, &EvictionRequest::none())
        .expect("enforce");

    // The verified entry went; the unverified one is untouched — awaiting
    // hashing, not eviction (SYNC-052). The post-status still shows the
    // residual overage the unverified bytes hold.
    assert_eq!(report.evicted, vec![ids[0].clone()]);
    assert_eq!(report.assessment.over_by, 100);
    assert_eq!(report.assessment.reclaimable_bytes, 0);
    assert_eq!(report.assessment.residual_bytes, 100);
    assert!(!report.assessment.within_quota());
    let read = store.read_txn().expect("read");
    assert!(read.cache_entry(&ids[1]).expect("entry").is_some());
}

#[test]
fn enforce_within_quota_evicts_nothing() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();
    items(
        &mut store,
        &[Seed::new(1, 100, 1_000), Seed::new(2, 100, 2_000)],
        &host,
    );

    let evictor = Evictor::with_limit(10_000);
    let report = evictor
        .enforce(&mut store, &host, &EvictionRequest::none())
        .expect("enforce");
    assert!(report.evicted.is_empty());
    assert_eq!(report.reclaimed_bytes, 0);
    assert!(report.assessment.within_quota());
    assert_eq!(host.object_refs().len(), 2);
}

// ---------------------------------------------------------------------------
// No-race interlocks: open reads and live transfers (SYNC-043/052)
// ---------------------------------------------------------------------------

#[test]
fn eviction_never_races_open_reads_or_live_transfers() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();

    let ids = items(
        &mut store,
        &[
            Seed::new(1, 100, 1_000), // oldest — but protected by an open read
            Seed::new(2, 100, 2_000), // next  — but hydrating (live transfer)
            Seed::new(3, 100, 3_000), // evictable
        ],
        &host,
    );
    // Item 2 has a live transfer at the same content version.
    seed_live_transfer(&mut store, &ids[1], &[range(0, 16)]);

    let evictor = Evictor::with_limit(0); // reclaim everything eligible
    let request = EvictionRequest::protecting([ids[0].clone()]);
    let report = evictor
        .enforce(&mut store, &host, &request)
        .expect("enforce");

    // Only item 3 is free to go; items 1 and 2 are locked.
    assert_eq!(report.evicted, vec![ids[2].clone()]);
    let read = store.read_txn().expect("read");
    assert!(read.cache_entry(&ids[0]).expect("entry").is_some());
    assert!(read.cache_entry(&ids[1]).expect("entry").is_some());
    assert!(read.cache_entry(&ids[2]).expect("entry").is_none());

    // Over quota, but the overage is locked by a read and a transfer — an
    // honest residual, not silent loss (SYNC-054).
    assert!(!report.assessment.within_quota());
    assert!(!report.assessment.fully_reclaimable());
    assert_eq!(report.assessment.residual_bytes, 200);
}

// ---------------------------------------------------------------------------
// Quota change: durable drain + actionable status (SYNC-054)
// ---------------------------------------------------------------------------

#[test]
fn shrinking_the_quota_yields_an_actionable_status_and_drains_deterministically() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();
    items(
        &mut store,
        &[
            Seed::new(1, 100, 1_000),
            Seed::new(2, 100, 2_000),
            Seed::new(3, 100, 3_000),
            Seed::new(4, 100, 4_000),
        ],
        &host,
    );

    // The old quota was comfortable; a shrink to 250 makes it actionable.
    let evictor = Evictor::with_limit(250);
    let before = evictor
        .assess(&mut store, &EvictionRequest::none())
        .expect("assess");
    assert_eq!(before.unpinned_bytes, 400);
    assert_eq!(before.over_by, 150);
    assert_eq!(before.reclaimable_bytes, 150);
    assert_eq!(before.residual_bytes, 0);
    assert!(before.fully_reclaimable());

    // Assessing again is pure — nothing was evicted.
    assert_eq!(
        evictor
            .accounting(&mut store)
            .expect("accounting")
            .unpinned_bytes,
        400
    );

    // Enforcing drains the two oldest (100 + 100 >= 150) and lands within.
    let report = evictor
        .enforce(&mut store, &host, &EvictionRequest::none())
        .expect("enforce");
    assert_eq!(report.evicted.len(), 2);
    assert!(report.assessment.within_quota());
    assert_eq!(
        evictor
            .accounting(&mut store)
            .expect("accounting")
            .unpinned_bytes,
        200
    );
}

// ---------------------------------------------------------------------------
// Content-addressed dedup: shared object survives its non-last referrers
// ---------------------------------------------------------------------------

#[test]
fn a_shared_object_is_deleted_only_when_its_last_referrer_is_evicted() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();

    // Two entries, identical content: one blob row, one on-disk object.
    let ids = items(
        &mut store,
        &[
            Seed::new(1, 100, 1_000).content(42),
            Seed::new(2, 100, 2_000).content(42),
            Seed::new(3, 100, 3_000).content(7), // distinct content
        ],
        &host,
    );
    assert_eq!(
        host.object_refs(),
        vec!["cache-object-42", "cache-object-7"]
    );

    // Evict just enough to drop the oldest shared referrer (item 1). Its
    // object must survive — item 2 still needs it.
    let evictor = Evictor::with_limit(0);
    let request = EvictionRequest::protecting([ids[1].clone(), ids[2].clone()]);
    let report = evictor
        .enforce(&mut store, &host, &request)
        .expect("enforce");
    assert_eq!(report.evicted, vec![ids[0].clone()]);
    assert_eq!(report.objects_deleted, Vec::<String>::new());
    assert_eq!(report.reclaimed_bytes, 0); // no disk freed: object shared
    assert!(host.has_object("cache-object-42"));

    // Now evict the last referrer (item 2). The object goes, once.
    let request = EvictionRequest::protecting([ids[2].clone()]);
    let report = evictor
        .enforce(&mut store, &host, &request)
        .expect("enforce");
    assert_eq!(report.evicted, vec![ids[1].clone()]);
    assert_eq!(report.objects_deleted, vec!["cache-object-42".to_owned()]);
    assert_eq!(report.reclaimed_bytes, 100);
    assert!(!host.has_object("cache-object-42"));
    assert!(host.has_object("cache-object-7"));
}

// ---------------------------------------------------------------------------
// Storage pressure: reclaim a target regardless of the quota
// ---------------------------------------------------------------------------

#[test]
fn reclaim_frees_a_target_even_when_under_quota() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();
    let ids = items(
        &mut store,
        &[
            Seed::new(1, 100, 1_000),
            Seed::new(2, 100, 2_000),
            Seed::new(3, 100, 3_000).pinned(PinOrigin::User),
        ],
        &host,
    );

    // Comfortably within quota, but the disk is full: free at least 150 bytes.
    let evictor = Evictor::with_limit(10_000);
    let report = evictor
        .reclaim(&mut store, &host, 150, &EvictionRequest::none())
        .expect("reclaim");

    // The two oldest unpinned entries go; the pinned one is untouched.
    assert_eq!(report.evicted, vec![ids[0].clone(), ids[1].clone()]);
    assert_eq!(report.reclaimed_bytes, 200);
    let read = store.read_txn().expect("read");
    assert!(read.cache_entry(&ids[2]).expect("entry").is_some());
}

// ---------------------------------------------------------------------------
// Pinning orchestration (POL-2, SYNC-051/062)
// ---------------------------------------------------------------------------

#[test]
fn pinning_folds_onto_the_entry_and_user_intent_wins_over_archive() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();
    let ids = items(&mut store, &[Seed::new(1, 100, 1_000)], &host);
    let item = ids[0].clone();

    // Archive-Mode coverage pins and folds onto the materialized row.
    let out = cache::pin(&mut store, &item, PinOrigin::ArchiveMode, 5_000).expect("pin");
    assert_eq!(out.origin, PinOrigin::ArchiveMode);
    assert!(out.changed);
    assert!(out.folded);
    {
        let read = store.read_txn().expect("read");
        assert_eq!(
            read.cache_entry(&item).expect("entry").expect("some").pin,
            Some(PinOrigin::ArchiveMode)
        );
    }

    // An explicit user pin upgrades the origin.
    let out = cache::pin(&mut store, &item, PinOrigin::User, 6_000).expect("pin");
    assert_eq!(out.origin, PinOrigin::User);
    assert!(out.changed);

    // Archive-Mode coverage must not downgrade the user pin.
    let out = cache::pin(&mut store, &item, PinOrigin::ArchiveMode, 7_000).expect("pin");
    assert_eq!(out.origin, PinOrigin::User);
    assert!(!out.changed);

    // The pinned entry is quota-exempt: enforcing an impossible quota leaves it.
    let evictor = Evictor::with_limit(0);
    let report = evictor
        .enforce(&mut store, &host, &EvictionRequest::none())
        .expect("enforce");
    assert!(report.evicted.is_empty());
    assert_eq!(report.assessment.unpinned_bytes, 0);
}

#[test]
fn unpin_is_directional_archive_teardown_spares_a_user_pin() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();
    let ids = items(
        &mut store,
        &[Seed::new(1, 100, 1_000), Seed::new(2, 100, 2_000)],
        &host,
    );
    let user_item = ids[0].clone();
    let archive_item = ids[1].clone();

    cache::pin(&mut store, &user_item, PinOrigin::User, 5_000).expect("pin");
    cache::pin(&mut store, &archive_item, PinOrigin::ArchiveMode, 5_000).expect("pin");

    // Archive-Mode teardown releases only Archive-Mode coverage.
    assert!(cache::unpin(&mut store, &archive_item, PinOrigin::ArchiveMode).expect("unpin"));
    // The user pin is not archive coverage — teardown leaves it standing.
    assert!(!cache::unpin(&mut store, &user_item, PinOrigin::ArchiveMode).expect("unpin"));

    let read = store.read_txn().expect("read");
    assert!(read.pin(&user_item).expect("pin").is_some());
    assert!(read.pin(&archive_item).expect("pin").is_none());
    // The folded flag was cleared on the released entry only.
    assert_eq!(
        read.cache_entry(&user_item)
            .expect("entry")
            .expect("some")
            .pin,
        Some(PinOrigin::User)
    );
    assert_eq!(
        read.cache_entry(&archive_item)
            .expect("entry")
            .expect("some")
            .pin,
        None
    );
}

// ---------------------------------------------------------------------------
// Bounded frontier: eviction pages past a wall of protected rows
// ---------------------------------------------------------------------------

#[test]
fn eviction_pages_past_many_in_use_rows_to_reach_an_evictable_tail() {
    let mut store = open_store();
    seed_base(&mut store);
    let host = MemoryHost::default();

    // 400 unpinned rows, the oldest 390 all held by open reads, the newest 10
    // free. The walk must page past the protected wall (page size 256) to
    // reach the tail rather than stalling on the first page.
    let mut seeds = Vec::new();
    for message in 0..400 {
        seeds.push(Seed::new(message, 10, 1_000 + message));
    }
    let ids = items(&mut store, &seeds, &host);
    let protected: HashSet<ItemId> = ids[..390].iter().cloned().collect();

    let evictor = Evictor::with_limit(0);
    let request = EvictionRequest {
        protected: protected.clone(),
    };
    let report = evictor
        .enforce(&mut store, &host, &request)
        .expect("enforce");

    // Exactly the 10 unprotected tail rows are evicted, newest-last order.
    assert_eq!(report.evicted.len(), 10);
    let evicted: HashSet<ItemId> = report.evicted.iter().cloned().collect();
    for id in &ids[390..] {
        assert!(evicted.contains(id));
    }
    for id in &ids[..390] {
        assert!(!evicted.contains(id));
    }
}
