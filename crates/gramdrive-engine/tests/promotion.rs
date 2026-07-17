//! Integrity verification and atomic cache promotion (TASK-260715-3s6cpe),
//! driven end-to-end over the transfer machine and a durable state store:
//! whole-content hashing, content-addressed dedup with per-attachment
//! provenance, fail-closed verification of truncated/unreadable/version-drift
//! content, idempotent re-invocation, file-before-row ordering, and the
//! reconciliation backstops that make an interrupted promotion converge
//! (SYNC-042, SYNC-050..053).

// A panicking test is a failing test (clippy.toml exempts `#[test]` bodies);
// the shared harness below sits at module level in an integration binary that
// links into no product artifact, so the same exemption is restated here — the
// pattern the other engine integration suites use.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};

use gramdrive_engine::cache::{
    Materialization, Promoter, Promotion, PromotionConfig, PromotionHost, PromotionHostError,
};
use gramdrive_engine::fetch::{Staging, StagingError, StagingHost};
use gramdrive_engine::model::ByteRange;
use gramdrive_engine::model::hash::sha256;
use gramdrive_engine::model::identity::{
    AccountScope, AttachmentIndex, AttachmentKey, ChatId, ChatKey, ContentHash, ItemId, MessageId,
    MessageKey, SchemaFamily,
};
use gramdrive_engine::model::version::{ContentVersion, MetadataVersion};
use gramdrive_engine::state::repo::{
    AccountRecord, AttachmentAvailability, AttachmentFacts, CacheVerification, ChatRecord,
    ChatType, FailureCategory, FileFacts, ItemAvailability, ItemRecord, MessageChange,
    MessageRevision, PinOrigin, RetentionMode, SourceKind, TransferId,
};
use gramdrive_engine::state::{LocalStorage, StateStore, StorageError, StoredObject};
use gramdrive_engine::transfer::{
    ClaimOutcome, CompleteOutcome, EngineError, Priority, RequestOutcome, RetryPolicy,
    TransferMachine,
};
use gramdrive_testkit::fixture;

const CHAT: i64 = 100;

// ---------------------------------------------------------------------------
// Identities and content
// ---------------------------------------------------------------------------

fn scope() -> AccountScope {
    fixture::scope()
}

fn chat_key() -> ChatKey {
    ChatKey {
        scope: scope(),
        chat_id: ChatId(CHAT),
    }
}

fn attachment_key(message: i64, index: u32) -> AttachmentKey {
    AttachmentKey {
        message: MessageKey {
            chat: chat_key(),
            message_id: MessageId(message),
        },
        index: AttachmentIndex(index),
    }
}

fn attachment_item(message: i64, index: u32) -> ItemId {
    fixture::attachment_id(scope(), CHAT, message, index)
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

/// Deterministic content: `len` bytes seeded from `seed` so distinct
/// attachments can be given either identical or distinct bytes on purpose.
fn content(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

// ---------------------------------------------------------------------------
// Store seeding
// ---------------------------------------------------------------------------

fn open_store() -> StateStore {
    StateStore::open_in_memory().expect("open")
}

/// Account, chat, and the account-root item every attachment hangs under.
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

/// One fetchable attachment: the owning message, the attachment facts (its own
/// name, size and version — the provenance a shared blob must not erase), and
/// the provider item the transfer hydrates.
fn seed_attachment(
    store: &mut StateStore,
    message: i64,
    index: u32,
    name: &str,
    logical_size: Option<u64>,
    content_version: &str,
) {
    let tx = store.write_txn().expect("write");
    tx.apply_message_changes(
        &chat_key(),
        &[MessageChange::Observed(MessageRevision {
            message_id: MessageId(message),
            sender_id: Some(500),
            sent_at_ms: 1_000 * message,
            edited_at_ms: None,
            observed_at_ms: 1_000 * message + 5,
            payload_schema: SchemaFamily(1),
            payload: format!("payload-{message}").into_bytes(),
        })],
    )
    .expect("message");
    tx.upsert_attachment(&AttachmentFacts {
        key: attachment_key(message, index),
        original_name: Some(name.to_owned()),
        mime_type: None,
        logical_size,
        content_version: version(content_version),
        telegram_unique_id: None,
        telegram_file_id: None,
        file_reference: None,
        availability: AttachmentAvailability::Fetchable,
        can_be_saved: true,
    })
    .expect("attachment");
    tx.upsert_item(&ItemRecord {
        id: attachment_item(message, index),
        parent: Some(fixture::account_root_id(scope())),
        display_name: name.to_owned(),
        safe_name: name.to_owned(),
        metadata_version: metadata("m1"),
        content: Some(FileFacts {
            mime_type: None,
            logical_size,
            content_version: Some(version(content_version)),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("item");
    tx.commit().expect("commit");
}

/// Moves an item's projection to a new content version, as the change pipeline
/// would on observing a source-side republish (SYNC-042).
fn republish(store: &mut StateStore, item: &ItemId, from: &str, to: &str, size: u64, now_ms: i64) {
    let tx = store.write_txn().expect("write");
    tx.update_item_content(
        item,
        Some(&version(from)),
        &FileFacts {
            mime_type: None,
            logical_size: Some(size),
            content_version: Some(version(to)),
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

// ---------------------------------------------------------------------------
// The host: staging store, content-addressed cache store, reconcile inventory
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Inner {
    staging: HashMap<String, Vec<u8>>,
    cache: HashMap<String, Vec<u8>>,
    fail_promote: Option<String>,
}

/// A cheap handle over shared in-memory storage: it is `StagingHost`,
/// `PromotionHost`, and the reconciliation `LocalStorage` at once, so the same
/// bytes back every layer. Cloneable because a promote pass needs a staging
/// handle and a promotion handle at the same time; the clones share `inner`.
#[derive(Clone, Default, Debug)]
struct MemoryHost {
    inner: Arc<Mutex<Inner>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner").finish_non_exhaustive()
    }
}

/// The content-addressed cache handle for `hash`: a deterministic function of
/// the digest, which is what makes identical bytes collide onto one object.
fn reference_for(hash: &ContentHash) -> String {
    let ContentHash::Sha256(digest) = hash;
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("cache-{hex}")
}

impl MemoryHost {
    fn write_staging(&self, handle: &str, bytes: &[u8]) {
        self.inner
            .lock()
            .expect("lock")
            .staging
            .insert(handle.to_owned(), bytes.to_vec());
    }

    fn set_fail_promote(&self, detail: Option<&str>) {
        self.inner.lock().expect("lock").fail_promote = detail.map(str::to_owned);
    }

    fn cache_object(&self, reference: &str) -> Option<Vec<u8>> {
        self.inner
            .lock()
            .expect("lock")
            .cache
            .get(reference)
            .cloned()
    }

    fn insert_cache_object(&self, reference: &str, bytes: &[u8]) {
        self.inner
            .lock()
            .expect("lock")
            .cache
            .insert(reference.to_owned(), bytes.to_vec());
    }

    fn remove_cache_object_raw(&self, reference: &str) {
        self.inner.lock().expect("lock").cache.remove(reference);
    }

    fn cache_refs(&self) -> Vec<String> {
        let mut refs: Vec<String> = self
            .inner
            .lock()
            .expect("lock")
            .cache
            .keys()
            .cloned()
            .collect();
        refs.sort();
        refs
    }

    fn staging_refs(&self) -> Vec<String> {
        let mut refs: Vec<String> = self
            .inner
            .lock()
            .expect("lock")
            .staging
            .keys()
            .cloned()
            .collect();
        refs.sort();
        refs
    }
}

impl StagingHost for MemoryHost {
    fn open(
        &mut self,
        transfer: TransferId,
        existing: Option<&str>,
    ) -> Result<Box<dyn Staging>, StagingError> {
        let handle = existing
            .map(str::to_owned)
            .unwrap_or_else(|| format!("stage-{}", transfer.0));
        self.inner
            .lock()
            .expect("lock")
            .staging
            .entry(handle.clone())
            .or_default();
        Ok(Box::new(MemoryStaging {
            handle,
            inner: Arc::clone(&self.inner),
        }))
    }
}

impl PromotionHost for MemoryHost {
    fn promote(
        &mut self,
        staging: Option<&str>,
        hash: &ContentHash,
    ) -> Result<Materialization, PromotionHostError> {
        let mut inner = self.inner.lock().expect("lock");
        if let Some(detail) = inner.fail_promote.clone() {
            return Err(PromotionHostError::new(detail));
        }
        let reference = reference_for(hash);
        if inner.cache.contains_key(&reference) {
            // Dedup hit: the durable object already exists, so nothing is
            // moved and the redundant staging object is dropped.
            if let Some(handle) = staging {
                inner.staging.remove(handle);
            }
            return Ok(Materialization {
                reference,
                deduplicated: true,
            });
        }
        // Atomic rename: the staging bytes become the cache object.
        let bytes = match staging {
            Some(handle) => inner.staging.remove(handle).unwrap_or_default(),
            None => Vec::new(),
        };
        inner.cache.insert(reference.clone(), bytes);
        Ok(Materialization {
            reference,
            deduplicated: false,
        })
    }
}

impl LocalStorage for MemoryHost {
    fn cache_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .cache
            .iter()
            .map(|(reference, bytes)| StoredObject {
                reference: reference.clone(),
                size: bytes.len() as u64,
            })
            .collect())
    }

    fn staging_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        Ok(self
            .inner
            .lock()
            .expect("lock")
            .staging
            .iter()
            .map(|(reference, bytes)| StoredObject {
                reference: reference.clone(),
                size: bytes.len() as u64,
            })
            .collect())
    }

    fn remove_cache_object(&self, reference: &str) -> Result<(), StorageError> {
        self.inner.lock().expect("lock").cache.remove(reference);
        Ok(())
    }

    fn remove_staging_object(&self, reference: &str) -> Result<(), StorageError> {
        self.inner.lock().expect("lock").staging.remove(reference);
        Ok(())
    }
}

#[derive(Debug)]
struct MemoryStaging {
    handle: String,
    inner: Arc<Mutex<Inner>>,
}

impl Staging for MemoryStaging {
    fn handle(&self) -> &str {
        &self.handle
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), StagingError> {
        let mut inner = self.inner.lock().expect("lock");
        let object = inner.staging.entry(self.handle.clone()).or_default();
        let offset = usize::try_from(offset).expect("offset fits usize");
        let end = offset + bytes.len();
        if object.len() < end {
            object.resize(end, 0);
        }
        object[offset..end].copy_from_slice(bytes);
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), StagingError> {
        let inner = self.inner.lock().expect("lock");
        let object = inner
            .staging
            .get(&self.handle)
            .ok_or(StagingError::Failed {
                detail: "staging object vanished".to_owned(),
            })?;
        let offset = usize::try_from(offset).expect("offset fits usize");
        let end = offset + buf.len();
        let slice = object.get(offset..end).ok_or(StagingError::Failed {
            detail: "read past written bytes".to_owned(),
        })?;
        buf.copy_from_slice(slice);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Driver: carry a transfer to a terminal `done` with staged bytes
// ---------------------------------------------------------------------------

/// Requests, claims, stages `staged` at offset 0, records `completed`, and
/// completes — leaving a `done` transfer whose staging holds `staged`. The
/// split between `completed` (what the journal claims) and `staged` (what the
/// staging object holds) is what lets a truncation test claim more than it
/// staged.
#[allow(clippy::too_many_arguments)]
fn drive_to_done(
    store: &mut StateStore,
    host: &MemoryHost,
    item: &ItemId,
    // The pinned version comes from the item projection at request time; the
    // call sites name it only for readability.
    _content_version: &str,
    requested: &[ByteRange],
    completed: &[ByteRange],
    staged: &[u8],
    now_ms: i64,
) -> TransferId {
    let machine = machine();
    let created = machine
        .request(store, item, requested, Priority::FOREGROUND, now_ms)
        .expect("request");
    let RequestOutcome::Created {
        transfer,
        displaced,
    } = created
    else {
        panic!("expected a fresh transfer, got {created:?}");
    };
    assert!(displaced.is_none());

    let mut claim = match machine.claim(store, now_ms).expect("claim") {
        ClaimOutcome::Claimed(claim) => *claim,
        other => panic!("expected a claim, got {other:?}"),
    };
    assert_eq!(claim.id(), transfer);

    if !completed.is_empty() {
        let handle = format!("stage-{}", transfer.0);
        host.write_staging(&handle, staged);
        machine
            .record_progress(store, &mut claim, completed, &handle, now_ms)
            .expect("progress");
    }

    match machine.complete(store, &claim, now_ms).expect("complete") {
        CompleteOutcome::Promoted { .. } => transfer,
        other => panic!("expected promotion-ready, got {other:?}"),
    }
}

/// The whole-object common case: stage exactly `bytes`, claim `[0, len)`.
fn drive_whole(
    store: &mut StateStore,
    host: &MemoryHost,
    item: &ItemId,
    content_version: &str,
    bytes: &[u8],
    now_ms: i64,
) -> TransferId {
    let completed = if bytes.is_empty() {
        Vec::new()
    } else {
        vec![range(0, bytes.len() as u64)]
    };
    drive_to_done(
        store,
        host,
        item,
        content_version,
        &[],
        &completed,
        bytes,
        now_ms,
    )
}

fn promote(
    store: &mut StateStore,
    host: &MemoryHost,
    transfer: TransferId,
    now_ms: i64,
) -> Promotion {
    Promoter::default()
        .promote(
            store,
            &mut host.clone(),
            &mut host.clone(),
            transfer,
            now_ms,
        )
        .expect("promote")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn whole_attachment_verifies_hashes_and_publishes() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "photo.jpg", Some(64), "v1");
    let bytes = content(64, 0);
    let host = MemoryHost::default();

    let transfer = drive_whole(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &bytes,
        1_000,
    );
    let outcome = promote(&mut store, &host, transfer, 2_000);

    let Promotion::Materialized {
        hash,
        size,
        materialization_ref,
        deduplicated,
        attachment_linked,
    } = outcome
    else {
        panic!("expected materialized, got {outcome:?}");
    };
    assert_eq!(hash, sha256(&bytes));
    assert_eq!(size, 64);
    assert!(!deduplicated);
    assert!(attachment_linked);
    // The staging object was renamed into content-addressed cache and holds
    // exactly the verified bytes.
    assert_eq!(materialization_ref, reference_for(&sha256(&bytes)));
    assert_eq!(
        host.cache_object(&materialization_ref).as_deref(),
        Some(&bytes[..])
    );
    assert!(
        host.staging_refs().is_empty(),
        "staging consumed by the promote"
    );

    let read = store.read_txn().expect("read");
    let entry = read
        .cache_entry(&attachment_item(5, 0))
        .expect("entry")
        .expect("present");
    assert_eq!(entry.verification, CacheVerification::Verified);
    assert_eq!(entry.blob_hash, Some(sha256(&bytes)));
    assert_eq!(entry.size, 64);
    assert_eq!(entry.content_version, version("v1"));
    assert_eq!(
        entry.materialization_ref.as_deref(),
        Some(materialization_ref.as_str())
    );
    assert_eq!(entry.pin, None);

    let blob = read
        .blob(scope().account, &sha256(&bytes))
        .expect("blob")
        .expect("present");
    assert_eq!(blob.size, 64);
    assert_eq!(blob.first_seen_at_ms, 2_000);

    let attachment = read
        .attachment(&attachment_key(5, 0))
        .expect("attachment")
        .expect("present");
    assert_eq!(attachment.blob_hash, Some(sha256(&bytes)));
    assert_eq!(attachment.last_verified_at_ms, Some(2_000));
    assert_eq!(attachment.facts.original_name.as_deref(), Some("photo.jpg"));
}

#[test]
fn identical_content_deduplicates_but_keeps_per_attachment_provenance() {
    let mut store = open_store();
    seed_base(&mut store);
    // Two distinct attachments in two distinct messages, same bytes.
    seed_attachment(&mut store, 5, 0, "first.bin", Some(48), "v1");
    seed_attachment(&mut store, 6, 0, "second.bin", Some(48), "v1");
    let bytes = content(48, 7);
    let host = MemoryHost::default();

    let first = drive_whole(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &bytes,
        1_000,
    );
    let first_outcome = promote(&mut store, &host, first, 2_000);
    let second = drive_whole(
        &mut store,
        &host,
        &attachment_item(6, 0),
        "v1",
        &bytes,
        3_000,
    );
    let second_outcome = promote(&mut store, &host, second, 4_000);

    // First materializes; second finds the object already there.
    let (first_ref, first_dedup) = match first_outcome {
        Promotion::Materialized {
            materialization_ref,
            deduplicated,
            ..
        } => (materialization_ref, deduplicated),
        other => panic!("first: {other:?}"),
    };
    let (second_ref, second_dedup) = match second_outcome {
        Promotion::Materialized {
            materialization_ref,
            deduplicated,
            ..
        } => (materialization_ref, deduplicated),
        other => panic!("second: {other:?}"),
    };
    assert!(!first_dedup);
    assert!(second_dedup, "identical bytes reuse the existing object");
    assert_eq!(first_ref, second_ref, "one content-addressed object");
    assert_eq!(
        host.cache_refs(),
        vec![first_ref.clone()],
        "bytes stored once"
    );
    assert!(
        host.staging_refs().is_empty(),
        "both staging areas consumed"
    );

    let read = store.read_txn().expect("read");
    // One blob, referenced by both attachments (SYNC-052 back-reference).
    let referencing: HashSet<AttachmentKey> = read
        .attachments_referencing_blob(scope().account, &sha256(&bytes))
        .expect("refs")
        .into_iter()
        .collect();
    assert_eq!(
        referencing,
        HashSet::from([attachment_key(5, 0), attachment_key(6, 0)])
    );
    // Two cache entries sharing one on-disk object, each its own item.
    let entry_a = read
        .cache_entry(&attachment_item(5, 0))
        .expect("a")
        .expect("present");
    let entry_b = read
        .cache_entry(&attachment_item(6, 0))
        .expect("b")
        .expect("present");
    assert_eq!(entry_a.materialization_ref, entry_b.materialization_ref);
    assert_eq!(entry_a.blob_hash, entry_b.blob_hash);
    // Provenance preserved: each attachment keeps its own name.
    let name_a = read
        .attachment(&attachment_key(5, 0))
        .expect("a")
        .expect("present")
        .facts
        .original_name;
    let name_b = read
        .attachment(&attachment_key(6, 0))
        .expect("b")
        .expect("present")
        .facts
        .original_name;
    assert_eq!(name_a.as_deref(), Some("first.bin"));
    assert_eq!(name_b.as_deref(), Some("second.bin"));
}

#[test]
fn truncated_staging_fails_closed() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "photo.jpg", Some(64), "v1");
    let bytes = content(64, 0);
    let host = MemoryHost::default();

    // The journal claims the whole object, but the staging holds only half:
    // completeness passed the coverage gate, integrity must catch the rest.
    let transfer = drive_to_done(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &[],
        &[range(0, 64)],
        &bytes[..32],
        1_000,
    );
    let outcome = promote(&mut store, &host, transfer, 2_000);

    let Promotion::IntegrityFailed { disposal, .. } = outcome else {
        panic!("expected integrity failure, got {outcome:?}");
    };
    assert!(
        disposal.is_some(),
        "untrusted staging is handed back to drop"
    );

    // Nothing published: no blob, no cache entry, no cache object.
    let read = store.read_txn().expect("read");
    assert!(
        read.blob(scope().account, &sha256(&bytes))
            .expect("blob")
            .is_none()
    );
    assert!(
        read.cache_entry(&attachment_item(5, 0))
            .expect("entry")
            .is_none()
    );
    assert!(host.cache_refs().is_empty());
}

#[test]
fn vanished_staging_fails_closed() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "photo.jpg", Some(64), "v1");
    let bytes = content(64, 0);
    let host = MemoryHost::default();

    let transfer = drive_whole(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &bytes,
        1_000,
    );
    // Simulate the staged bytes being lost before verification.
    host.remove_cache_object_raw(&format!("stage-{}", transfer.0));
    host.inner
        .lock()
        .expect("lock")
        .staging
        .remove(&format!("stage-{}", transfer.0));

    let outcome = promote(&mut store, &host, transfer, 2_000);
    assert!(
        matches!(outcome, Promotion::IntegrityFailed { .. }),
        "got {outcome:?}"
    );
    let read = store.read_txn().expect("read");
    assert!(
        read.cache_entry(&attachment_item(5, 0))
            .expect("entry")
            .is_none()
    );
}

#[test]
fn version_drift_after_completion_fails_closed() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "photo.jpg", Some(64), "v1");
    let bytes = content(64, 0);
    let host = MemoryHost::default();

    let transfer = drive_whole(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &bytes,
        1_000,
    );
    // The item republishes to v2 after the transfer completed for v1.
    republish(&mut store, &attachment_item(5, 0), "v1", "v2", 64, 1_500);

    let outcome = promote(&mut store, &host, transfer, 2_000);
    let Promotion::VersionDeparted { category, disposal } = outcome else {
        panic!("expected version departure, got {outcome:?}");
    };
    assert_eq!(category, FailureCategory::VersionConflict);
    assert!(disposal.is_some(), "staging intact and handed back");

    let read = store.read_txn().expect("read");
    assert!(
        read.cache_entry(&attachment_item(5, 0))
            .expect("entry")
            .is_none()
    );
    assert!(
        read.blob(scope().account, &sha256(&bytes))
            .expect("blob")
            .is_none()
    );
    assert!(
        host.cache_refs().is_empty(),
        "no object materialized for a dead pin"
    );
}

#[test]
fn re_promoting_is_an_idempotent_no_op() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "photo.jpg", Some(64), "v1");
    let bytes = content(64, 0);
    let host = MemoryHost::default();

    let transfer = drive_whole(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &bytes,
        1_000,
    );
    let first = promote(&mut store, &host, transfer, 2_000);
    assert!(matches!(first, Promotion::Materialized { .. }));

    // The staging object is already consumed; a second call must not need it.
    let second = promote(&mut store, &host, transfer, 3_000);
    let Promotion::AlreadyMaterialized {
        hash,
        materialization_ref,
    } = second
    else {
        panic!("expected an idempotent no-op, got {second:?}");
    };
    assert_eq!(hash, Some(sha256(&bytes)));
    assert_eq!(materialization_ref, Some(reference_for(&sha256(&bytes))));

    // The blob's first-seen time is the first promotion's, not the second's.
    let read = store.read_txn().expect("read");
    let blob = read
        .blob(scope().account, &sha256(&bytes))
        .expect("blob")
        .expect("present");
    assert_eq!(blob.first_seen_at_ms, 2_000);
    assert_eq!(host.cache_refs().len(), 1);
}

#[test]
fn host_storage_refusal_writes_no_row_file_before_row() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "photo.jpg", Some(64), "v1");
    let bytes = content(64, 0);
    let host = MemoryHost::default();

    let transfer = drive_whole(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &bytes,
        1_000,
    );
    host.set_fail_promote(Some("no space left on device"));

    let error = Promoter::default()
        .promote(
            &mut store,
            &mut host.clone(),
            &mut host.clone(),
            transfer,
            2_000,
        )
        .expect_err("promote should surface the host refusal");
    match error {
        EngineError::Storage { detail } => assert!(detail.contains("no space")),
        other => panic!("expected storage error, got {other:?}"),
    }

    // The row is never written when the file step fails (SYNC-053 ordering).
    let read = store.read_txn().expect("read");
    assert!(
        read.cache_entry(&attachment_item(5, 0))
            .expect("entry")
            .is_none()
    );
    assert!(
        read.blob(scope().account, &sha256(&bytes))
            .expect("blob")
            .is_none()
    );
    // The staged bytes are intact for a retry.
    assert_eq!(host.staging_refs(), vec![format!("stage-{}", transfer.0)]);
}

#[test]
fn partial_range_is_not_a_blob() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "video.bin", Some(64), "v1");
    let bytes = content(64, 0);
    let host = MemoryHost::default();

    // A ranged read of the first 32 bytes: complete for its request, but not
    // the whole object.
    let transfer = drive_to_done(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &[range(0, 32)],
        &[range(0, 32)],
        &bytes[..32],
        1_000,
    );
    let outcome = promote(&mut store, &host, transfer, 2_000);
    let Promotion::NotWholeContent { disposal } = outcome else {
        panic!("expected not-whole-content, got {outcome:?}");
    };
    assert!(disposal.is_some());

    let read = store.read_txn().expect("read");
    assert!(
        read.cache_entry(&attachment_item(5, 0))
            .expect("entry")
            .is_none()
    );
    assert!(host.cache_refs().is_empty());
}

#[test]
fn zero_byte_object_promotes_to_the_empty_content_hash() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "empty.txt", Some(0), "v1");
    let host = MemoryHost::default();

    let transfer = drive_whole(&mut store, &host, &attachment_item(5, 0), "v1", &[], 1_000);
    let outcome = promote(&mut store, &host, transfer, 2_000);

    let Promotion::Materialized {
        hash,
        size,
        materialization_ref,
        attachment_linked,
        ..
    } = outcome
    else {
        panic!("expected materialized, got {outcome:?}");
    };
    assert_eq!(hash, sha256(&[]));
    assert_eq!(size, 0);
    assert!(attachment_linked);
    assert_eq!(
        host.cache_object(&materialization_ref).as_deref(),
        Some(&[][..])
    );

    let read = store.read_txn().expect("read");
    let entry = read
        .cache_entry(&attachment_item(5, 0))
        .expect("entry")
        .expect("present");
    assert_eq!(entry.size, 0);
    assert_eq!(entry.verification, CacheVerification::Verified);
}

#[test]
fn a_pinned_item_promotes_pinned_and_stays_out_of_the_eviction_scan() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "photo.jpg", Some(64), "v1");
    let bytes = content(64, 0);
    let host = MemoryHost::default();
    {
        let tx = store.write_txn().expect("write");
        tx.pin_item(&attachment_item(5, 0), PinOrigin::User, 500)
            .expect("pin");
        tx.commit().expect("commit");
    }

    let transfer = drive_whole(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &bytes,
        1_000,
    );
    let outcome = promote(&mut store, &host, transfer, 2_000);
    assert!(matches!(outcome, Promotion::Materialized { .. }));

    let read = store.read_txn().expect("read");
    let entry = read
        .cache_entry(&attachment_item(5, 0))
        .expect("entry")
        .expect("present");
    assert_eq!(entry.pin, Some(PinOrigin::User));
    // Pinned content never enters the eviction scan (SYNC-051).
    assert!(read.eviction_candidates(10).expect("scan").is_empty());
}

#[test]
fn hashing_is_correct_across_many_small_read_grains() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "photo.jpg", Some(64), "v1");
    let bytes = content(64, 3);
    let host = MemoryHost::default();

    let transfer = drive_whole(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &bytes,
        1_000,
    );
    // A 7-byte read grain forces the streaming hasher across ten partial
    // reads plus a final short one — the digest must still be the one-shot.
    let promoter = Promoter::new(PromotionConfig {
        read_chunk_bytes: NonZeroU64::new(7).expect("non-zero"),
    });
    let outcome = promoter
        .promote(
            &mut store,
            &mut host.clone(),
            &mut host.clone(),
            transfer,
            2_000,
        )
        .expect("promote");
    match outcome {
        Promotion::Materialized { hash, .. } => assert_eq!(hash, sha256(&bytes)),
        other => panic!("expected materialized, got {other:?}"),
    }
}

#[test]
fn interrupted_promotion_converges_under_reconciliation() {
    let mut store = open_store();
    seed_base(&mut store);
    seed_attachment(&mut store, 5, 0, "photo.jpg", Some(64), "v1");
    let bytes = content(64, 0);
    let host = MemoryHost::default();

    let transfer = drive_whole(
        &mut store,
        &host,
        &attachment_item(5, 0),
        "v1",
        &bytes,
        1_000,
    );
    let materialization_ref = match promote(&mut store, &host, transfer, 2_000) {
        Promotion::Materialized {
            materialization_ref,
            ..
        } => materialization_ref,
        other => panic!("expected materialized, got {other:?}"),
    };

    // A cleanly promoted state has nothing to reconcile: the staging was
    // consumed, the row and the object agree.
    let clean = store.reconcile(&host, 2_100).expect("reconcile");
    assert!(
        clean.plan.is_empty(),
        "clean state: {:?}",
        clean.plan.findings
    );

    // A file-before-row crash (object written, row never committed) shows up
    // as an orphan the pass deletes.
    host.insert_cache_object("cache-orphan", &bytes);
    let orphaned = store.reconcile(&host, 2_200).expect("reconcile");
    assert!(orphaned.converged());
    assert!(
        host.cache_object("cache-orphan").is_none(),
        "orphan reclaimed"
    );
    // The legitimately promoted object is untouched.
    assert!(host.cache_object(&materialization_ref).is_some());

    // A provider/OS eviction of the materialized bytes (SYNC-053) drops the
    // cache entry but never the pin.
    {
        let tx = store.write_txn().expect("write");
        tx.set_cache_pin(&attachment_item(5, 0), Some(PinOrigin::User))
            .expect("pin fold");
        tx.pin_item(&attachment_item(5, 0), PinOrigin::User, 500)
            .expect("pin");
        tx.commit().expect("commit");
    }
    host.remove_cache_object_raw(&materialization_ref);
    let purged = store.reconcile(&host, 2_300).expect("reconcile");
    assert!(purged.converged());
    let read = store.read_txn().expect("read");
    assert!(
        read.cache_entry(&attachment_item(5, 0))
            .expect("entry")
            .is_none(),
        "entry dropped"
    );
    assert!(
        read.pin(&attachment_item(5, 0)).expect("pin").is_some(),
        "pin survives"
    );
}
