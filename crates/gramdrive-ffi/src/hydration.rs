//! Session-owned attachment and allowed-story hydration composition.
//!
//! This is the top-of-graph wiring the lower crates deliberately cannot own:
//! durable content locators from `gramdrive-state`, the TDLib ranged
//! downloader from `gramdrive-source-tdjson`, the transfer/fetch/promote
//! engine, and filesystem staging/cache hosts under the shared data root.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration as StdDuration, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use std::time::Instant;

use gramdrive_engine::backfill::{BackfillScheduler, HostConditions};
use gramdrive_engine::cache::{
    Materialization, Promoter, Promotion, PromotionHost, PromotionHostError,
};
use gramdrive_engine::fetch::{
    AttemptEnd, Clock, FetchCoordinator, RunOutcome, Staging, StagingError, StagingHost,
};
use gramdrive_engine::render_pipeline::{
    GeneratedFileLease, GeneratedFileLeaseAcquire, reclaim_unreferenced_generations,
};
use gramdrive_engine::transfer::{Priority, TransferMachine};
use gramdrive_model::ByteRange;
use gramdrive_model::identity::{
    AccountId, AccountKey, AttachmentKey, CanonicalKey, ContentHash, ItemId, ItemKey,
    StoryAppearanceLocation,
};
use gramdrive_model::version::ContentVersion;
use gramdrive_source::{
    ContentChunk, ContentSink, ContentSource, FetchRequest, SinkControl, SourceError, SourceFuture,
    Thumbnail, ThumbnailSpec,
};
use gramdrive_source_tdjson::attachment::map_message_attachments;
use gramdrive_source_tdjson::download::{
    CatalogEntry, DownloadConfig, FetchCatalog, FileTarget, RefreshTarget, RefreshedFileTarget,
    RemoteFileType, TdDownloader,
};
use gramdrive_source_tdjson::message::{
    AttachmentAvailability as SourceAvailability, MessageRecord, NORMALIZED_MESSAGE_SCHEMA_FAMILY,
};
use gramdrive_source_tdjson::runtime::TdClient;
use gramdrive_source_tdjson::thumbnail::{
    TdThumbnailer, ThumbnailCatalog, ThumbnailConfig, ThumbnailTarget,
};
use gramdrive_state::repo::{
    AttachmentAvailability as StateAttachmentAvailability, CacheVerification, FailureCategory,
    ItemAvailability as StateItemAvailability, RetentionMode, StoryContentState, TransferId,
};
use gramdrive_state::{LocalStorage, StateStore, StorageError, StoredObject};

use crate::api::{CancellationToken, DriveError, ProgressListener, TransferProgress};
use crate::shared_state::shared_state_layout;

type ContentGenerationKey = (String, String);

// LRU is advisory: cache-hit bytes have already been verified, while quota
// accounting and eviction eligibility are durable properties of the cache
// entry itself. Coarsening avoids a write transaction for every open.
const CACHE_TOUCH_GRANULARITY_MS: i64 = 60_000;
const GENERATED_LEASE_WAIT: StdDuration = StdDuration::from_millis(250);

/// A verified materialization returned to a native provider host.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HydratedFile {
    /// Absolute path of the verified cache object.
    pub path: String,
    /// Content version the bytes were verified against.
    pub content_version: String,
    /// Exact verified extent.
    pub byte_count: u64,
    /// Opaque hand-off lease for a generated document. The native agent owns
    /// its release after File Provider has cloned the staged path; attachment
    /// cache objects need no render-generation lease.
    pub lease_id: Option<String>,
}

/// A bounded preview staged for a native provider host.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HydratedThumbnail {
    /// Absolute path of the atomically published preview object.
    pub path: String,
    /// Content version whose preview was requested.
    pub content_version: String,
    /// Encoded image MIME type returned by the source.
    pub mime_type: String,
    /// Exact encoded preview byte count.
    pub byte_count: u64,
}

/// The process-wide hydration owner for one shared data root.
///
/// Namespace sessions register their own `TdClient`; requests are routed by
/// the account encoded in the stable item identity. One coordinator and one
/// durable queue therefore serve every account without crossing session
/// ownership.
#[derive(uniffi::Object)]
pub struct Hydrator {
    database: String,
    coordinator: FetchCoordinator,
    storage: FileStorage,
    sources: Arc<RoutingSource>,
    admissions: Mutex<HashMap<ContentGenerationKey, Weak<Mutex<()>>>>,
    materializations: MaterializationRegistry,
    staged_leases: Mutex<HashMap<String, GeneratedFileLease>>,
    archive_batches: Mutex<HashMap<i64, Arc<CancellationToken>>>,
    /// Owns all demand hydration and transfer-driver work. UniFFI's Tokio
    /// compatibility wrapper may be polled by Swift's cooperative executor,
    /// but it only forwards a result from this independent multi-thread
    /// runtime; it never runs SQLite, filesystem, or transfer work itself.
    runtime: Arc<HydrationRuntime>,
    #[cfg(test)]
    cancel_probe: Mutex<Option<Arc<dyn CancelProbe>>>,
    #[cfg(test)]
    promotion_probe: Mutex<Option<Arc<dyn PromotionProbe>>>,
    #[cfg(test)]
    materialization_probe: Mutex<Option<Arc<dyn MaterializationProbe>>>,
    #[cfg(test)]
    driver_probe: Mutex<Option<Arc<dyn DriverProbe>>>,
    changed: Arc<tokio::sync::watch::Sender<u64>>,
    next_registration: std::sync::atomic::AtomicU64,
    next_staged_lease: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for Hydrator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Hydrator").finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct HydrationDriver {
    database: String,
    coordinator: FetchCoordinator,
    storage: FileStorage,
    sources: Arc<RoutingSource>,
    materializations: MaterializationRegistry,
    changed: Arc<tokio::sync::watch::Sender<u64>>,
    #[cfg(test)]
    promotion_probe: Option<Arc<dyn PromotionProbe>>,
    #[cfg(test)]
    driver_probe: Option<Arc<dyn DriverProbe>>,
}

type MaterializationRegistry = Arc<Mutex<MaterializationRegistryState>>;

/// The independent Tokio owner for native hydration. A Hydrator can be
/// released by an exported async future, so teardown uses Tokio's nonblocking
/// shutdown rather than dropping a runtime inside that async context.
struct HydrationRuntime {
    runtime: Mutex<Option<tokio::runtime::Runtime>>,
    handle: tokio::runtime::Handle,
}

/// Native hydration has a deliberately small scheduler because transfer
/// drivers are bounded separately. Keep this named so saturation coverage
/// derives its capacity from the production runtime rather than duplicating
/// an unrelated test literal.
const HYDRATION_RUNTIME_WORKERS: usize = 2;

impl HydrationRuntime {
    fn new() -> Result<Arc<Self>, DriveError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            // The server admits at most eight native hydrations. Two workers
            // ensure a slow transfer driver cannot serialize every foreground
            // completion, while Tokio's blocking pool keeps synchronous state
            // setup out of those scheduler workers.
            .worker_threads(HYDRATION_RUNTIME_WORKERS)
            .max_blocking_threads(8)
            .thread_name("gramdrive-hydration")
            .enable_time()
            .build()
            .map_err(|error| DriveError::Internal {
                detail: format!("could not start hydration runtime: {error}"),
            })?;
        let handle = runtime.handle().clone();
        Ok(Arc::new(Self {
            runtime: Mutex::new(Some(runtime)),
            handle,
        }))
    }

    fn handle(&self) -> tokio::runtime::Handle {
        self.handle.clone()
    }
}

impl Drop for HydrationRuntime {
    fn drop(&mut self) {
        let runtime = self
            .runtime
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(runtime) = runtime {
            runtime.shutdown_background();
        }
    }
}

#[derive(Default)]
struct MaterializationRegistryState {
    by_transfer: HashMap<TransferId, Arc<MaterializationCompletion>>,
    pending: HashMap<ContentGenerationKey, Weak<MaterializationCompletion>>,
}

// Native hydration treats 60 seconds without an event as a dead request.
// Retaining a terminal result for twice that deadline covers an opener that
// attached immediately before publication without keeping transfer history.
const MATERIALIZATION_RESULT_RETENTION: StdDuration = StdDuration::from_secs(120);

struct MaterializationCompletion {
    result: tokio::sync::watch::Sender<Option<Result<(), DriveError>>>,
}

impl Default for MaterializationCompletion {
    fn default() -> Self {
        let (result, _) = tokio::sync::watch::channel(None);
        Self { result }
    }
}

impl MaterializationCompletion {
    fn resolve(&self, result: Result<(), DriveError>) {
        self.result.send_replace(Some(result));
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<Option<Result<(), DriveError>>> {
        self.result.subscribe()
    }

    fn is_resolved(&self) -> bool {
        self.result.borrow().is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelDecision {
    Resolved,
    AwaitTerminal,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelCheckpoint {
    LastReaderClosed,
    SourceGenerationCancelled,
}

#[cfg(test)]
trait CancelProbe: Send + Sync {
    fn checkpoint(&self, checkpoint: CancelCheckpoint);
}

#[cfg(test)]
trait PromotionProbe: Send + Sync {
    fn before_publication(&self, transfer: TransferId);
}

#[cfg(test)]
trait MaterializationProbe: Send + Sync {
    fn before_bind(&self, transfer: TransferId);
}

#[cfg(test)]
trait DriverProbe: Send + Sync {
    fn before_claim(&self);
}

struct ArchiveProgress;

impl ProgressListener for ArchiveProgress {
    fn on_progress(&self, _progress: TransferProgress) {}
}

static HYDRATORS: OnceLock<Mutex<HashMap<PathBuf, Weak<Hydrator>>>> = OnceLock::new();

fn hydrators() -> &'static Mutex<HashMap<PathBuf, Weak<Hydrator>>> {
    HYDRATORS.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Hydrator {
    pub(crate) fn shared(data_root: &str) -> Result<Arc<Self>, DriveError> {
        let layout = shared_state_layout(data_root.to_owned())?;
        let key = fs::canonicalize(data_root).unwrap_or_else(|_| PathBuf::from(data_root));
        let mut registry = hydrators()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(existing) = registry.get(&key).and_then(Weak::upgrade) {
            return Ok(existing);
        }

        let storage = FileStorage::new(PathBuf::from(&layout.cache_dir))?;
        let runtime = HydrationRuntime::new()?;
        let mut store = StateStore::open(&layout.database_file).map_err(state_error)?;
        let report = store.reconcile(&storage, now_ms()).map_err(state_error)?;
        if !report.converged() {
            return Err(DriveError::Storage {
                detail: "hydration startup reconciliation did not converge".to_owned(),
            });
        }
        let accounts = store
            .read_txn()
            .map_err(state_error)?
            .accounts()
            .map_err(state_error)?;
        let sources = Arc::new(RoutingSource::default());
        let (changed, _) = tokio::sync::watch::channel(0);
        let hydrator = Arc::new(Self {
            database: layout.database_file,
            coordinator: FetchCoordinator::new(
                TransferMachine::default(),
                gramdrive_engine::fetch::FetchConfig::default(),
            ),
            storage,
            sources,
            admissions: Mutex::new(HashMap::new()),
            materializations: Arc::new(Mutex::new(MaterializationRegistryState::default())),
            staged_leases: Mutex::new(HashMap::new()),
            archive_batches: Mutex::new(HashMap::new()),
            runtime,
            #[cfg(test)]
            cancel_probe: Mutex::new(None),
            #[cfg(test)]
            promotion_probe: Mutex::new(None),
            #[cfg(test)]
            materialization_probe: Mutex::new(None),
            #[cfg(test)]
            driver_probe: Mutex::new(None),
            changed: Arc::new(changed),
            next_registration: std::sync::atomic::AtomicU64::new(1),
            next_staged_lease: std::sync::atomic::AtomicU64::new(1),
        });
        // This constructor is the synchronous production relaunch boundary
        // used by the owned namespace session. Converge every durable
        // post-commit byte-policy journal before returning a hydrator that a
        // provider or TDLib session can use.
        for account in accounts {
            hydrator.resume_retention_purge(account.account)?;
            hydrator.purge_disallowed_attachment_materializations(account.account)?;
            hydrator.purge_disallowed_story_materializations(account.account)?;
        }
        registry.insert(key, Arc::downgrade(&hydrator));
        Ok(hydrator)
    }

    pub(crate) fn register_source(
        &self,
        data_root: &str,
        account_id: i64,
        client: TdClient,
    ) -> Result<u64, DriveError> {
        let layout = shared_state_layout(data_root.to_owned())?;
        let catalog = Arc::new(StateFetchCatalog::open(
            &layout.database_file,
            AccountId(account_id),
        )?);
        let downloader = Arc::new(TdDownloader::new(
            client.clone(),
            catalog,
            DownloadConfig::default(),
        ));
        let thumbnail_catalog = Arc::new(StateThumbnailCatalog::open(
            &layout.database_file,
            AccountId(account_id),
        )?);
        let thumbnailer = Arc::new(TdThumbnailer::new(
            client,
            thumbnail_catalog,
            ThumbnailConfig::default(),
        ));
        let token = self.next_registration.fetch_add(1, Ordering::Relaxed);
        self.sources
            .register_with_thumbnail(account_id, token, downloader, thumbnailer);
        Ok(token)
    }

    pub(crate) fn unregister_source(&self, account_id: i64, token: u64) {
        if let Some(batch) = self
            .archive_batches
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&account_id)
            .cloned()
        {
            batch.cancel();
        }
        self.sources.unregister(account_id, token);
    }

    /// Starts at most one policy-gated Archive hydration for this account.
    ///
    /// The namespace worker calls this once per tick after metadata work.
    /// The durable scheduler enforces Archive Mode, metadata completion,
    /// pacing, pause/flood state, and host network/power/disk conditions.
    /// The actual fetch goes through `hydrate_inner`, so authoritative
    /// attachment/story restrictions are revalidated before source access.
    pub(crate) fn schedule_archive_backfill(
        self: &Arc<Self>,
        scheduler: BackfillScheduler,
        scope: gramdrive_model::identity::AccountScope,
        conditions: HostConditions,
    ) -> Result<bool, DriveError> {
        {
            let batches = self
                .archive_batches
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if batches.contains_key(&scope.account.account_id.0) {
                return Ok(false);
            }
        }
        let mut store = StateStore::open(&self.database).map_err(state_error)?;
        let Some(item) = scheduler
            .archive_media_worklist(&mut store, scope, conditions, now_ms(), 1)
            .map_err(engine_error)?
            .into_iter()
            .next()
        else {
            return Ok(false);
        };
        scheduler
            .note_dispatch(&mut store, scope, now_ms())
            .map_err(engine_error)?;
        let token = CancellationToken::new();
        {
            let mut batches = self
                .archive_batches
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if batches.contains_key(&scope.account.account_id.0) {
                return Ok(false);
            }
            batches.insert(scope.account.account_id.0, Arc::clone(&token));
        }

        let hydrator = Arc::clone(self);
        let item_text = item.text().to_owned();
        let account_id = scope.account.account_id.0;
        let spawn = std::thread::Builder::new()
            .name(format!("gramdrive-archive-{account_id}"))
            .spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .map_err(|error| DriveError::Internal {
                        detail: format!("could not start Archive hydration runtime: {error}"),
                    })
                    .and_then(|runtime| {
                        runtime.block_on(hydrator.hydrate_inner(
                            account_id,
                            item_text,
                            None,
                            Arc::new(ArchiveProgress),
                            Arc::clone(&token),
                        ))
                    });
                if let Err(DriveError::RateLimited { retry_after_ms, .. }) = result
                    && let Ok(mut store) = StateStore::open(&hydrator.database)
                {
                    let retry_after_ms = retry_after_ms.and_then(|value| i64::try_from(value).ok());
                    let _ =
                        scheduler.note_flood_wait(&mut store, scope, retry_after_ms, 1, now_ms());
                }
                hydrator
                    .archive_batches
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(&account_id);
            });
        if let Err(error) = spawn {
            self.archive_batches
                .lock()
                .unwrap_or_else(|lock_error| lock_error.into_inner())
                .remove(&account_id);
            return Err(DriveError::Internal {
                detail: format!("could not spawn Archive hydration worker: {error}"),
            });
        }
        Ok(true)
    }

    /// Removes bytes and byte-retention intent for attachments that became
    /// authoritatively restricted after materialization.
    ///
    /// Deletion/unavailability alone is intentionally not a restriction:
    /// Audit may retain already-observed allowed bytes. Protection,
    /// `can_be_saved=false`, view-once, and restricted item/content facts
    /// override Mirror, Audit, Archive Mode, and explicit pins. Database
    /// ownership and a physical-file purge journal commit together; replay
    /// then removes or safely preserves a shared object and acknowledges it.
    pub(crate) fn purge_disallowed_attachment_materializations(
        &self,
        account: AccountKey,
    ) -> Result<(), DriveError> {
        let mut store = StateStore::open(&self.database).map_err(state_error)?;
        let purge = {
            let read = store.read_txn().map_err(state_error)?;
            let mut candidates: HashSet<_> = read
                .materialized_attachment_keys(account)
                .map_err(state_error)?
                .into_iter()
                .collect();
            candidates.extend(
                read.retained_attachment_keys(account)
                    .map_err(state_error)?,
            );

            let mut purge = Vec::new();
            for key in candidates {
                let attachment_restricted = read
                    .attachment(&key)
                    .map_err(state_error)?
                    .is_some_and(|attachment| {
                        !attachment.facts.can_be_saved
                            || matches!(
                                attachment.facts.availability,
                                StateAttachmentAvailability::Restricted
                                    | StateAttachmentAvailability::ViewOnce
                            )
                    });
                let canonical = ItemKey::Canonical(CanonicalKey::Attachment(key)).id();
                let mut items = vec![canonical.clone()];
                items.extend(
                    read.appearances_of(&canonical)
                        .map_err(state_error)?
                        .into_iter()
                        .map(|item| item.id),
                );
                let item_restricted = items
                    .iter()
                    .try_fold(false, |restricted, item| {
                        Ok::<_, gramdrive_state::StateError>(
                            restricted
                                || read.item(item)?.is_some_and(|stored| {
                                    stored.availability == StateItemAvailability::Restricted
                                }),
                        )
                    })
                    .map_err(state_error)?;
                let source_restricted = read
                    .chat(&key.message.chat)
                    .map_err(state_error)?
                    .is_none_or(|chat| chat.is_protected)
                    || attachment_restricted;
                if source_restricted || item_restricted {
                    purge.push((key, canonical, items));
                }
            }
            purge
        };
        if purge.is_empty() {
            return Ok(());
        }

        let tx = store.write_txn().map_err(state_error)?;
        let queued_at_ms = now_ms();
        for (key, canonical, items) in &purge {
            for item in items {
                tx.queue_restricted_cache_purge(account, item, queued_at_ms)
                    .map_err(state_error)?;
            }
            tx.queue_retained_attachment_purge(account, canonical, queued_at_ms)
                .map_err(state_error)?;
            tx.unlink_attachment_blob(key).map_err(state_error)?;
        }
        tx.purge_unreferenced_blobs(account).map_err(state_error)?;
        tx.commit().map_err(state_error)?;
        self.resume_retention_purge(account)?;
        Ok(())
    }

    /// Applies story retention to materialized provider bytes after a story
    /// projection commit.
    ///
    /// Story appearances whose canonical story was purged or became
    /// protected are removed. Audit-retained allowed stories therefore keep
    /// observed bytes, while an active-to-profile transition keeps the old
    /// cache row as the canonical-blob materialization reused by the new
    /// month appearance.
    /// The object itself is deleted only after the row removal proves no
    /// other appearance or attachment references it.
    pub(crate) fn purge_disallowed_story_materializations(
        &self,
        account: AccountKey,
    ) -> Result<(), DriveError> {
        let mut store = StateStore::open(&self.database).map_err(state_error)?;
        let purge = {
            let read = store.read_txn().map_err(state_error)?;
            let retention = read.retention_mode(account).map_err(state_error)?;
            let mut purge = Vec::new();
            for entry in read
                .cache_entries_for_account(account)
                .map_err(state_error)?
            {
                let ItemKey::StoryAppearance(appearance) = entry.item.key() else {
                    continue;
                };
                let retained_and_allowed =
                    match read.story(&appearance.story).map_err(state_error)? {
                        Some(story) => {
                            let currently_allowed = story.facts.can_be_forwarded
                                && story.facts.availability
                                    == StateAttachmentAvailability::Fetchable
                                && story.facts.content_state == StoryContentState::Available;
                            let retained_audit_bytes = retention == Some(RetentionMode::Audit)
                                && story.facts.can_be_forwarded
                                && story.facts.content_state == StoryContentState::Inaccessible
                                && story.blob_hash.as_ref() == entry.blob_hash.as_ref()
                                && story.blob_hash.is_some()
                                && read
                                    .story_appearances(&appearance.story)
                                    .map_err(state_error)?
                                    .iter()
                                    .any(|candidate| {
                                        matches!(
                                            candidate.location,
                                            StoryAppearanceLocation::Month { .. }
                                        ) && candidate.removed_at_ms.is_some()
                                    });
                            currently_allowed || retained_audit_bytes
                        }
                        None => false,
                    };
                if !retained_and_allowed {
                    purge.push(entry);
                }
            }
            purge
        };
        if purge.is_empty() {
            return Ok(());
        }
        let tx = store.write_txn().map_err(state_error)?;
        for entry in &purge {
            tx.remove_cache_entry(&entry.item).map_err(state_error)?;
        }
        tx.commit().map_err(state_error)?;

        for reference in purge
            .iter()
            .filter_map(|entry| entry.materialization_ref.as_deref())
        {
            let still_referenced = store
                .read_txn()
                .map_err(state_error)?
                .materialization_ref_referenced(reference)
                .map_err(state_error)?;
            if !still_referenced {
                self.storage
                    .remove_cache_object(reference)
                    .map_err(|error| DriveError::Storage {
                        detail: format!("could not purge removed story bytes: {error}"),
                    })?;
            }
        }
        Ok(())
    }

    /// Resumes physical object deletion journalled by an Audit-to-Mirror
    /// database transaction. Each removed, already absent, or shared object is
    /// acknowledged in a separate short transaction, so a crash before or
    /// after either side is safe to replay. The return value counts journal
    /// acknowledgements, not filesystem removals. A handle still referenced by
    /// another cache row is only acknowledged; shared bytes remain owned.
    pub(crate) fn resume_retention_purge(&self, account: AccountKey) -> Result<u64, DriveError> {
        let mut acknowledged = 0_u64;
        loop {
            let mut store = StateStore::open(&self.database).map_err(state_error)?;
            let batch = store
                .read_txn()
                .map_err(state_error)?
                .retention_purge_queue(account, 64)
                .map_err(state_error)?;
            if batch.is_empty() {
                return Ok(acknowledged);
            }
            for pending in batch {
                let referenced = store
                    .read_txn()
                    .map_err(state_error)?
                    .materialization_ref_referenced(&pending.materialization_ref)
                    .map_err(state_error)?;
                if !referenced {
                    self.storage
                        .remove_cache_object(&pending.materialization_ref)
                        .map_err(|error| DriveError::Storage {
                            detail: format!("could not purge Audit-only bytes: {error}"),
                        })?;
                }
                let tx = store.write_txn().map_err(state_error)?;
                tx.acknowledge_retention_purge(account, &pending.materialization_ref)
                    .map_err(state_error)?;
                tx.commit().map_err(state_error)?;
                acknowledged = acknowledged.saturating_add(1);
            }
        }
    }

    fn spawn_driver(&self) {
        let driver = HydrationDriver {
            database: self.database.clone(),
            coordinator: self.coordinator.clone(),
            storage: self.storage.clone(),
            sources: Arc::clone(&self.sources),
            materializations: Arc::clone(&self.materializations),
            changed: Arc::clone(&self.changed),
            #[cfg(test)]
            promotion_probe: self
                .promotion_probe
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
            #[cfg(test)]
            driver_probe: self
                .driver_probe
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        };
        // Drivers are deliberately detached from any one provider request:
        // closing one coalesced reader must not drop the source future needed
        // by the readers that remain. Durable state is the completion channel.
        drop(tokio::spawn(async move {
            let _ = drive_queue(&driver).await;
            driver.changed.send_modify(|sequence| {
                *sequence = sequence.wrapping_add(1);
            });
        }));
    }

    /// Returns the short-lived admission lock for one pinned object.
    ///
    /// The registry stores weak references so an unbounded sequence of opens
    /// does not retain one mutex per historical content version. Holding the
    /// returned lock makes reader registration, cancellation-generation
    /// establishment, and driver eligibility one atomic synchronous step.
    fn admission_for(&self, item: &ItemId, version: &ContentVersion) -> Arc<Mutex<()>> {
        let key = RoutingSource::cancellation_key(item, version);
        let mut admissions = self
            .admissions
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        admissions.retain(|_, admission| admission.strong_count() != 0);
        if let Some(admission) = admissions.get(&key).and_then(Weak::upgrade) {
            return admission;
        }
        let admission = Arc::new(Mutex::new(()));
        admissions.insert(key, Arc::downgrade(&admission));
        admission
    }

    #[cfg(test)]
    fn cancel_checkpoint(&self, checkpoint: CancelCheckpoint) {
        let probe = self
            .cancel_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(probe) = probe {
            probe.checkpoint(checkpoint);
        }
    }

    #[cfg(test)]
    fn materialization_checkpoint(&self, transfer: TransferId) {
        let probe = self
            .materialization_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(probe) = probe {
            probe.before_bind(transfer);
        }
    }

    async fn hydrate_inner(
        &self,
        account_id: i64,
        item_text: String,
        pinned_text: Option<String>,
        listener: Arc<dyn ProgressListener>,
        token: Arc<CancellationToken>,
    ) -> Result<HydratedFile, DriveError> {
        if token.is_cancelled() {
            return Err(cancelled());
        }
        let item = ItemId::parse_text(&item_text).map_err(|error| DriveError::InvalidArgument {
            detail: format!("item id is malformed: {error}"),
        })?;
        if item_account_id(&item) != Some(account_id) {
            return Err(DriveError::NotFound {
                detail: "item does not belong to the requested account".to_owned(),
            });
        }

        let mut store = StateStore::open(&self.database).map_err(state_error)?;
        let admission = admission(
            &mut store,
            AccountId(account_id),
            &item,
            pinned_text.as_deref(),
        )?;
        let ContentAdmission {
            version,
            extent,
            cache_only,
        } = admission;

        if let Some(hit) = cached_file(
            &mut store,
            &item,
            &version,
            extent,
            now_ms(),
            token.as_ref(),
        )? {
            listener.on_progress(TransferProgress {
                bytes_transferred: hit.file.byte_count,
                bytes_total: Some(hit.file.byte_count),
            });
            return Ok(self.adopt_cached_file(hit));
        }
        if cache_only {
            return Err(generated_cache_miss_error(
                &mut store,
                AccountId(account_id),
                &item,
                &version,
            ));
        }
        if !self.sources.has_account(account_id) {
            return Err(DriveError::AuthRequired {
                detail: "the account's owned Telegram session is unavailable".to_owned(),
            });
        }

        let generation = self.admission_for(&item, &version);
        let _generation = generation.lock().unwrap_or_else(|error| error.into_inner());
        // Reserve the causal completion before the durable request becomes
        // visible to an already-running queue driver. The transfer id is not
        // known yet, so the reservation is temporarily keyed by the pinned
        // content generation and bound immediately after `open`/`hydrate`.
        let reservation = reserve_materialization(&self.materializations, &item, &version);
        let (transfer, reader) = if extent == 0 {
            let opened = self
                .coordinator
                .subscribe(&mut store, &item, &[], Priority::FOREGROUND, now_ms())
                .map_err(engine_error)?;
            if !opened.coalesced {
                self.sources.reset_cancel(&item, &version);
            }
            if let Some(displaced) = opened.displaced {
                self.storage.remove_staging(&displaced.staging)?;
            }
            (opened.transfer, Some(opened.reader))
        } else {
            let wanted = ByteRange::new(0, extent).map_err(|error| DriveError::Internal {
                detail: format!("whole-object range is invalid: {error}"),
            })?;
            let sink = Box::new(ProgressSink {
                delivered: 0,
                total: extent,
                listener,
                token: Arc::clone(&token),
            });
            let opened = self
                .coordinator
                .open(
                    &mut store,
                    &item,
                    wanted,
                    Priority::FOREGROUND,
                    sink,
                    now_ms(),
                )
                .map_err(engine_error)?;
            if !opened.coalesced {
                self.sources.reset_cancel(&item, &version);
            }
            if let Some(displaced) = opened.displaced {
                self.storage.remove_staging(&displaced.staging)?;
            }
            (opened.transfer, Some(opened.reader))
        };
        #[cfg(test)]
        self.materialization_checkpoint(transfer);
        let materialization = bind_materialization(
            &self.materializations,
            transfer,
            &item,
            &version,
            &reservation,
        );
        let mut materialized = materialization.subscribe();
        self.spawn_driver();
        drop(_generation);
        let mut changes = self.changed.subscribe();

        loop {
            if token.is_cancelled() {
                self.cancel_reader(&mut store, &item, &version, transfer, reader)
                    .await?;
                return Err(cancelled());
            }

            if let Some(hit) = cached_file(
                &mut store,
                &item,
                &version,
                extent,
                now_ms(),
                token.as_ref(),
            )? {
                if let Some(reader) = reader {
                    let _ = self.coordinator.close(reader);
                }
                return Ok(self.adopt_cached_file(hit));
            }

            let row = {
                let read = store.read_txn().map_err(state_error)?;
                read.transfer(transfer).map_err(state_error)?
            }
            .ok_or_else(|| DriveError::Internal {
                detail: "hydration transfer disappeared".to_owned(),
            })?;
            match row.state {
                gramdrive_state::repo::TransferState::Failed => {
                    if let Some(reader) = reader {
                        let _ = self.coordinator.close(reader);
                    }
                    return Err(failure_error(
                        row.failure_category.unwrap_or(FailureCategory::Internal),
                    ));
                }
                gramdrive_state::repo::TransferState::Cancelled => {
                    if let Some(reader) = reader {
                        let _ = self.coordinator.close(reader);
                    }
                    return Err(cancelled());
                }
                gramdrive_state::repo::TransferState::Suspended => {
                    if let Some(reader) = reader {
                        let _ = self.coordinator.close(reader);
                    }
                    return Err(failure_error(
                        row.failure_category.unwrap_or(FailureCategory::Unavailable),
                    ));
                }
                gramdrive_state::repo::TransferState::Done => {
                    let result = materialized.borrow().clone();
                    match result {
                        Some(Ok(())) => {
                            if let Some(reader) = reader {
                                let _ = self.coordinator.close(reader);
                            }
                            return Err(DriveError::Integrity {
                                detail: "materialization completed without a verified cache entry"
                                    .to_owned(),
                            });
                        }
                        Some(Err(error)) => {
                            if let Some(reader) = reader {
                                let _ = self.coordinator.close(reader);
                            }
                            return Err(error);
                        }
                        None => {
                            tokio::select! {
                                () = token.cancelled() => {
                                    self.cancel_reader(
                                        &mut store,
                                        &item,
                                        &version,
                                        transfer,
                                        reader,
                                    ).await?;
                                    return Err(cancelled());
                                }
                                result = materialized.changed() => {
                                    if result.is_err() {
                                        if let Some(reader) = reader {
                                            let _ = self.coordinator.close(reader);
                                        }
                                        return Err(DriveError::Internal {
                                            detail: "materialization completion channel closed"
                                                .to_owned(),
                                        });
                                    }
                                }
                            }
                            continue;
                        }
                    }
                }
                gramdrive_state::repo::TransferState::Queued
                | gramdrive_state::repo::TransferState::Running => {}
            }

            if let Some(next_retry_at_ms) = row.next_retry_at_ms
                && next_retry_at_ms > now_ms()
            {
                tokio::select! {
                    () = token.cancelled() => {
                        self.cancel_reader(&mut store, &item, &version, transfer, reader).await?;
                        return Err(cancelled());
                    }
                    () = wait_until_unchecked(next_retry_at_ms) => self.spawn_driver(),
                    _ = changes.changed() => {}
                }
            } else {
                tokio::select! {
                    () = token.cancelled() => {
                        self.cancel_reader(&mut store, &item, &version, transfer, reader).await?;
                        return Err(cancelled());
                    }
                    _ = changes.changed() => {}
                }
            }
        }
    }

    async fn cancel_reader(
        &self,
        store: &mut StateStore,
        item: &ItemId,
        version: &ContentVersion,
        transfer: TransferId,
        reader: Option<gramdrive_engine::fetch::ReaderId>,
    ) -> Result<(), DriveError> {
        if self.begin_cancel_reader(store, item, version, transfer, reader)?
            == CancelDecision::Resolved
        {
            return Ok(());
        }

        let mut changes = self.changed.subscribe();
        loop {
            let row = {
                let read = store.read_txn().map_err(state_error)?;
                read.transfer(transfer).map_err(state_error)?
            };
            if row.is_none_or(|row| !row.state.is_live()) {
                self.storage.discard_transfer(transfer)?;
                return Ok(());
            }
            let _ = changes.changed().await;
        }
    }

    /// Atomically closes one reader and, if it was the last, binds durable
    /// cancellation to the source generation that owns `transfer`.
    ///
    /// This section is deliberately synchronous and per-content. A new open
    /// either attaches before the close (and prevents cancellation), or waits
    /// until the abandoned generation is durably marked and its exact source
    /// signal is cancelled. Network work and terminal-state waiting happen
    /// outside the admission lock.
    fn begin_cancel_reader(
        &self,
        store: &mut StateStore,
        item: &ItemId,
        version: &ContentVersion,
        transfer: TransferId,
        reader: Option<gramdrive_engine::fetch::ReaderId>,
    ) -> Result<CancelDecision, DriveError> {
        let admission = self.admission_for(item, version);
        let _admission = admission.lock().unwrap_or_else(|error| error.into_inner());
        let closed = reader.and_then(|reader| self.coordinator.close(reader));
        if reader.is_some() && closed.is_none() {
            // Delivery already resolved this reader. Its completed transfer
            // must not be retroactively cancelled because the caller raced
            // completion with closing its request.
            return Ok(CancelDecision::Resolved);
        }
        let remaining = closed.map_or(0, |closed| closed.remaining_readers);
        if remaining != 0 {
            return Ok(CancelDecision::Resolved);
        }
        #[cfg(test)]
        self.cancel_checkpoint(CancelCheckpoint::LastReaderClosed);
        let requested = self
            .coordinator
            .request_cancel(store, transfer, now_ms())
            .map_err(engine_error)?;
        if !requested {
            return Ok(CancelDecision::Resolved);
        }
        let state = {
            let read = store.read_txn().map_err(state_error)?;
            read.transfer(transfer)
                .map_err(state_error)?
                .map(|row| row.state)
        };
        if state.is_some_and(|state| state != gramdrive_state::repo::TransferState::Running) {
            let disposal = self
                .coordinator
                .machine()
                .acknowledge_requested_cancel(store, transfer, now_ms())
                .map_err(engine_error)?;
            if let Some(disposal) = disposal {
                self.storage.remove_staging(&disposal.staging)?;
            }
            self.storage.discard_transfer(transfer)?;
            self.changed.send_modify(|sequence| {
                *sequence = sequence.wrapping_add(1);
            });
            return Ok(CancelDecision::Resolved);
        }
        self.sources.cancel(item, version);
        #[cfg(test)]
        self.cancel_checkpoint(CancelCheckpoint::SourceGenerationCancelled);
        self.spawn_driver();
        Ok(CancelDecision::AwaitTerminal)
    }

    async fn thumbnail_inner(
        &self,
        account_id: i64,
        item_text: String,
        pinned_text: Option<String>,
        max_width_px: u32,
        max_height_px: u32,
        token: Arc<CancellationToken>,
    ) -> Result<Option<HydratedThumbnail>, DriveError> {
        if token.is_cancelled() {
            return Err(cancelled());
        }
        let item = ItemId::parse_text(&item_text).map_err(|error| DriveError::InvalidArgument {
            detail: format!("item id is malformed: {error}"),
        })?;
        if item_account_id(&item) != Some(account_id) {
            return Err(DriveError::NotFound {
                detail: "item does not belong to the requested account".to_owned(),
            });
        }
        let width =
            NonZeroU32::new(max_width_px.min(1024)).ok_or_else(|| DriveError::InvalidArgument {
                detail: "thumbnail width must be non-zero".to_owned(),
            })?;
        let height = NonZeroU32::new(max_height_px.min(1024)).ok_or_else(|| {
            DriveError::InvalidArgument {
                detail: "thumbnail height must be non-zero".to_owned(),
            }
        })?;
        let version = {
            let mut store = StateStore::open(&self.database).map_err(state_error)?;
            thumbnail_admission(
                &mut store,
                AccountId(account_id),
                &item,
                pinned_text.as_deref(),
            )?
        };
        let thumbnail = tokio::select! {
            result = self.sources.thumbnail(
                item.clone(),
                ThumbnailSpec { max_width_px: width, max_height_px: height },
            ) => result.map_err(source_error)?,
            () = token.cancelled() => return Err(cancelled()),
        };
        let Some(thumbnail) = thumbnail else {
            return Ok(None);
        };
        let path = self.storage.publish_thumbnail(
            &item,
            &version,
            width.get(),
            height.get(),
            thumbnail.bytes(),
        )?;
        Ok(Some(HydratedThumbnail {
            path: path.to_string_lossy().into_owned(),
            content_version: version.as_str().to_owned(),
            mime_type: thumbnail.mime_type().to_owned(),
            byte_count: u64::try_from(thumbnail.bytes().len()).unwrap_or(u64::MAX),
        }))
    }
}

async fn drive_queue(driver: &HydrationDriver) -> Result<(), DriveError> {
    let mut store = StateStore::open(&driver.database).map_err(state_error)?;
    let mut staging = FileStagingHost::new(driver.storage.staging_dir.clone())?;
    let mut promotion = FilePromotionHost::new(
        driver.storage.staging_dir.clone(),
        driver.storage.blob_dir.clone(),
    )?;
    let mut promoter = Promoter::default();

    loop {
        #[cfg(test)]
        if let Some(probe) = &driver.driver_probe {
            probe.before_claim();
        }
        let outcome = driver
            .coordinator
            .run_next(
                &mut store,
                driver.sources.as_ref(),
                &mut staging,
                &SystemClock,
            )
            .await
            .map_err(engine_error)?;
        let RunOutcome::Ran(report) = outcome else {
            return Ok(());
        };
        for disposal in report.disposals {
            driver.storage.remove_staging(&disposal.staging)?;
        }
        if matches!(&report.end, AttemptEnd::Promoted { .. }) {
            let (item, version) = {
                let read = store.read_txn().map_err(state_error)?;
                let transfer = read
                    .transfer(report.transfer)
                    .map_err(state_error)?
                    .ok_or_else(|| DriveError::Internal {
                        detail: "completed hydration transfer disappeared".to_owned(),
                    })?;
                (transfer.item, transfer.content_version)
            };
            let materialization = producer_materialization(
                &driver.materializations,
                report.transfer,
                &item,
                &version,
            );
            #[cfg(test)]
            if let Some(probe) = &driver.promotion_probe {
                probe.before_publication(report.transfer);
            }
            let result = promote_transfer(
                &mut store,
                &mut staging,
                &mut promotion,
                &mut promoter,
                &driver.storage,
                report.transfer,
            );
            materialization.resolve(result.clone());
            retain_materialization_until_expiry(
                &driver.materializations,
                report.transfer,
                &materialization,
            );
            result?;
        } else if !matches!(&report.end, AttemptEnd::Requeued { .. }) {
            // Terminal and parked attempts cannot later publish this transfer.
            // Requeued work retains its causal completion across backoff.
            forget_materialization(&driver.materializations, report.transfer);
        }
        driver.changed.send_modify(|sequence| {
            *sequence = sequence.wrapping_add(1);
        });
    }
}

fn materialization_key(item: &ItemId, version: &ContentVersion) -> ContentGenerationKey {
    (item.text().to_owned(), version.as_str().to_owned())
}

fn reserve_materialization(
    registry: &MaterializationRegistry,
    item: &ItemId,
    version: &ContentVersion,
) -> Arc<MaterializationCompletion> {
    let mut registry = registry.lock().unwrap_or_else(|error| error.into_inner());
    registry
        .pending
        .retain(|_, completion| completion.strong_count() != 0);
    let completion = Arc::new(MaterializationCompletion::default());
    registry.pending.insert(
        materialization_key(item, version),
        Arc::downgrade(&completion),
    );
    completion
}

fn bind_materialization(
    registry: &MaterializationRegistry,
    transfer: TransferId,
    item: &ItemId,
    version: &ContentVersion,
    reservation: &Arc<MaterializationCompletion>,
) -> Arc<MaterializationCompletion> {
    let key = materialization_key(item, version);
    let reservation_weak = Arc::downgrade(reservation);
    let mut registry = registry.lock().unwrap_or_else(|error| error.into_inner());
    let completion = registry
        .by_transfer
        .get(&transfer)
        .cloned()
        .unwrap_or_else(|| Arc::clone(reservation));
    registry
        .by_transfer
        .insert(transfer, Arc::clone(&completion));
    if registry
        .pending
        .get(&key)
        .is_some_and(|pending| pending.ptr_eq(&reservation_weak))
    {
        registry.pending.remove(&key);
    }
    completion
}

fn producer_materialization(
    registry: &MaterializationRegistry,
    transfer: TransferId,
    item: &ItemId,
    version: &ContentVersion,
) -> Arc<MaterializationCompletion> {
    let key = materialization_key(item, version);
    let mut registry = registry.lock().unwrap_or_else(|error| error.into_inner());
    let completion = if let Some(bound) = registry.by_transfer.get(&transfer) {
        Arc::clone(bound)
    } else {
        registry
            .pending
            .get(&key)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(MaterializationCompletion::default()))
    };
    registry
        .by_transfer
        .insert(transfer, Arc::clone(&completion));
    completion
}

fn retain_materialization_until_expiry(
    registry: &MaterializationRegistry,
    transfer: TransferId,
    completion: &Arc<MaterializationCompletion>,
) {
    let registry = Arc::downgrade(registry);
    let completion = Arc::downgrade(completion);
    drop(tokio::spawn(async move {
        tokio::time::sleep(MATERIALIZATION_RESULT_RETENTION).await;
        if let Some(registry) = registry.upgrade() {
            expire_materialization(&registry, transfer, &completion);
        }
    }));
}

fn expire_materialization(
    registry: &MaterializationRegistry,
    transfer: TransferId,
    completion: &Weak<MaterializationCompletion>,
) {
    let mut registry = registry.lock().unwrap_or_else(|error| error.into_inner());
    if registry
        .by_transfer
        .get(&transfer)
        .is_some_and(|bound| completion.ptr_eq(&Arc::downgrade(bound)) && bound.is_resolved())
    {
        registry.by_transfer.remove(&transfer);
    }
    registry
        .pending
        .retain(|_, pending| !pending.ptr_eq(completion));
}

fn forget_materialization(registry: &MaterializationRegistry, transfer: TransferId) {
    let mut registry = registry.lock().unwrap_or_else(|error| error.into_inner());
    let forgotten = registry.by_transfer.remove(&transfer);
    if let Some(forgotten) = forgotten {
        let forgotten = Arc::downgrade(&forgotten);
        registry
            .pending
            .retain(|_, pending| !pending.ptr_eq(&forgotten));
    } else {
        registry
            .pending
            .retain(|_, pending| pending.strong_count() != 0);
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Hydrator {
    /// Materializes one attachment through its owned account session.
    pub async fn hydrate(
        self: Arc<Self>,
        account_id: i64,
        item_id: String,
        content_version: Option<String>,
        listener: Arc<dyn ProgressListener>,
        token: Arc<CancellationToken>,
    ) -> Result<HydratedFile, DriveError> {
        // UniFFI polls this exported future on a Swift cooperative executor.
        // Submit the entire hydration to the owner's independent runtime. The
        // wrapper below only awaits the join handle, so neither SQLite nor
        // filesystem work can occupy the Swift executor or async-compat's
        // fallback current-thread runtime.
        let runtime = Arc::clone(&self.runtime);
        let handle = runtime.handle();
        handle
            .spawn(async move {
                let _runtime = runtime;
                self.hydrate_inner(account_id, item_id, content_version, listener, token)
                    .await
            })
            .await
            .map_err(|error| DriveError::Internal {
                detail: format!("hydration task terminated: {error}"),
            })?
    }

    /// Releases the opaque hand-off lease emitted with a generated document
    /// after its native File Provider clone finishes (or its connection is
    /// cancelled/disconnected). Unknown ids are intentionally harmless so a
    /// teardown racing a prior release remains bounded and idempotent.
    pub fn release_hydration_lease(&self, lease_id: String) -> Result<(), DriveError> {
        let lease = self
            .staged_leases
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&lease_id);
        let Some(lease) = lease else { return Ok(()) };
        let base = lease
            .path()
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        drop(lease);
        if let Some(base) = base {
            let mut store = StateStore::open(&self.database).map_err(state_error)?;
            reclaim_unreferenced_generations(&mut store, &base).map_err(|error| {
                DriveError::Storage {
                    detail: format!("could not reclaim released generated hydration: {error}"),
                }
            })?;
        }
        Ok(())
    }

    /// Fetches only the source's dedicated bounded preview for one attachment.
    pub async fn thumbnail(
        self: Arc<Self>,
        account_id: i64,
        item_id: String,
        content_version: Option<String>,
        max_width_px: u32,
        max_height_px: u32,
        token: Arc<CancellationToken>,
    ) -> Result<Option<HydratedThumbnail>, DriveError> {
        // This exported future is polled by UniFFI's async-compat runtime,
        // which can be a single current-thread executor owned by Swift.
        // Thumbnail admission opens SQLite and publication touches the
        // filesystem, so submit the whole operation to the Hydrator-owned
        // multi-thread runtime just as full-content hydration does.
        let runtime = Arc::clone(&self.runtime);
        let handle = runtime.handle();
        handle
            .spawn(async move {
                let _runtime = runtime;
                self.thumbnail_inner(
                    account_id,
                    item_id,
                    content_version,
                    max_width_px,
                    max_height_px,
                    token,
                )
                .await
            })
            .await
            .map_err(|error| DriveError::Internal {
                detail: format!("thumbnail task terminated: {error}"),
            })?
    }
}

#[derive(Default)]
struct RoutingSource {
    sources: RwLock<HashMap<i64, RegisteredSource>>,
    cancellations: Mutex<HashMap<ContentGenerationKey, Arc<CancellationSignal>>>,
}

struct RegisteredSource {
    token: u64,
    source: Arc<dyn ContentSource>,
    thumbnailer: Option<Arc<dyn ThumbnailServing>>,
}

/// The dedicated preview half of an account's TDLib session.
///
/// Content hydration intentionally uses the narrower `ContentSource` port,
/// while thumbnails use TDLib's distinct, byte-bounded preview driver. Keeping
/// that split here makes the production ownership explicit and lets the
/// exported thumbnail boundary be exercised with a deterministic driver in
/// unit tests without changing its scheduling path.
trait ThumbnailServing: Send + Sync {
    fn thumbnail(&self, item: ItemId, spec: ThumbnailSpec) -> SourceFuture<'_, Option<Thumbnail>>;
}

impl ThumbnailServing for TdThumbnailer {
    fn thumbnail(&self, item: ItemId, spec: ThumbnailSpec) -> SourceFuture<'_, Option<Thumbnail>> {
        TdThumbnailer::thumbnail(self, item, spec)
    }
}

impl RoutingSource {
    fn cancellation_key(item: &ItemId, version: &ContentVersion) -> ContentGenerationKey {
        (item.text().to_owned(), version.as_str().to_owned())
    }

    fn signal(&self, item: &ItemId, version: &ContentVersion) -> Arc<CancellationSignal> {
        let mut cancellations = self
            .cancellations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Arc::clone(
            cancellations
                .entry(Self::cancellation_key(item, version))
                .or_insert_with(|| Arc::new(CancellationSignal::default())),
        )
    }

    fn reset_cancel(&self, item: &ItemId, version: &ContentVersion) {
        self.cancellations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&Self::cancellation_key(item, version));
    }

    fn cancel(&self, item: &ItemId, version: &ContentVersion) {
        self.signal(item, version).cancel();
    }

    #[cfg(test)]
    fn register(&self, account_id: i64, token: u64, source: Arc<dyn ContentSource>) {
        self.sources
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                account_id,
                RegisteredSource {
                    token,
                    source,
                    thumbnailer: None,
                },
            );
    }

    fn register_with_thumbnail(
        &self,
        account_id: i64,
        token: u64,
        source: Arc<dyn ContentSource>,
        thumbnailer: Arc<dyn ThumbnailServing>,
    ) {
        self.sources
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                account_id,
                RegisteredSource {
                    token,
                    source,
                    thumbnailer: Some(thumbnailer),
                },
            );
    }

    fn unregister(&self, account_id: i64, token: u64) {
        let mut sources = self
            .sources
            .write()
            .unwrap_or_else(|error| error.into_inner());
        if sources
            .get(&account_id)
            .is_some_and(|source| source.token == token)
        {
            sources.remove(&account_id);
        }
    }

    fn has_account(&self, account_id: i64) -> bool {
        self.sources
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(&account_id)
    }

    async fn thumbnail(
        &self,
        item: ItemId,
        spec: ThumbnailSpec,
    ) -> Result<Option<Thumbnail>, SourceError> {
        let thumbnailer = item_account_id(&item).and_then(|account| {
            self.sources
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .get(&account)
                .and_then(|entry| entry.thumbnailer.as_ref().map(Arc::clone))
        });
        let thumbnailer = thumbnailer.ok_or_else(|| SourceError::AuthRequired {
            detail: "the account's owned Telegram session is unavailable".to_owned(),
        })?;
        thumbnailer.thumbnail(item, spec).await
    }
}

impl ContentSource for RoutingSource {
    fn fetch<'a>(
        &'a self,
        request: FetchRequest,
        sink: &'a mut dyn ContentSink,
    ) -> SourceFuture<'a, ()> {
        let signal = self.signal(&request.item, &request.version);
        let source = item_account_id(&request.item).and_then(|account| {
            self.sources
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .get(&account)
                .map(|entry| Arc::clone(&entry.source))
        });
        Box::pin(async move {
            let source = source.ok_or_else(|| SourceError::AuthRequired {
                detail: "the account's owned Telegram session is unavailable".to_owned(),
            })?;
            let fetch = source.fetch(request, sink);
            tokio::pin!(fetch);
            tokio::select! {
                result = &mut fetch => result,
                () = signal.cancelled() => Err(SourceError::Cancelled {
                    detail: "the last hydration reader cancelled".to_owned(),
                }),
            }
        })
    }
}

struct CancellationSignal {
    cancelled: tokio::sync::watch::Sender<bool>,
}

impl Default for CancellationSignal {
    fn default() -> Self {
        let (cancelled, _) = tokio::sync::watch::channel(false);
        Self { cancelled }
    }
}

impl CancellationSignal {
    fn cancel(&self) {
        self.cancelled.send_replace(true);
    }

    async fn cancelled(&self) {
        let mut cancelled = self.cancelled.subscribe();
        let _ = cancelled.wait_for(|cancelled| *cancelled).await;
    }
}

struct StateFetchCatalog {
    store: Mutex<StateStore>,
    account: AccountId,
}

struct StateThumbnailCatalog {
    store: Mutex<StateStore>,
    account: AccountId,
}

impl StateThumbnailCatalog {
    fn open(database: &str, account: AccountId) -> Result<Self, DriveError> {
        Ok(Self {
            store: Mutex::new(StateStore::open(database).map_err(state_error)?),
            account,
        })
    }
}

impl ThumbnailCatalog for StateThumbnailCatalog {
    fn resolve(&self, item: &ItemId) -> Option<ThumbnailTarget> {
        let mut store = self.store.lock().ok()?;
        let read = store.read_txn().ok()?;
        let (key, availability) = attachment_eligibility(&read, self.account, item).ok()??;
        let payload = read.current_message_payload(&key.message).ok()??;
        if payload.schema != NORMALIZED_MESSAGE_SCHEMA_FAMILY {
            return None;
        }
        let record: MessageRecord = serde_json::from_slice(&payload.bytes).ok()?;
        let mapped = map_message_attachments(&record, key.message.chat.scope)
            .into_iter()
            .find(|attachment| attachment.key == key)?;
        let mut target = ThumbnailTarget::from_descriptor(&mapped.descriptor);
        target.availability = availability;
        Some(target)
    }
}

impl StateFetchCatalog {
    fn open(database: &str, account: AccountId) -> Result<Self, DriveError> {
        Ok(Self {
            store: Mutex::new(StateStore::open(database).map_err(state_error)?),
            account,
        })
    }

    fn content_key(&self, item: &ItemId) -> Option<HydrationContentKey> {
        hydration_content_key_for_account(item, self.account)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HydrationContentKey {
    Attachment(AttachmentKey),
    Story(gramdrive_model::identity::StoryKey),
}

fn attachment_key_for_account(item: &ItemId, account: AccountId) -> Option<AttachmentKey> {
    if item_account_id(item) != Some(account.0) {
        return None;
    }
    match item.key() {
        ItemKey::Canonical(CanonicalKey::Attachment(key))
        | ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
            item: CanonicalKey::Attachment(key),
            ..
        }) => Some(key),
        _ => None,
    }
}

fn hydration_content_key_for_account(
    item: &ItemId,
    account: AccountId,
) -> Option<HydrationContentKey> {
    if let Some(key) = attachment_key_for_account(item, account) {
        return Some(HydrationContentKey::Attachment(key));
    }
    if item_account_id(item) != Some(account.0) {
        return None;
    }
    match item.key() {
        ItemKey::Canonical(CanonicalKey::Story(key)) => Some(HydrationContentKey::Story(key)),
        ItemKey::StoryAppearance(appearance) => Some(HydrationContentKey::Story(appearance.story)),
        _ => None,
    }
}

/// Resolves every persisted policy fact that gates attachment bytes.
///
/// Both the pre-cache admission path and the TDLib catalog use this one
/// resolver, so a verified object cannot bypass a restriction learned after
/// it was materialized and a source fetch cannot observe weaker policy than a
/// cache hit.
fn attachment_eligibility(
    read: &gramdrive_state::repo::ReadTxn<'_>,
    account: AccountId,
    item: &ItemId,
) -> Result<Option<(AttachmentKey, SourceAvailability)>, gramdrive_state::StateError> {
    let Some(item_record) = read.item(item)? else {
        return Ok(None);
    };
    if item_record.deleted_at_ms.is_some() || item_record.content.is_none() {
        return Ok(None);
    }
    let Some(key) = attachment_key_for_account(item, account) else {
        return Ok(None);
    };
    let Some(attachment) = read.attachment(&key)? else {
        return Ok(None);
    };
    let Some(chat) = read.chat(&key.message.chat)? else {
        return Ok(None);
    };
    let facts = attachment.facts;
    let availability = if chat.is_protected
        || !facts.can_be_saved
        || item_record.availability == StateItemAvailability::Restricted
        || facts.availability == StateAttachmentAvailability::Restricted
    {
        SourceAvailability::Restricted
    } else if chat.deleted_at_ms.is_some()
        || chat.left_at_ms.is_some()
        || item_record.availability == StateItemAvailability::Unavailable
        || facts.availability == StateAttachmentAvailability::Unavailable
    {
        SourceAvailability::Unavailable
    } else if facts.availability == StateAttachmentAvailability::ViewOnce {
        SourceAvailability::ViewOnce
    } else {
        SourceAvailability::Fetchable
    };
    Ok(Some((key, availability)))
}

fn story_eligibility(
    read: &gramdrive_state::repo::ReadTxn<'_>,
    account: AccountId,
    item: &ItemId,
) -> Result<
    Option<(gramdrive_model::identity::StoryKey, SourceAvailability)>,
    gramdrive_state::StateError,
> {
    let Some(item_record) = read.item(item)? else {
        return Ok(None);
    };
    if item_record.deleted_at_ms.is_some() || item_record.content.is_none() {
        return Ok(None);
    }
    let Some(HydrationContentKey::Story(key)) = hydration_content_key_for_account(item, account)
    else {
        return Ok(None);
    };
    let Some(story) = read.story(&key)? else {
        return Ok(None);
    };
    let Some(chat) = read.chat(&key.poster)? else {
        return Ok(None);
    };
    let removed = match item.key() {
        ItemKey::StoryAppearance(appearance) => read
            .story_appearances(&key)?
            .into_iter()
            .find(|candidate| candidate.location == appearance.location)
            .is_none_or(|appearance| appearance.removed_at_ms.is_some()),
        _ => false,
    };
    let facts = story.facts;
    let availability = if chat.is_protected
        || !facts.can_be_forwarded
        || item_record.availability == StateItemAvailability::Restricted
        || facts.availability == StateAttachmentAvailability::Restricted
        || facts.content_state == StoryContentState::Protected
    {
        SourceAvailability::Restricted
    } else if removed
        || chat.deleted_at_ms.is_some()
        || chat.left_at_ms.is_some()
        || item_record.availability == StateItemAvailability::Unavailable
        || facts.availability == StateAttachmentAvailability::Unavailable
        || facts.content_state != StoryContentState::Available
    {
        SourceAvailability::Unavailable
    } else {
        SourceAvailability::Fetchable
    };
    Ok(Some((key, availability)))
}

fn content_eligibility(
    read: &gramdrive_state::repo::ReadTxn<'_>,
    account: AccountId,
    item: &ItemId,
) -> Result<Option<(HydrationContentKey, SourceAvailability)>, gramdrive_state::StateError> {
    match hydration_content_key_for_account(item, account) {
        Some(HydrationContentKey::Attachment(_)) => {
            Ok(attachment_eligibility(read, account, item)?
                .map(|(key, availability)| (HydrationContentKey::Attachment(key), availability)))
        }
        Some(HydrationContentKey::Story(_)) => Ok(story_eligibility(read, account, item)?
            .map(|(key, availability)| (HydrationContentKey::Story(key), availability))),
        None => Ok(None),
    }
}

impl FetchCatalog for StateFetchCatalog {
    fn resolve(&self, item: &ItemId) -> Option<CatalogEntry> {
        let mut store = self.store.lock().ok()?;
        let read = store.read_txn().ok()?;
        let item_record = read.item(item).ok()??;
        if item_record.deleted_at_ms.is_some() {
            return None;
        }
        if item_record.content.is_none() {
            return Some(CatalogEntry::Directory);
        }
        let (key, availability) = content_eligibility(&read, self.account, item).ok()??;
        match key {
            HydrationContentKey::Attachment(key) => {
                let attachment = read.attachment(&key).ok()??;
                let facts = attachment.facts;
                let file_id = facts.telegram_local_file_id?;
                let remote_file_type = attachment_remote_file_type(&facts.telegram_representation)?;
                Some(CatalogEntry::File(FileTarget {
                    file_id,
                    remote_id: facts.telegram_file_id,
                    remote_file_type: Some(remote_file_type),
                    refresh: RefreshTarget::Message {
                        chat_id: key.message.chat.chat_id.0,
                        message_id: key.message.message_id.0,
                    },
                    availability,
                    remote_unique_id: facts.telegram_unique_id,
                    size: facts.exact_size,
                    version: facts.content_version,
                }))
            }
            HydrationContentKey::Story(key) => {
                let story = read.story(&key).ok()??;
                let locator = story
                    .locators
                    .into_iter()
                    .find(|locator| locator.is_primary)?;
                let file_id = locator.local_file_id?;
                let remote_file_type = story_remote_file_type(locator.file_type);
                Some(CatalogEntry::File(FileTarget {
                    file_id,
                    remote_id: locator.remote_file_id,
                    remote_file_type: Some(remote_file_type),
                    refresh: RefreshTarget::Story {
                        poster_chat_id: key.poster.chat_id.0,
                        story_id: key.story_id.0,
                    },
                    availability,
                    remote_unique_id: locator.remote_unique_id,
                    size: locator.size.or(locator.expected_size),
                    version: story.facts.content_version,
                }))
            }
        }
    }

    fn persist_refresh(
        &self,
        item: &ItemId,
        refresh: &RefreshedFileTarget,
    ) -> Result<(), SourceError> {
        let key = self
            .content_key(item)
            .ok_or_else(|| SourceError::NotFound {
                detail: "refreshed item is not hydratable content in this account".to_owned(),
            })?;
        let mut store = self.store.lock().map_err(|_| SourceError::Internal {
            detail: "state catalog lock is poisoned".to_owned(),
        })?;
        let tx = store.write_txn().map_err(source_state_error)?;
        match key {
            HydrationContentKey::Attachment(key) => {
                let state = tx
                    .read()
                    .attachment(&key)
                    .map_err(source_state_error)?
                    .ok_or_else(|| SourceError::NotFound {
                        detail: "attachment disappeared during locator refresh".to_owned(),
                    })?;
                verify_persisted_refresh(
                    state.facts.telegram_local_file_id,
                    state.facts.telegram_unique_id.as_deref(),
                    state.facts.exact_size,
                    &state.facts.content_version,
                    refresh,
                )?;
                let mut facts = state.facts;
                facts.telegram_local_file_id = Some(refresh.file_id);
                facts.telegram_file_id = refresh.remote_id.clone();
                facts.telegram_unique_id = refresh.remote_unique_id.clone();
                facts.exact_size = refresh.size;
                tx.upsert_attachment(&facts).map_err(source_state_error)?;
            }
            HydrationContentKey::Story(key) => {
                let state = tx
                    .read()
                    .story(&key)
                    .map_err(source_state_error)?
                    .ok_or_else(|| SourceError::NotFound {
                        detail: "story disappeared during locator refresh".to_owned(),
                    })?;
                let mut locators = state.locators;
                let primary = locators
                    .iter_mut()
                    .find(|locator| locator.is_primary)
                    .ok_or_else(|| SourceError::NotFound {
                        detail: "story primary locator disappeared during refresh".to_owned(),
                    })?;
                verify_persisted_refresh(
                    primary.local_file_id,
                    primary.remote_unique_id.as_deref(),
                    primary.size.or(primary.expected_size),
                    &state.facts.content_version,
                    refresh,
                )?;
                primary.local_file_id = Some(refresh.file_id);
                primary.remote_file_id = refresh.remote_id.clone();
                primary.remote_unique_id = refresh.remote_unique_id.clone();
                primary.size = refresh.size;
                tx.upsert_story_with_locators(&state.facts, &locators)
                    .map_err(source_state_error)?;
            }
        }
        tx.commit().map_err(source_state_error)
    }
}

fn attachment_remote_file_type(
    representation: &gramdrive_state::repo::TelegramRepresentation,
) -> Option<RemoteFileType> {
    use gramdrive_state::repo::TelegramRepresentation;
    match representation {
        TelegramRepresentation::OriginalDocument => Some(RemoteFileType::Document),
        TelegramRepresentation::Photo => Some(RemoteFileType::Photo),
        TelegramRepresentation::Video => Some(RemoteFileType::Video),
        TelegramRepresentation::Animation => Some(RemoteFileType::Animation),
        TelegramRepresentation::Audio => Some(RemoteFileType::Audio),
        TelegramRepresentation::Voice => Some(RemoteFileType::VoiceNote),
        TelegramRepresentation::VideoNote => Some(RemoteFileType::VideoNote),
        TelegramRepresentation::Sticker => Some(RemoteFileType::Sticker),
        TelegramRepresentation::UnknownLegacy | TelegramRepresentation::Other(_) => None,
    }
}

fn story_remote_file_type(
    file_type: gramdrive_state::repo::StoryLocatorFileType,
) -> RemoteFileType {
    use gramdrive_state::repo::StoryLocatorFileType;
    match file_type {
        StoryLocatorFileType::PhotoStory => RemoteFileType::PhotoStory,
        StoryLocatorFileType::VideoStory => RemoteFileType::VideoStory,
        StoryLocatorFileType::Thumbnail => RemoteFileType::Thumbnail,
    }
}

fn verify_persisted_refresh(
    _file_id: Option<i32>,
    remote_unique_id: Option<&str>,
    exact_size: Option<u64>,
    version: &ContentVersion,
    refresh: &RefreshedFileTarget,
) -> Result<(), SourceError> {
    if refresh.file_id <= 0 {
        return Err(SourceError::Internal {
            detail: "locator refresh returned no usable process-local file id".to_owned(),
        });
    }
    if let Some(before) = remote_unique_id
        && refresh.remote_unique_id.as_deref() != Some(before)
    {
        return Err(SourceError::VersionConflict {
            current: Some(version.clone()),
            detail: "locator refresh failed to preserve stable content identity".to_owned(),
        });
    }
    if let Some(before) = exact_size
        && refresh.size != Some(before)
    {
        return Err(SourceError::VersionConflict {
            current: Some(version.clone()),
            detail: "locator refresh failed to preserve the pinned content extent".to_owned(),
        });
    }
    if !refresh.can_be_saved || refresh.availability != SourceAvailability::Fetchable {
        return Err(SourceError::Restricted {
            detail: "locator refresh observed content that may no longer be saved".to_owned(),
        });
    }
    Ok(())
}

struct ProgressSink {
    delivered: u64,
    total: u64,
    listener: Arc<dyn ProgressListener>,
    token: Arc<CancellationToken>,
}

impl ContentSink for ProgressSink {
    fn accept(&mut self, chunk: ContentChunk<'_>) -> SinkControl {
        if self.token.is_cancelled() {
            return SinkControl::Stop;
        }
        self.delivered = self.delivered.saturating_add(chunk.len()).min(self.total);
        self.listener.on_progress(TransferProgress {
            bytes_transferred: self.delivered,
            bytes_total: Some(self.total),
        });
        SinkControl::Continue
    }
}

#[derive(Debug)]
struct FileStaging {
    handle: String,
    file: Mutex<File>,
}

impl Staging for FileStaging {
    fn handle(&self) -> &str {
        &self.handle
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), StagingError> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| staging_failed("staging lock poisoned"))?;
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.write_all(bytes))
            .and_then(|_| file.sync_data())
            .map_err(|error| staging_io("write", error))
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), StagingError> {
        let mut file = self
            .file
            .lock()
            .map_err(|_| staging_failed("staging lock poisoned"))?;
        file.seek(SeekFrom::Start(offset))
            .and_then(|_| file.read_exact(buffer))
            .map_err(|error| staging_io("read", error))
    }
}

struct FileStagingHost {
    root: PathBuf,
}

impl FileStagingHost {
    fn new(root: PathBuf) -> Result<Self, DriveError> {
        fs::create_dir_all(&root).map_err(file_storage_error)?;
        Ok(Self { root })
    }

    fn checked(&self, path: &str) -> Result<PathBuf, StagingError> {
        let path = PathBuf::from(path);
        if path.parent() != Some(self.root.as_path()) {
            return Err(staging_failed(
                "staging handle is outside the owned directory",
            ));
        }
        Ok(path)
    }
}

impl StagingHost for FileStagingHost {
    fn open(
        &mut self,
        transfer: TransferId,
        existing: Option<&str>,
    ) -> Result<Box<dyn Staging>, StagingError> {
        let path = match existing {
            Some(handle) => self.checked(handle)?,
            None => self.root.join(format!("transfer-{}.partial", transfer.0)),
        };
        let file = OpenOptions::new()
            .create(existing.is_none())
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| staging_io("open", error))?;
        Ok(Box::new(FileStaging {
            handle: path.to_string_lossy().into_owned(),
            file: Mutex::new(file),
        }))
    }
}

struct FilePromotionHost {
    staging_root: PathBuf,
    root: PathBuf,
}

impl FilePromotionHost {
    fn new(staging_root: PathBuf, root: PathBuf) -> Result<Self, DriveError> {
        fs::create_dir_all(&root).map_err(file_storage_error)?;
        Ok(Self { staging_root, root })
    }

    fn checked_staging(&self, reference: &str) -> Result<PathBuf, PromotionHostError> {
        let path = PathBuf::from(reference);
        if path.parent() != Some(self.staging_root.as_path()) {
            return Err(PromotionHostError::new(
                "promotion source escaped the owned staging directory",
            ));
        }
        Ok(path)
    }
}

impl PromotionHost for FilePromotionHost {
    fn promote(
        &mut self,
        staging: Option<&str>,
        hash: &ContentHash,
    ) -> Result<Materialization, PromotionHostError> {
        let target = self.root.join(hash_hex(hash));
        let staging = staging.map(|path| self.checked_staging(path)).transpose()?;
        if target.exists() {
            if let Some(staging) = &staging {
                remove_file_if_exists(staging).map_err(promotion_io)?;
            }
            return Ok(Materialization {
                reference: target.to_string_lossy().into_owned(),
                deduplicated: true,
            });
        }
        let source = match staging {
            Some(path) => path,
            None => {
                let empty = self.root.join(format!(".{}.partial", hash_hex(hash)));
                File::create(&empty)
                    .and_then(|file| file.sync_all())
                    .map_err(promotion_io)?;
                empty
            }
        };
        OpenOptions::new()
            .read(true)
            .open(&source)
            .and_then(|file| file.sync_all())
            .map_err(promotion_io)?;
        fs::rename(&source, &target).map_err(promotion_io)?;
        File::open(&self.root)
            .and_then(|directory| directory.sync_all())
            .map_err(promotion_io)?;
        Ok(Materialization {
            reference: target.to_string_lossy().into_owned(),
            deduplicated: false,
        })
    }
}

#[derive(Debug, Clone)]
struct FileStorage {
    staging_dir: PathBuf,
    blob_dir: PathBuf,
    generated_dir: PathBuf,
    thumbnail_dir: PathBuf,
    next_thumbnail: Arc<std::sync::atomic::AtomicU64>,
}

impl FileStorage {
    fn new(root: PathBuf) -> Result<Self, DriveError> {
        let staging_dir = root.join("transfers");
        let blob_dir = root.join("blobs").join("sha256");
        let generated_dir = root.join("generated");
        let thumbnail_dir = root.join("thumbnails");
        fs::create_dir_all(&staging_dir).map_err(file_storage_error)?;
        fs::create_dir_all(&blob_dir).map_err(file_storage_error)?;
        fs::create_dir_all(&generated_dir).map_err(file_storage_error)?;
        fs::create_dir_all(&thumbnail_dir).map_err(file_storage_error)?;
        Ok(Self {
            staging_dir,
            blob_dir,
            generated_dir,
            thumbnail_dir,
            next_thumbnail: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        })
    }

    fn publish_thumbnail(
        &self,
        item: &ItemId,
        version: &ContentVersion,
        width: u32,
        height: u32,
        bytes: &[u8],
    ) -> Result<PathBuf, DriveError> {
        let stem = format!(
            "{}-{}-{width}x{height}",
            hex_bytes(item.text().as_bytes()),
            hex_bytes(version.as_str().as_bytes())
        );
        let destination = self.thumbnail_dir.join(format!("{stem}.preview"));
        // Preview locators may refresh independently of the pinned main
        // content version. Reuse is therefore valid only for identical bytes;
        // equal length alone can otherwise preserve a stale preview forever,
        // including across agent relaunches.
        if fs::read(&destination).is_ok_and(|published| published == bytes) {
            return Ok(destination);
        }
        let sequence = self.next_thumbnail.fetch_add(1, Ordering::Relaxed);
        let temporary = self
            .thumbnail_dir
            .join(format!(".{stem}.{sequence}.partial"));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(file_storage_error)?;
            file.write_all(bytes).map_err(file_storage_error)?;
            file.sync_all().map_err(file_storage_error)?;
            fs::rename(&temporary, &destination).map_err(file_storage_error)?;
            File::open(&self.thumbnail_dir)
                .and_then(|directory| directory.sync_all())
                .map_err(file_storage_error)?;
            Ok(destination.clone())
        })();
        if result.is_err() {
            let _ = remove_file_if_exists(&temporary);
        }
        result
    }

    fn remove_staging(&self, reference: &str) -> Result<(), DriveError> {
        let path = PathBuf::from(reference);
        if path.parent() != Some(self.staging_dir.as_path()) {
            return Err(DriveError::Storage {
                detail: "staging disposal escaped the owned cache directory".to_owned(),
            });
        }
        remove_file_if_exists(&path).map_err(file_storage_error)
    }

    fn discard_transfer(&self, transfer: TransferId) -> Result<(), DriveError> {
        remove_file_if_exists(
            &self
                .staging_dir
                .join(format!("transfer-{}.partial", transfer.0)),
        )
        .map_err(file_storage_error)
    }

    fn objects(&self, directory: &Path) -> Result<Vec<StoredObject>, StorageError> {
        let mut objects = Vec::new();
        for entry in fs::read_dir(directory).map_err(storage_io)? {
            let entry = entry.map_err(storage_io)?;
            let metadata = entry.metadata().map_err(storage_io)?;
            if metadata.is_file() {
                objects.push(StoredObject {
                    reference: entry.path().to_string_lossy().into_owned(),
                    size: metadata.len(),
                });
            }
        }
        Ok(objects)
    }

    fn generated_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        let mut objects = Vec::new();
        let mut pending = vec![self.generated_dir.clone()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).map_err(storage_io)? {
                let entry = entry.map_err(storage_io)?;
                let file_type = entry.file_type().map_err(storage_io)?;
                if file_type.is_dir() {
                    pending.push(entry.path());
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let known_document = entry.file_name().to_str().is_some_and(|name| {
                    matches!(
                        name,
                        "Messages.md" | "Messages.ndjson" | ".chat.json" | "chat.json"
                    )
                });
                if known_document {
                    let metadata = entry.metadata().map_err(storage_io)?;
                    objects.push(StoredObject {
                        reference: entry.path().to_string_lossy().into_owned(),
                        size: metadata.len(),
                    });
                }
            }
        }
        objects.sort_by(|left, right| left.reference.cmp(&right.reference));
        Ok(objects)
    }

    fn prune_empty_generated_parents(&self, path: &Path) {
        let mut parent = path.parent();
        while let Some(directory) = parent {
            if directory == self.generated_dir {
                break;
            }
            if fs::remove_dir(directory).is_err() {
                break;
            }
            parent = directory.parent();
        }
    }
}

impl LocalStorage for FileStorage {
    fn cache_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        let mut objects = self.objects(&self.blob_dir)?;
        objects.extend(self.generated_objects()?);
        Ok(objects)
    }

    fn staging_objects(&self) -> Result<Vec<StoredObject>, StorageError> {
        self.objects(&self.staging_dir)
    }

    fn remove_cache_object(&self, reference: &str) -> Result<(), StorageError> {
        let path = PathBuf::from(reference);
        let blob = path.parent() == Some(self.blob_dir.as_path());
        let generated = path
            .strip_prefix(&self.generated_dir)
            .ok()
            .filter(|relative| {
                relative
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_)))
            })
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                matches!(
                    name,
                    "Messages.md" | "Messages.ndjson" | ".chat.json" | "chat.json"
                )
            });
        if !blob && !generated {
            return Err(StorageError::new(
                "cache handle escaped the owned directory",
            ));
        }
        if generated && path.exists() {
            let parent = path
                .parent()
                .ok_or_else(|| StorageError::new("generated cache handle has no parent"))?;
            let canonical_parent = parent.canonicalize().map_err(storage_io)?;
            let canonical_generated = self.generated_dir.canonicalize().map_err(storage_io)?;
            if !canonical_parent.starts_with(canonical_generated) {
                return Err(StorageError::new(
                    "generated cache handle escaped the owned directory",
                ));
            }
        }
        remove_file_if_exists(&path).map_err(storage_io)?;
        if generated {
            self.prune_empty_generated_parents(&path);
        }
        Ok(())
    }

    fn remove_staging_object(&self, reference: &str) -> Result<(), StorageError> {
        let path = PathBuf::from(reference);
        if path.parent() != Some(self.staging_dir.as_path()) {
            return Err(StorageError::new(
                "staging handle escaped the owned directory",
            ));
        }
        remove_file_if_exists(&path).map_err(storage_io)
    }
}

struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        now_ms()
    }
}

struct ContentAdmission {
    version: ContentVersion,
    extent: u64,
    cache_only: bool,
}

fn admission(
    store: &mut StateStore,
    account: AccountId,
    item: &ItemId,
    pinned: Option<&str>,
) -> Result<ContentAdmission, DriveError> {
    let read = store.read_txn().map_err(state_error)?;
    let record = read
        .item(item)
        .map_err(state_error)?
        .ok_or_else(|| DriveError::NotFound {
            detail: "item does not exist".to_owned(),
        })?;
    if record.deleted_at_ms.is_some() {
        return Err(DriveError::NotFound {
            detail: "item is deleted".to_owned(),
        });
    }
    let facts = record.content.ok_or_else(|| DriveError::InvalidArgument {
        detail: "directories have no content".to_owned(),
    })?;
    let cache_only = matches!(
        item.key(),
        ItemKey::Canonical(CanonicalKey::GeneratedDoc(_))
            | ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
                item: CanonicalKey::GeneratedDoc(_),
                ..
            })
    );
    if cache_only {
        match record.availability {
            StateItemAvailability::Fetchable => {}
            StateItemAvailability::Restricted => {
                return Err(DriveError::Restricted {
                    detail: "generated content is protected by source policy".to_owned(),
                });
            }
            StateItemAvailability::Unavailable => {
                return Err(DriveError::SourceUnavailable {
                    detail: "generated content is unavailable".to_owned(),
                });
            }
        }
    } else {
        let (content_key, availability) = content_eligibility(&read, account, item)
            .map_err(state_error)?
            .ok_or_else(|| DriveError::SourceUnavailable {
                detail: "content eligibility facts are unavailable".to_owned(),
            })?;
        match availability {
            SourceAvailability::Fetchable => {}
            SourceAvailability::Restricted | SourceAvailability::ViewOnce => {
                return Err(DriveError::Restricted {
                    detail: "content is protected by source policy".to_owned(),
                });
            }
            SourceAvailability::Unavailable => {
                let retained_audit_blob = match content_key {
                    HydrationContentKey::Story(key) => read
                        .story(&key)
                        .map_err(state_error)?
                        .is_some_and(|story| story.blob_hash.is_some()),
                    HydrationContentKey::Attachment(_) => false,
                };
                if !retained_audit_blob {
                    return Err(DriveError::SourceUnavailable {
                        detail: "content is unavailable at the source".to_owned(),
                    });
                }
            }
        }
    }
    let version = facts.content_version.ok_or_else(|| DriveError::Integrity {
        detail: "content has no version pin".to_owned(),
    })?;
    if pinned.is_some_and(|pinned| pinned != version.as_str()) {
        return Err(DriveError::VersionConflict {
            detail: "pinned content version is no longer current".to_owned(),
        });
    }
    let extent = facts.logical_size.ok_or_else(|| DriveError::Integrity {
        detail: "content extent is unknown".to_owned(),
    })?;
    Ok(ContentAdmission {
        version,
        extent,
        cache_only,
    })
}

/// Classifies a generated-document cache miss after its initial admission.
///
/// Monthly publication atomically replaces the item and cache row, but it can
/// do so between `admission` and `cached_file`. That ordinary race is a version
/// conflict, not a missing source: the provider can refresh metadata and retry
/// once. Returning source-unavailable here made otherwise valid Markdown and
/// NDJSON opens fail intermittently while history publication was active.
fn generated_cache_miss_error(
    store: &mut StateStore,
    account: AccountId,
    item: &ItemId,
    admitted_version: &ContentVersion,
) -> DriveError {
    match admission(store, account, item, None) {
        Ok(current) if current.cache_only && current.version != *admitted_version => {
            DriveError::VersionConflict {
                detail: "generated content advanced during cache admission".to_owned(),
            }
        }
        Ok(_) => DriveError::SourceUnavailable {
            detail: "generated materialization is not currently available".to_owned(),
        },
        Err(error) => error,
    }
}

fn thumbnail_admission(
    store: &mut StateStore,
    account: AccountId,
    item: &ItemId,
    pinned: Option<&str>,
) -> Result<ContentVersion, DriveError> {
    let read = store.read_txn().map_err(state_error)?;
    let record = read
        .item(item)
        .map_err(state_error)?
        .ok_or_else(|| DriveError::NotFound {
            detail: "item does not exist".to_owned(),
        })?;
    if record.deleted_at_ms.is_some() {
        return Err(DriveError::NotFound {
            detail: "item is deleted".to_owned(),
        });
    }
    let facts = record.content.ok_or_else(|| DriveError::InvalidArgument {
        detail: "directories have no thumbnails".to_owned(),
    })?;
    let (_, availability) = attachment_eligibility(&read, account, item)
        .map_err(state_error)?
        .ok_or_else(|| DriveError::SourceUnavailable {
            detail: "attachment eligibility facts are unavailable".to_owned(),
        })?;
    match availability {
        SourceAvailability::Fetchable => {}
        SourceAvailability::Restricted | SourceAvailability::ViewOnce => {
            return Err(DriveError::Restricted {
                detail: "source policy forbids a preview".to_owned(),
            });
        }
        SourceAvailability::Unavailable => {
            return Err(DriveError::SourceUnavailable {
                detail: "content is unavailable at the source".to_owned(),
            });
        }
    }
    let version = facts.content_version.ok_or_else(|| DriveError::Integrity {
        detail: "content has no version pin".to_owned(),
    })?;
    if pinned.is_some_and(|pinned| pinned != version.as_str()) {
        return Err(DriveError::VersionConflict {
            detail: "pinned content version is no longer current".to_owned(),
        });
    }
    Ok(version)
}

struct CachedFile {
    file: HydratedFile,
    generated_lease: Option<GeneratedFileLease>,
}

impl Hydrator {
    fn adopt_cached_file(&self, cached: CachedFile) -> HydratedFile {
        let CachedFile {
            mut file,
            generated_lease,
        } = cached;
        let Some(lease) = generated_lease else {
            return file;
        };
        let sequence = self.next_staged_lease.fetch_add(1, Ordering::Relaxed);
        let lease_id = format!("hydration-lease-{sequence}");
        self.staged_leases
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(lease_id.clone(), lease);
        file.lease_id = Some(lease_id);
        file
    }
}

fn cached_file(
    store: &mut StateStore,
    item: &ItemId,
    version: &ContentVersion,
    extent: u64,
    now: i64,
    token: &CancellationToken,
) -> Result<Option<CachedFile>, DriveError> {
    let entry = {
        let read = store.read_txn().map_err(state_error)?;
        match read.cache_entry(item).map_err(state_error)? {
            Some(entry) => Some(entry),
            None => match item.key() {
                ItemKey::StoryAppearance(appearance) => read
                    .story(&appearance.story)
                    .map_err(state_error)?
                    .and_then(|story| story.blob_hash)
                    .map(|hash| {
                        read.verified_cache_entry_for_blob(
                            appearance.story.poster.scope.account,
                            &hash,
                            version,
                            extent,
                        )
                        .map_err(state_error)
                    })
                    .transpose()?
                    .flatten(),
                _ => None,
            },
        }
    };
    let Some(entry) = entry else { return Ok(None) };
    if entry.content_version != *version
        || entry.verification != CacheVerification::Verified
        || entry.size != extent
    {
        return Ok(None);
    }
    let Some(reference) = entry.materialization_ref else {
        return Ok(None);
    };
    let metadata = match fs::metadata(&reference) {
        Ok(metadata) if metadata.is_file() && metadata.len() == extent => metadata,
        _ => return Ok(None),
    };
    let _ = metadata;
    if now.saturating_sub(entry.last_access_at_ms) >= CACHE_TOUCH_GRANULARITY_MS {
        // A write lock held by the agent must not turn an already verified
        // File Provider cache hit into cannotSynchronize. The state layer
        // attempts this with a zero busy timeout and restores its regular
        // timeout afterwards; all bookkeeping failures are deliberately
        // best-effort after byte verification.
        let _ = store.try_touch_cache_entry(&entry.item, now);
    }
    let generated_lease = if entry.kind == gramdrive_state::repo::CacheKind::GeneratedDoc {
        // Acquire while the render pipeline's reclamation lock is held. If a
        // concurrent publication swept this old generation first, treat it as
        // a normal generated cache miss so admission can report the version
        // conflict rather than returning a path that has already vanished.
        match GeneratedFileLease::acquire_bounded(
            Path::new(&reference),
            GENERATED_LEASE_WAIT,
            || token.is_cancelled(),
        ) {
            GeneratedFileLeaseAcquire::Acquired(lease) => Some(lease),
            GeneratedFileLeaseAcquire::Missing => return Ok(None),
            GeneratedFileLeaseAcquire::Busy => {
                return Err(DriveError::VersionConflict {
                    detail: "generated publication is briefly refreshing this version".to_owned(),
                });
            }
            GeneratedFileLeaseAcquire::Cancelled => return Err(cancelled()),
        }
    } else {
        None
    };
    Ok(Some(CachedFile {
        file: HydratedFile {
            path: reference,
            content_version: version.as_str().to_owned(),
            byte_count: extent,
            lease_id: None,
        },
        generated_lease,
    }))
}

fn promote_transfer(
    store: &mut StateStore,
    staging: &mut FileStagingHost,
    promotion: &mut FilePromotionHost,
    promoter: &mut Promoter,
    storage: &FileStorage,
    transfer: TransferId,
) -> Result<(), DriveError> {
    match promoter
        .promote(store, staging, promotion, transfer, now_ms())
        .map_err(engine_error)?
    {
        Promotion::Materialized { .. } | Promotion::AlreadyMaterialized { .. } => Ok(()),
        Promotion::IntegrityFailed { detail, disposal } => {
            if let Some(disposal) = disposal {
                storage.remove_staging(&disposal.staging)?;
            }
            Err(DriveError::Integrity { detail })
        }
        Promotion::VersionDeparted { disposal, .. } => {
            if let Some(disposal) = disposal {
                storage.remove_staging(&disposal.staging)?;
            }
            Err(DriveError::VersionConflict {
                detail: "content version departed during promotion".to_owned(),
            })
        }
        Promotion::NotWholeContent { disposal } => {
            if let Some(disposal) = disposal {
                storage.remove_staging(&disposal.staging)?;
            }
            Err(DriveError::Integrity {
                detail: "transfer did not cover the exact attachment extent".to_owned(),
            })
        }
    }
}

async fn wait_until_unchecked(deadline_ms: i64) {
    let delay = deadline_ms.saturating_sub(now_ms());
    let millis = u64::try_from(delay).unwrap_or(0);
    tokio::time::sleep(std::time::Duration::from_millis(millis)).await;
}

fn item_account_id(item: &ItemId) -> Option<i64> {
    let canonical = match item.key() {
        ItemKey::Canonical(key) => key,
        ItemKey::Appearance(appearance) => appearance.item,
        ItemKey::StoryAppearance(appearance) => CanonicalKey::Story(appearance.story),
    };
    Some(match canonical {
        CanonicalKey::Account(key) => key.account_id.0,
        CanonicalKey::ChatList(key) => key.scope.account.account_id.0,
        CanonicalKey::FolderCatalog(key) => key.scope.account.account_id.0,
        CanonicalKey::Chat(key) => key.scope.account.account_id.0,
        CanonicalKey::ActiveStories(key) => key.chat.scope.account.account_id.0,
        CanonicalKey::MonthDir(key) => key.chat.scope.account.account_id.0,
        CanonicalKey::YearDir(key) => key.chat.scope.account.account_id.0,
        CanonicalKey::MediaDir(key) => key.chat.scope.account.account_id.0,
        CanonicalKey::Message(key) => key.chat.scope.account.account_id.0,
        CanonicalKey::Attachment(key) => key.message.chat.scope.account.account_id.0,
        CanonicalKey::Story(key) => key.poster.scope.account.account_id.0,
        CanonicalKey::GeneratedDoc(key) => key.chat.scope.account.account_id.0,
        CanonicalKey::OrderDoc(key) => key.list.scope.account.account_id.0,
        CanonicalKey::Blob(key) => key.account.account_id.0,
    })
}

fn failure_error(category: FailureCategory) -> DriveError {
    match category {
        FailureCategory::InvalidRequest => DriveError::InvalidArgument {
            detail: "source rejected the hydration request".to_owned(),
        },
        FailureCategory::NotFound => DriveError::SourceUnavailable {
            // The transfer started only after durable admission proved the
            // row live. This category now means its renderer/source lost the
            // object; only a fresh durable lookup may assert deletion.
            detail: "content disappeared at the source; retry after refresh".to_owned(),
        },
        FailureCategory::AuthRequired => DriveError::AuthRequired {
            detail: "Telegram authorization is required".to_owned(),
        },
        FailureCategory::RateLimited => DriveError::RateLimited {
            detail: "bounded retry budget was exhausted".to_owned(),
            retry_after_ms: None,
        },
        FailureCategory::Restricted => DriveError::Restricted {
            detail: "source policy forbids saving these bytes".to_owned(),
        },
        FailureCategory::VersionConflict => DriveError::VersionConflict {
            detail: "content version changed during hydration".to_owned(),
        },
        FailureCategory::Cancelled => cancelled(),
        FailureCategory::Unavailable | FailureCategory::StaleReference => {
            DriveError::SourceUnavailable {
                detail: "source retry budget was exhausted".to_owned(),
            }
        }
        FailureCategory::DiskFull => DriveError::Storage {
            detail: "staging storage is full".to_owned(),
        },
        FailureCategory::Integrity => DriveError::Integrity {
            detail: "staged bytes failed verification".to_owned(),
        },
        FailureCategory::Internal => DriveError::Internal {
            detail: "hydration engine invariant failed".to_owned(),
        },
    }
}

fn engine_error(error: gramdrive_engine::transfer::EngineError) -> DriveError {
    match error {
        gramdrive_engine::transfer::EngineError::State(error) => state_error(error),
        gramdrive_engine::transfer::EngineError::Storage { detail } => {
            DriveError::Storage { detail }
        }
        gramdrive_engine::transfer::EngineError::NotHydratable { reason } => {
            DriveError::Restricted {
                detail: reason.to_owned(),
            }
        }
        gramdrive_engine::transfer::EngineError::RangeBeyondExtent { .. } => {
            DriveError::Integrity {
                detail: error.to_string(),
            }
        }
        gramdrive_engine::transfer::EngineError::IncompleteContent { .. }
        | gramdrive_engine::transfer::EngineError::UnknownExtent
        | gramdrive_engine::transfer::EngineError::ProgressRegression
        | gramdrive_engine::transfer::EngineError::StagingChanged => DriveError::Integrity {
            detail: error.to_string(),
        },
    }
}

pub(crate) fn state_error(error: gramdrive_state::StateError) -> DriveError {
    match error {
        gramdrive_state::StateError::InvalidArgument { what } => DriveError::InvalidArgument {
            detail: what.to_owned(),
        },
        gramdrive_state::StateError::RowNotFound { entity } => DriveError::NotFound {
            detail: format!("{entity} does not exist"),
        },
        error => DriveError::Storage {
            detail: error.to_string(),
        },
    }
}

fn source_state_error(error: gramdrive_state::StateError) -> SourceError {
    SourceError::Internal {
        detail: format!("state-backed locator refresh failed: {error}"),
    }
}

fn source_error(error: SourceError) -> DriveError {
    match error {
        SourceError::InvalidRequest { detail } | SourceError::CursorRejected { detail } => {
            DriveError::InvalidArgument { detail }
        }
        // A source adapter cannot prove the durable provider row is gone.
        // Keep its result retryable; admission is the only durable absence
        // authority for File Provider callbacks.
        SourceError::NotFound { detail } => DriveError::SourceUnavailable { detail },
        SourceError::AuthRequired { detail } => DriveError::AuthRequired { detail },
        SourceError::RateLimited {
            retry_after,
            detail,
        } => DriveError::RateLimited {
            detail,
            retry_after_ms: retry_after
                .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
        },
        SourceError::Restricted { detail } => DriveError::Restricted { detail },
        SourceError::StaleReference { detail } | SourceError::Unavailable { detail } => {
            DriveError::SourceUnavailable { detail }
        }
        SourceError::VersionConflict { detail, .. } => DriveError::VersionConflict { detail },
        SourceError::Cancelled { detail } => DriveError::Cancelled { detail },
        SourceError::Internal { detail } => DriveError::Internal { detail },
    }
}

fn cancelled() -> DriveError {
    DriveError::Cancelled {
        detail: "hydration cancelled by caller".to_owned(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn hash_hex(hash: &ContentHash) -> String {
    let ContentHash::Sha256(bytes) = hash;
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn staging_failed(detail: impl Into<String>) -> StagingError {
    StagingError::Failed {
        detail: detail.into(),
    }
}

fn staging_io(step: &str, error: std::io::Error) -> StagingError {
    if error.raw_os_error() == Some(28) {
        StagingError::Full {
            detail: format!("{step}: {error}"),
        }
    } else {
        staging_failed(format!("{step}: {error}"))
    }
}

fn promotion_io(error: std::io::Error) -> PromotionHostError {
    PromotionHostError::new(error.to_string())
}

fn storage_io(error: std::io::Error) -> StorageError {
    StorageError::new(error.to_string())
}

fn file_storage_error(error: std::io::Error) -> DriveError {
    DriveError::Storage {
        detail: error.to_string(),
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), std::io::Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    use gramdrive_model::identity::{
        AccountKey, AccountScope, AttachmentIndex, ChatId, ChatKey, ChatListKey, ChatListKind,
        DocFormat, DocPartition, GeneratedDocKey, MessageId, MessageKey, NamespaceVersion,
        SchemaFamily, StoryAppearanceKey, StoryAppearanceLocation, StoryId, StoryKey,
    };
    use gramdrive_source_tdjson::StoryCommit;
    use gramdrive_source_tdjson::message::normalize_message;
    use gramdrive_state::repo::{
        AccountRecord, AttachmentFacts, AttachmentFidelity, AttachmentLogicalKind,
        AuditToMirrorConfirmation, CacheEntryRecord, CacheKind, ChatListEntry, ChatRecord,
        ChatSyncRecord, ChatType, FileFacts, ItemAvailability, ItemRecord, MessageChange,
        MessageRevision, PinOrigin, RetentionMode, SourceKind, StoryAppearanceRecord,
        StoryContentLocatorRecord, StoryContentState, StoryFacts, StoryLocatorFileType, SyncWindow,
        TelegramRepresentation, TransferState,
    };

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "gramdrive-hydration-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(path.join("state")).expect("state directory");
            Self(path)
        }

        fn text(&self) -> &str {
            self.0.to_str().expect("UTF-8 temporary path")
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scope() -> AccountScope {
        AccountScope {
            account: AccountKey {
                account_id: AccountId(7),
            },
            namespace_version: NamespaceVersion(1),
        }
    }

    fn root_id() -> ItemId {
        ItemKey::Canonical(CanonicalKey::Account(scope().account)).id()
    }

    fn chat_key() -> ChatKey {
        ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        }
    }

    fn attachment_key() -> AttachmentKey {
        AttachmentKey {
            message: MessageKey {
                chat: chat_key(),
                message_id: MessageId(5),
            },
            index: AttachmentIndex(0),
        }
    }

    fn item_id() -> ItemId {
        ItemKey::Canonical(CanonicalKey::Attachment(attachment_key())).id()
    }

    fn second_attachment_key() -> AttachmentKey {
        AttachmentKey {
            message: MessageKey {
                chat: chat_key(),
                message_id: MessageId(6),
            },
            index: AttachmentIndex(0),
        }
    }

    fn second_item_id() -> ItemId {
        ItemKey::Canonical(CanonicalKey::Attachment(second_attachment_key())).id()
    }

    fn seed(root: &TempRoot, bytes: &[u8]) -> String {
        let layout = shared_state_layout(root.text().to_owned()).expect("layout");
        let mut store = StateStore::open(&layout.database_file).expect("state");
        let tx = store.write_txn().expect("transaction");
        tx.upsert_account(&AccountRecord {
            account: scope().account,
            source_kind: SourceKind::LocalTdlib,
            display_name: "Test".to_owned(),
            auth_state: "authorized".to_owned(),
            namespace_version: scope().namespace_version,
            display_timezone: "UTC".to_owned(),
            retention_mode: RetentionMode::Mirror,
            archive_mode: false,
            secret_ref: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        })
        .expect("account");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: root_id(),
            parent: None,
            display_name: "Test".to_owned(),
            safe_name: "Test".to_owned(),
            metadata_version: gramdrive_model::version::MetadataVersion::new("m1")
                .expect("metadata version"),
            content: None,
            availability: ItemAvailability::Fetchable,
            created_at_ms: Some(1),
            modified_at_ms: Some(1),
            deleted_at_ms: None,
        })
        .expect("root item");
        tx.upsert_chat(&ChatRecord {
            key: chat_key(),
            chat_type: ChatType::Private,
            title: "Chat".to_owned(),
            username: None,
            is_protected: false,
            archive_mode: false,
            metadata_version: gramdrive_model::version::MetadataVersion::new("chat-m1")
                .expect("chat version"),
            left_at_ms: None,
            deleted_at_ms: None,
            last_update_at_ms: Some(1),
        })
        .expect("chat");
        tx.apply_message_changes(
            &chat_key(),
            &[MessageChange::Observed(MessageRevision {
                message_id: MessageId(5),
                sender_id: Some(42),
                sent_at_ms: 1,
                edited_at_ms: None,
                observed_at_ms: 1,
                payload_schema: SchemaFamily(1),
                payload: vec![1],
            })],
        )
        .expect("message");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: item_id(),
            parent: Some(root_id()),
            display_name: "payload.bin".to_owned(),
            safe_name: "payload.bin".to_owned(),
            metadata_version: gramdrive_model::version::MetadataVersion::new("m1")
                .expect("metadata version"),
            content: Some(FileFacts {
                mime_type: Some("application/octet-stream".to_owned()),
                logical_size: Some(bytes.len() as u64),
                content_version: Some(ContentVersion::new("content-v1").expect("version")),
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: Some(1),
            modified_at_ms: Some(1),
            deleted_at_ms: None,
        })
        .expect("attachment item");
        tx.upsert_attachment(&AttachmentFacts {
            key: attachment_key(),
            logical_kind: AttachmentLogicalKind::Document,
            telegram_representation: TelegramRepresentation::OriginalDocument,
            fidelity: AttachmentFidelity::Original,
            source_name: Some("payload.bin".to_owned()),
            mime_type: Some("application/octet-stream".to_owned()),
            exact_size: Some(bytes.len() as u64),
            content_version: ContentVersion::new("content-v1").expect("version"),
            telegram_unique_id: Some("stable-content-id".to_owned()),
            telegram_local_file_id: Some(700),
            telegram_file_id: Some("remote-old".to_owned()),
            file_reference: None,
            availability: StateAttachmentAvailability::Fetchable,
            can_be_saved: true,
        })
        .expect("attachment facts");
        tx.commit().expect("seed commit");
        layout.database_file
    }

    fn seed_second(database: &str, bytes: &[u8]) {
        let mut store = StateStore::open(database).expect("state");
        let tx = store.write_txn().expect("transaction");
        tx.apply_message_changes(
            &chat_key(),
            &[MessageChange::Observed(MessageRevision {
                message_id: MessageId(6),
                sender_id: Some(43),
                sent_at_ms: 2,
                edited_at_ms: None,
                observed_at_ms: 2,
                payload_schema: SchemaFamily(1),
                payload: vec![2],
            })],
        )
        .expect("second message");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: second_item_id(),
            parent: Some(root_id()),
            display_name: "second.bin".to_owned(),
            safe_name: "second.bin".to_owned(),
            metadata_version: gramdrive_model::version::MetadataVersion::new("m2")
                .expect("metadata version"),
            content: Some(FileFacts {
                mime_type: Some("application/octet-stream".to_owned()),
                logical_size: Some(bytes.len() as u64),
                content_version: Some(ContentVersion::new("content-v2").expect("version")),
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: Some(2),
            modified_at_ms: Some(2),
            deleted_at_ms: None,
        })
        .expect("second item");
        tx.upsert_attachment(&AttachmentFacts {
            key: second_attachment_key(),
            logical_kind: AttachmentLogicalKind::Document,
            telegram_representation: TelegramRepresentation::OriginalDocument,
            fidelity: AttachmentFidelity::Original,
            source_name: Some("second.bin".to_owned()),
            mime_type: Some("application/octet-stream".to_owned()),
            exact_size: Some(bytes.len() as u64),
            content_version: ContentVersion::new("content-v2").expect("version"),
            telegram_unique_id: Some("stable-second-id".to_owned()),
            telegram_local_file_id: Some(701),
            telegram_file_id: Some("remote-second".to_owned()),
            file_reference: None,
            availability: StateAttachmentAvailability::Fetchable,
            can_be_saved: true,
        })
        .expect("second attachment");
        tx.commit().expect("second seed commit");
    }

    fn configure_archive(database: &str, retention: RetentionMode, archive_mode: bool) {
        let mut store = StateStore::open(database).expect("state");
        let tx = store.write_txn().expect("Archive policy transaction");
        tx.record_chat_sync(
            &chat_key(),
            &ChatSyncRecord {
                window: Some(SyncWindow {
                    oldest: MessageId(5),
                    newest: MessageId(5),
                }),
                history_complete: true,
                last_sync_at_ms: Some(10),
            },
        )
        .expect("complete metadata history");
        tx.set_retention_mode(scope().account, retention, None, 10)
            .expect("retention mode");
        tx.set_archive_mode(scope().account, archive_mode, 11)
            .expect("Archive Mode");
        tx.commit().expect("commit Archive policy");
    }

    async fn wait_for_fetches(source: &BytesSource, expected: usize) {
        tokio::time::timeout(StdDuration::from_secs(3), async {
            while source.fetches.load(Ordering::SeqCst) < expected {
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .expect("Archive worker fetch");
    }

    fn story_key() -> StoryKey {
        StoryKey {
            poster: chat_key(),
            story_id: StoryId(91),
        }
    }

    fn story_item(location: StoryAppearanceLocation) -> ItemId {
        ItemKey::StoryAppearance(StoryAppearanceKey {
            story: story_key(),
            view: ChatListKind::Main,
            location,
        })
        .id()
    }

    fn seed_story(root: &TempRoot, bytes: &[u8]) -> String {
        let database = seed(root, bytes);
        let mut store = StateStore::open(&database).expect("state");
        let version = ContentVersion::new("story-content-v1").expect("version");
        let tx = store.write_txn().expect("story transaction");
        tx.upsert_story_with_locators(
            &StoryFacts {
                key: story_key(),
                source_timestamp_ms: 1_721_555_200_000,
                mime_type: Some("video/mp4".to_owned()),
                exact_size: Some(bytes.len() as u64),
                content_version: version.clone(),
                availability: StateAttachmentAvailability::Fetchable,
                can_be_forwarded: true,
                content_state: StoryContentState::Available,
            },
            &[StoryContentLocatorRecord {
                story: story_key(),
                role: "video-primary".to_owned(),
                file_type: StoryLocatorFileType::VideoStory,
                is_primary: true,
                local_file_id: Some(791),
                remote_file_id: Some("story-remote".to_owned()),
                remote_unique_id: Some("story-unique".to_owned()),
                size: Some(bytes.len() as u64),
                expected_size: Some(bytes.len() as u64),
                content_version: version.clone(),
            }],
        )
        .expect("story facts");
        tx.set_story_appearance(&StoryAppearanceRecord {
            story: story_key(),
            location: StoryAppearanceLocation::Active,
            display_name: "Story 91.mp4".to_owned(),
            posted_at_ms: 1_721_555_200_000,
            expires_at_ms: Some(1_721_641_600_000),
            removed_at_ms: None,
            profile_scan_generation: None,
            profile_pin_order: None,
        })
        .expect("active appearance");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: story_item(StoryAppearanceLocation::Active),
            parent: Some(root_id()),
            display_name: "Story 91.mp4".to_owned(),
            safe_name: "Story 91.mp4".to_owned(),
            metadata_version: gramdrive_model::version::MetadataVersion::new("story-m1")
                .expect("metadata version"),
            content: Some(FileFacts {
                mime_type: Some("video/mp4".to_owned()),
                logical_size: Some(bytes.len() as u64),
                content_version: Some(version),
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: Some(1_721_555_200_000),
            modified_at_ms: None,
            deleted_at_ms: None,
        })
        .expect("active item");
        tx.commit().expect("story seed commit");
        database
    }

    fn transition_story_to_month(database: &str) -> ItemId {
        let location = StoryAppearanceLocation::Month {
            year: 2024,
            month: 7,
        };
        let month_item = story_item(location);
        let active_item = story_item(StoryAppearanceLocation::Active);
        let mut store = StateStore::open(database).expect("state");
        let tx = store.write_txn().expect("transition transaction");
        tx.set_story_appearance(&StoryAppearanceRecord {
            story: story_key(),
            location,
            display_name: "Story 91.mp4".to_owned(),
            posted_at_ms: 1_721_555_200_000,
            expires_at_ms: None,
            removed_at_ms: None,
            profile_scan_generation: Some(2),
            profile_pin_order: Some(0),
        })
        .expect("month appearance");
        tx.tombstone_item(
            &active_item,
            2_000,
            &gramdrive_model::version::MetadataVersion::new("story-active-removed")
                .expect("metadata version"),
        )
        .expect("remove active item");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: month_item.clone(),
            parent: Some(root_id()),
            display_name: "Story 91.mp4".to_owned(),
            safe_name: "Story 91.mp4".to_owned(),
            metadata_version: gramdrive_model::version::MetadataVersion::new("story-m2")
                .expect("metadata version"),
            content: Some(FileFacts {
                mime_type: Some("video/mp4".to_owned()),
                logical_size: tx
                    .read()
                    .story(&story_key())
                    .expect("story query")
                    .expect("story")
                    .facts
                    .exact_size,
                content_version: Some(
                    ContentVersion::new("story-content-v1").expect("content version"),
                ),
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: Some(1_721_555_200_000),
            modified_at_ms: None,
            deleted_at_ms: None,
        })
        .expect("month item");
        tx.commit().expect("transition commit");
        month_item
    }

    #[derive(Debug)]
    struct BytesSource {
        bytes: Vec<u8>,
        fetches: AtomicUsize,
    }

    impl ContentSource for BytesSource {
        fn fetch<'a>(
            &'a self,
            request: FetchRequest,
            sink: &'a mut dyn ContentSink,
        ) -> SourceFuture<'a, ()> {
            Box::pin(async move {
                self.fetches.fetch_add(1, Ordering::SeqCst);
                let start = usize::try_from(request.range.start()).expect("range start");
                let end = usize::try_from(request.range.end()).expect("range end");
                let chunk = ContentChunk::new(request.range.start(), &self.bytes[start..end])
                    .expect("non-empty chunk");
                if sink.accept(chunk) == SinkControl::Stop {
                    return Err(SourceError::Cancelled {
                        detail: "test sink stopped".to_owned(),
                    });
                }
                Ok(())
            })
        }
    }

    /// A normal source implementation that records where the production
    /// driver polls it. It has no scheduling knobs: the exported Hydrator
    /// boundary and its owned runtime decide that placement.
    #[derive(Debug)]
    struct ThreadRecordingSource {
        bytes: Vec<u8>,
        threads: Mutex<Vec<String>>,
    }

    impl ThreadRecordingSource {
        fn thread_names(&self) -> Vec<String> {
            self.threads
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    impl ContentSource for ThreadRecordingSource {
        fn fetch<'a>(
            &'a self,
            request: FetchRequest,
            sink: &'a mut dyn ContentSink,
        ) -> SourceFuture<'a, ()> {
            Box::pin(async move {
                self.threads
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(
                        std::thread::current()
                            .name()
                            .unwrap_or("unnamed")
                            .to_owned(),
                    );
                let start = usize::try_from(request.range.start()).expect("range start");
                let end = usize::try_from(request.range.end()).expect("range end");
                let chunk = ContentChunk::new(request.range.start(), &self.bytes[start..end])
                    .expect("non-empty chunk");
                if sink.accept(chunk) == SinkControl::Stop {
                    return Err(SourceError::Cancelled {
                        detail: "test sink stopped".to_owned(),
                    });
                }
                Ok(())
            })
        }
    }

    /// Deterministic preview driver for the same production routing branch
    /// that normally holds `TdThumbnailer`. It records its poll thread but
    /// does not bypass admission or file publication.
    #[derive(Debug)]
    struct ThreadRecordingThumbnailer {
        threads: Mutex<Vec<String>>,
        bytes: Vec<u8>,
    }

    impl ThreadRecordingThumbnailer {
        fn thread_names(&self) -> Vec<String> {
            self.threads
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    impl ThumbnailServing for ThreadRecordingThumbnailer {
        fn thumbnail(
            &self,
            _item: ItemId,
            _spec: ThumbnailSpec,
        ) -> SourceFuture<'_, Option<Thumbnail>> {
            Box::pin(async move {
                self.threads
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(
                        std::thread::current()
                            .name()
                            .unwrap_or("unnamed")
                            .to_owned(),
                    );
                Ok(Some(
                    Thumbnail::new("image/png", self.bytes.clone()).expect("test thumbnail"),
                ))
            })
        }
    }

    /// Coordinates two real exported operations while their production source
    /// callbacks are live. The barrier occupies every HydrationRuntime worker
    /// exactly once; after that rendezvous the callbacks suspend normally, so
    /// a fresh foreground request must be able to start and publish without
    /// releasing either blocker.
    struct RuntimeSaturationCallbacks {
        worker_barrier: std::sync::Barrier,
        hold_blockers: Arc<AsyncGate>,
        entered: std::sync::mpsc::Sender<&'static str>,
        fresh_started: std::sync::mpsc::Sender<()>,
    }

    struct RuntimeSaturationSource {
        blocked_item: String,
        blocked_bytes: Vec<u8>,
        fresh_bytes: Vec<u8>,
        callbacks: Arc<RuntimeSaturationCallbacks>,
    }

    impl ContentSource for RuntimeSaturationSource {
        fn fetch<'a>(
            &'a self,
            request: FetchRequest,
            sink: &'a mut dyn ContentSink,
        ) -> SourceFuture<'a, ()> {
            Box::pin(async move {
                let bytes = if request.item.text() == self.blocked_item {
                    self.callbacks
                        .entered
                        .send("attachment")
                        .expect("announce blocked attachment callback");
                    self.callbacks.worker_barrier.wait();
                    self.callbacks.hold_blockers.wait().await;
                    &self.blocked_bytes
                } else {
                    self.callbacks
                        .fresh_started
                        .send(())
                        .expect("announce fresh attachment callback");
                    &self.fresh_bytes
                };
                let start = usize::try_from(request.range.start()).expect("range start");
                let end = usize::try_from(request.range.end()).expect("range end");
                let chunk = ContentChunk::new(request.range.start(), &bytes[start..end])
                    .expect("non-empty chunk");
                if sink.accept(chunk) == SinkControl::Stop {
                    return Err(SourceError::Cancelled {
                        detail: "test sink stopped".to_owned(),
                    });
                }
                Ok(())
            })
        }
    }

    struct RuntimeSaturationThumbnailer {
        callbacks: Arc<RuntimeSaturationCallbacks>,
        bytes: Vec<u8>,
    }

    impl ThumbnailServing for RuntimeSaturationThumbnailer {
        fn thumbnail(
            &self,
            _item: ItemId,
            _spec: ThumbnailSpec,
        ) -> SourceFuture<'_, Option<Thumbnail>> {
            Box::pin(async move {
                self.callbacks
                    .entered
                    .send("thumbnail")
                    .expect("announce blocked thumbnail callback");
                self.callbacks.worker_barrier.wait();
                self.callbacks.hold_blockers.wait().await;
                Ok(Some(
                    Thumbnail::new("image/png", self.bytes.clone()).expect("test thumbnail"),
                ))
            })
        }
    }

    struct NoopProgress;

    impl ProgressListener for NoopProgress {
        fn on_progress(&self, _progress: TransferProgress) {}
    }

    #[derive(Debug)]
    struct HangingSource {
        started: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }

    struct FetchDropGuard(Arc<AtomicUsize>);

    impl Drop for FetchDropGuard {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl ContentSource for HangingSource {
        fn fetch<'a>(
            &'a self,
            _request: FetchRequest,
            _sink: &'a mut dyn ContentSink,
        ) -> SourceFuture<'a, ()> {
            let started = Arc::clone(&self.started);
            let dropped = Arc::clone(&self.dropped);
            Box::pin(async move {
                let _guard = FetchDropGuard(dropped);
                started.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<Result<(), SourceError>>().await
            })
        }
    }

    #[derive(Debug)]
    struct ReplacingSource {
        bytes: Vec<u8>,
        fetches: AtomicUsize,
        first_started: Arc<AtomicBool>,
        first_dropped: Arc<AtomicUsize>,
        replacement_gate: Arc<AsyncGate>,
    }

    impl ContentSource for ReplacingSource {
        fn fetch<'a>(
            &'a self,
            request: FetchRequest,
            sink: &'a mut dyn ContentSink,
        ) -> SourceFuture<'a, ()> {
            let generation = self.fetches.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if generation == 0 {
                    let _guard = FetchDropGuard(Arc::clone(&self.first_dropped));
                    self.first_started.store(true, Ordering::Release);
                    return std::future::pending::<Result<(), SourceError>>().await;
                }

                self.replacement_gate.wait().await;
                let start = usize::try_from(request.range.start()).expect("range start");
                let end = usize::try_from(request.range.end()).expect("range end");
                let chunk = ContentChunk::new(request.range.start(), &self.bytes[start..end])
                    .expect("non-empty chunk");
                if sink.accept(chunk) == SinkControl::Stop {
                    return Err(SourceError::Cancelled {
                        detail: "test sink stopped".to_owned(),
                    });
                }
                Ok(())
            })
        }
    }

    struct BlockingCancelProbe {
        checkpoints: std::sync::mpsc::Sender<CancelCheckpoint>,
        last_reader_release: Mutex<std::sync::mpsc::Receiver<()>>,
        source_cancel_release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl CancelProbe for BlockingCancelProbe {
        fn checkpoint(&self, checkpoint: CancelCheckpoint) {
            self.checkpoints
                .send(checkpoint)
                .expect("announce cancellation checkpoint");
            let release = match checkpoint {
                CancelCheckpoint::LastReaderClosed => &self.last_reader_release,
                CancelCheckpoint::SourceGenerationCancelled => &self.source_cancel_release,
            };
            release
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("release cancellation checkpoint");
        }
    }

    struct BlockingPromotionProbe {
        checkpoints: std::sync::mpsc::Sender<TransferId>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl PromotionProbe for BlockingPromotionProbe {
        fn before_publication(&self, transfer: TransferId) {
            self.checkpoints
                .send(transfer)
                .expect("announce pre-publication checkpoint");
            self.release
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("release pre-publication checkpoint");
        }
    }

    struct BlockingMaterializationProbe {
        target: TransferId,
        checkpoints: std::sync::mpsc::Sender<TransferId>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    struct BlockingDriverProbe {
        remaining: AtomicUsize,
        checkpoints: std::sync::mpsc::Sender<()>,
        releases: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl DriverProbe for BlockingDriverProbe {
        fn before_claim(&self) {
            if self
                .remaining
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_err()
            {
                return;
            }
            self.checkpoints
                .send(())
                .expect("announce pre-claim checkpoint");
            self.releases
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("release pre-claim checkpoint");
        }
    }

    impl MaterializationProbe for BlockingMaterializationProbe {
        fn before_bind(&self, transfer: TransferId) {
            if transfer != self.target {
                return;
            }
            self.checkpoints
                .send(transfer)
                .expect("announce pre-bind checkpoint");
            self.release
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("release pre-bind checkpoint");
        }
    }

    struct SelectivePromotionProbe {
        target: TransferId,
        checkpoints: std::sync::mpsc::Sender<TransferId>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl PromotionProbe for SelectivePromotionProbe {
        fn before_publication(&self, transfer: TransferId) {
            if transfer != self.target {
                return;
            }
            self.checkpoints
                .send(transfer)
                .expect("announce selected pre-publication checkpoint");
            self.release
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("release selected pre-publication checkpoint");
        }
    }

    #[derive(Debug, Default)]
    struct AsyncGate {
        open: AtomicBool,
        changed: tokio::sync::Notify,
    }

    impl AsyncGate {
        fn release(&self) {
            self.open.store(true, Ordering::Release);
            self.changed.notify_waiters();
        }

        async fn wait(&self) {
            loop {
                let changed = self.changed.notified();
                if self.open.load(Ordering::Acquire) {
                    return;
                }
                changed.await;
            }
        }
    }

    #[derive(Debug)]
    struct GatedSource {
        bytes: Vec<u8>,
        fetches: AtomicUsize,
        started: Arc<AtomicUsize>,
        gate: Arc<AsyncGate>,
    }

    impl ContentSource for GatedSource {
        fn fetch<'a>(
            &'a self,
            request: FetchRequest,
            sink: &'a mut dyn ContentSink,
        ) -> SourceFuture<'a, ()> {
            Box::pin(async move {
                self.fetches.fetch_add(1, Ordering::SeqCst);
                self.started.fetch_add(1, Ordering::SeqCst);
                self.gate.wait().await;
                let start = usize::try_from(request.range.start()).expect("range start");
                let end = usize::try_from(request.range.end()).expect("range end");
                let chunk = ContentChunk::new(request.range.start(), &self.bytes[start..end])
                    .expect("non-empty chunk");
                if sink.accept(chunk) == SinkControl::Stop {
                    return Err(SourceError::Cancelled {
                        detail: "test sink stopped".to_owned(),
                    });
                }
                Ok(())
            })
        }
    }

    #[derive(Debug)]
    struct TwoItemGatedSource {
        first_item: String,
        first_bytes: Vec<u8>,
        second_bytes: Vec<u8>,
        started: Arc<AtomicUsize>,
        first_gate: Arc<AsyncGate>,
    }

    impl ContentSource for TwoItemGatedSource {
        fn fetch<'a>(
            &'a self,
            request: FetchRequest,
            sink: &'a mut dyn ContentSink,
        ) -> SourceFuture<'a, ()> {
            Box::pin(async move {
                self.started.fetch_add(1, Ordering::SeqCst);
                let bytes = if request.item.text() == self.first_item {
                    self.first_gate.wait().await;
                    &self.first_bytes
                } else {
                    &self.second_bytes
                };
                let start = usize::try_from(request.range.start()).expect("range start");
                let end = usize::try_from(request.range.end()).expect("range end");
                let chunk = ContentChunk::new(request.range.start(), &bytes[start..end])
                    .expect("non-empty chunk");
                if sink.accept(chunk) == SinkControl::Stop {
                    return Err(SourceError::Cancelled {
                        detail: "test sink stopped".to_owned(),
                    });
                }
                Ok(())
            })
        }
    }

    #[derive(Debug)]
    struct RetryOneSource {
        first_item: String,
        first_bytes: Vec<u8>,
        second_bytes: Vec<u8>,
        first_attempts: AtomicUsize,
    }

    impl ContentSource for RetryOneSource {
        fn fetch<'a>(
            &'a self,
            request: FetchRequest,
            sink: &'a mut dyn ContentSink,
        ) -> SourceFuture<'a, ()> {
            Box::pin(async move {
                let is_first = request.item.text() == self.first_item;
                if is_first && self.first_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(SourceError::Unavailable {
                        detail: "transient test failure".to_owned(),
                    });
                }
                let bytes = if is_first {
                    &self.first_bytes
                } else {
                    &self.second_bytes
                };
                let start = usize::try_from(request.range.start()).expect("range start");
                let end = usize::try_from(request.range.end()).expect("range end");
                let chunk = ContentChunk::new(request.range.start(), &bytes[start..end])
                    .expect("non-empty chunk");
                if sink.accept(chunk) == SinkControl::Stop {
                    return Err(SourceError::Cancelled {
                        detail: "test sink stopped".to_owned(),
                    });
                }
                Ok(())
            })
        }
    }

    async fn wait_for_test(mut condition: impl FnMut() -> bool) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("test condition timed out");
    }

    async fn race_first_opens_then_cancel(
        hydrator: &Arc<Hydrator>,
        database: &str,
        started: &Arc<AtomicUsize>,
        dropped: &Arc<AtomicUsize>,
        transfer: TransferId,
        generation: usize,
    ) {
        let item = item_id();
        let version = ContentVersion::new("content-v1").expect("version");
        if generation > 1 {
            let cancellations = hydrator
                .sources
                .cancellations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let signal = cancellations
                .get(&RoutingSource::cancellation_key(&item, &version))
                .expect("the previous cancelled generation remains observable");
            assert!(
                *signal.cancelled.borrow(),
                "the previous generation must still be cancelled before admission resets it"
            );
        }

        // Keep reset_cancel blocked after the first coordinator open. The
        // per-content admission must prevent the second open from attaching
        // and launching a driver until the reset becomes eligible to finish.
        let admission = hydrator.admission_for(&item, &version);
        let held_sources = Arc::clone(&hydrator.sources);
        let (locked_send, locked_receive) = std::sync::mpsc::sync_channel(0);
        let (release_send, release_receive) = std::sync::mpsc::sync_channel(0);
        let cancellation_holder = std::thread::spawn(move || {
            let _cancellations = held_sources
                .cancellations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            locked_send
                .send(())
                .expect("announce held cancellation map");
            release_receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("release held cancellation map");
        });
        locked_receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("cancellation map holder starts");

        let first_token = CancellationToken::new();
        let first_cancel = Arc::clone(&first_token);
        let first_hydrator = Arc::clone(hydrator);
        let first = tokio::spawn(async move {
            first_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    first_token,
                )
                .await
        });
        wait_for_test(|| hydrator.coordinator.reader_count(transfer) == 1).await;

        let second_entered = Arc::new(AtomicBool::new(false));
        let second_started = Arc::clone(&second_entered);
        let second_token = CancellationToken::new();
        let second_cancel = Arc::clone(&second_token);
        let second_hydrator = Arc::clone(hydrator);
        let second = tokio::spawn(async move {
            second_started.store(true, Ordering::Release);
            second_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    second_token,
                )
                .await
        });
        wait_for_test(|| {
            second_entered.load(Ordering::Acquire) && Arc::strong_count(&admission) >= 3
        })
        .await;
        assert_eq!(
            hydrator.coordinator.reader_count(transfer),
            1,
            "the second opener must wait behind generation establishment"
        );

        release_send
            .send(())
            .expect("release cancellation generation reset");
        cancellation_holder
            .join()
            .expect("cancellation map holder exits");
        wait_for_test(|| {
            hydrator.coordinator.reader_count(transfer) == 2
                && started.load(Ordering::SeqCst) == generation
        })
        .await;

        first_cancel.cancel();
        second_cancel.cancel();
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(first, second)
        })
        .await
        .expect("both raced readers cancel promptly");
        assert!(matches!(
            first.expect("first task"),
            Err(DriveError::Cancelled { .. })
        ));
        assert!(matches!(
            second.expect("second task"),
            Err(DriveError::Cancelled { .. })
        ));
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            generation,
            "the routed source future for this generation is dropped"
        );

        let mut store = StateStore::open(database).expect("state");
        let read = store.read_txn().expect("read");
        let row = read.transfer(transfer).expect("query").expect("transfer");
        assert_eq!(row.state, TransferState::Cancelled);
        assert!(row.completed_ranges.is_empty());
        assert_eq!(row.temp_ref, None);
        assert_eq!(read.cache_entry(&item_id()).expect("cache query"), None);
        drop(read);
        let layout = shared_state_layout(
            Path::new(database)
                .parent()
                .and_then(Path::parent)
                .expect("data root")
                .to_str()
                .expect("UTF-8 data root")
                .to_owned(),
        )
        .expect("layout");
        assert_eq!(
            fs::read_dir(Path::new(&layout.cache_dir).join("transfers"))
                .expect("staging directory")
                .count(),
            0,
            "cancelled races leave no unpublished staging"
        );
    }

    #[tokio::test]
    async fn hydration_materializes_exact_bytes_once_and_reuses_verified_cache() {
        let root = TempRoot::new();
        let bytes = b"the exact Telegram-served representation".to_vec();
        let database = seed(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let source = Arc::new(BytesSource {
            bytes: bytes.clone(),
            fetches: AtomicUsize::new(0),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);

        // Exercise the exported boundary, not the private implementation:
        // this proves cache miss and hit paths use the Hydrator-owned runtime
        // rather than the caller's Tokio/UniFFI compatibility executor.
        let materialized = Arc::clone(&hydrator)
            .hydrate(
                7,
                item_id().text().to_owned(),
                Some("content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("hydrate");
        assert_eq!(fs::read(&materialized.path).expect("cache bytes"), bytes);
        assert_eq!(materialized.byte_count, bytes.len() as u64);

        let again = Arc::clone(&hydrator)
            .hydrate(
                7,
                item_id().text().to_owned(),
                Some("content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("cache hit");
        assert_eq!(again.path, materialized.path);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);

        let mut store = StateStore::open(&database).expect("state re-open");
        let read = store.read_txn().expect("read");
        let cached = read
            .cache_entry(&item_id())
            .expect("cache query")
            .expect("verified cache row");
        assert_eq!(cached.verification, CacheVerification::Verified);
        assert_eq!(cached.size, bytes.len() as u64);
        assert!(
            read.attachment(&attachment_key())
                .expect("attachment query")
                .expect("attachment")
                .blob_hash
                .is_some(),
            "only promoted bytes are linked to the attachment"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exported_attachment_and_thumbnail_work_run_on_the_owned_runtime() {
        // This deliberately runs the caller on Tokio's single-thread
        // compatibility-shaped executor. Both exported operations must move
        // their SQLite admission, source/driver work, and filesystem
        // publication onto `HydrationRuntime` before doing any of it.
        let root = TempRoot::new();
        let bytes = b"owned runtime attachment bytes".to_vec();
        let _database = seed(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let content_source = Arc::new(ThreadRecordingSource {
            bytes: bytes.clone(),
            threads: Mutex::new(Vec::new()),
        });
        let thumbnailer = Arc::new(ThreadRecordingThumbnailer {
            threads: Mutex::new(Vec::new()),
            bytes: vec![0x89, 0x50, 0x4e, 0x47],
        });
        hydrator.sources.register_with_thumbnail(
            7,
            1,
            Arc::clone(&content_source) as Arc<dyn ContentSource>,
            Arc::clone(&thumbnailer) as Arc<dyn ThumbnailServing>,
        );

        let attachment = tokio::time::timeout(
            StdDuration::from_secs(2),
            Arc::clone(&hydrator).hydrate(
                7,
                item_id().text().to_owned(),
                Some("content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("exported attachment must not wait on the compatibility executor")
        .expect("exported attachment hydration");
        assert_eq!(fs::read(&attachment.path).expect("attachment bytes"), bytes);

        let thumbnail = tokio::time::timeout(
            StdDuration::from_secs(2),
            Arc::clone(&hydrator).thumbnail(
                7,
                item_id().text().to_owned(),
                Some("content-v1".to_owned()),
                64,
                64,
                CancellationToken::new(),
            ),
        )
        .await
        .expect("exported thumbnail must not wait on the compatibility executor")
        .expect("exported thumbnail hydration")
        .expect("thumbnail available");
        assert_eq!(
            fs::read(&thumbnail.path).expect("thumbnail bytes"),
            vec![0x89, 0x50, 0x4e, 0x47]
        );

        let threads = content_source
            .thread_names()
            .into_iter()
            .chain(thumbnailer.thread_names());
        for thread in threads {
            assert!(
                thread.starts_with("gramdrive-hydration"),
                "exported work escaped the owned hydration runtime: {thread}"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn foreground_hydration_completes_under_owned_runtime_saturation_while_history_turns_fairly()
     {
        // The caller intentionally resembles UniFFI async-compat: a single
        // current-thread executor. The two blockers enter the actual exported
        // attachment and thumbnail callbacks on HydrationRuntime, whose
        // capacity comes from the production runtime constant rather than a
        // test-local queue width.
        let root = TempRoot::new();
        let blocked_bytes = b"runtime-saturated attachment".to_vec();
        let fresh_bytes = b"fresh foreground attachment".to_vec();
        let database = seed(&root, &blocked_bytes);
        seed_second(&database, &fresh_bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let (entered_send, entered_receive) = std::sync::mpsc::channel();
        let (fresh_started_send, fresh_started_receive) = std::sync::mpsc::channel();
        let blockers = Arc::new(AsyncGate::default());
        let callbacks = Arc::new(RuntimeSaturationCallbacks {
            worker_barrier: std::sync::Barrier::new(HYDRATION_RUNTIME_WORKERS),
            hold_blockers: Arc::clone(&blockers),
            entered: entered_send,
            fresh_started: fresh_started_send,
        });
        hydrator.sources.register_with_thumbnail(
            7,
            1,
            Arc::new(RuntimeSaturationSource {
                blocked_item: item_id().text().to_owned(),
                blocked_bytes: blocked_bytes.clone(),
                fresh_bytes: fresh_bytes.clone(),
                callbacks: Arc::clone(&callbacks),
            }),
            Arc::new(RuntimeSaturationThumbnailer {
                callbacks,
                bytes: vec![0x89, 0x50, 0x4e, 0x47],
            }),
        );

        let blocked_thumbnail_hydrator = Arc::clone(&hydrator);
        let blocked_thumbnail = tokio::spawn(async move {
            blocked_thumbnail_hydrator
                .thumbnail(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    64,
                    64,
                    CancellationToken::new(),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(
            entered_receive
                .recv_timeout(StdDuration::from_secs(2))
                .expect("thumbnail enters an owned-runtime callback"),
            "thumbnail"
        );

        // The thumbnail callback is now live on one worker. Starting the
        // attachment second makes its real transfer driver occupy the other
        // worker before both callbacks release their barrier rendezvous.
        let blocked_attachment_hydrator = Arc::clone(&hydrator);
        let blocked_attachment = tokio::spawn(async move {
            blocked_attachment_hydrator
                .hydrate(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        tokio::task::yield_now().await;
        assert_eq!(
            entered_receive
                .recv_timeout(StdDuration::from_secs(2))
                .expect("attachment enters an owned-runtime callback"),
            "attachment"
        );

        // Namespace history owns a separate long-lived worker. Compose its
        // real scheduler seam while the native callbacks remain live: two
        // distinct background chats must both receive durable turns.
        let crawl_database = database.clone();
        let crawl = tokio::task::spawn_blocking(move || {
            let mut store = StateStore::open(&crawl_database).expect("history state");
            crate::namespace::test_background_history_fairness_turns(&mut store, scope(), now_ms())
        });

        let clock = Instant::now();
        let fresh_hydrator = Arc::clone(&hydrator);
        let fresh = tokio::spawn(async move {
            fresh_hydrator
                .hydrate(
                    7,
                    second_item_id().text().to_owned(),
                    Some("content-v2".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        tokio::task::yield_now().await;
        fresh_started_receive
            .recv_timeout(StdDuration::from_millis(250))
            .expect("fresh foreground callback starts before blockers release");
        let start_latency = clock.elapsed();
        let fresh = tokio::time::timeout(StdDuration::from_millis(250), fresh)
            .await
            .expect("fresh foreground hydration completes before blockers release")
            .expect("fresh foreground task")
            .expect("fresh foreground result");
        let completion_latency = clock.elapsed();
        let turns = tokio::time::timeout(StdDuration::from_millis(250), crawl)
            .await
            .expect("background history scheduler remains live")
            .expect("background history scheduler task");

        assert!(
            start_latency < StdDuration::from_millis(250),
            "fresh start latency was {start_latency:?}"
        );
        assert!(
            completion_latency < StdDuration::from_millis(250),
            "fresh completion latency was {completion_latency:?}"
        );
        assert_ne!(turns.0, turns.1, "background scheduler fairness regressed");
        assert_eq!(
            fs::read(&fresh.path).expect("fresh cache bytes"),
            fresh_bytes
        );
        // The foreground assertions above occur while both callbacks remain
        // live. Release only afterwards, then retain the attachment and
        // thumbnail completion checks to cover the existing driver behavior.
        blockers.release();
        let attachment = tokio::time::timeout(StdDuration::from_secs(2), blocked_attachment)
            .await
            .expect("blocked attachment drains")
            .expect("blocked attachment task")
            .expect("blocked attachment result");
        let thumbnail = tokio::time::timeout(StdDuration::from_secs(2), blocked_thumbnail)
            .await
            .expect("blocked thumbnail drains")
            .expect("blocked thumbnail task")
            .expect("blocked thumbnail result")
            .expect("blocked thumbnail available");
        assert_eq!(
            fs::read(attachment.path).expect("blocked attachment bytes"),
            blocked_bytes
        );
        assert_eq!(
            fs::read(thumbnail.path).expect("blocked thumbnail bytes"),
            vec![0x89, 0x50, 0x4e, 0x47]
        );
    }

    #[tokio::test]
    async fn generated_document_cache_hit_returns_bytes_while_wal_writer_holds_lru_lock() {
        let root = TempRoot::new();
        let database = seed(&root, b"attachment seed");
        let layout = shared_state_layout(root.text().to_owned()).expect("layout");
        let document = ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
            view: ChatListKind::Main,
            item: CanonicalKey::GeneratedDoc(GeneratedDocKey {
                chat: chat_key(),
                partition: DocPartition::Chat,
                format: DocFormat::Json,
                schema_family: SchemaFamily(1),
            }),
        })
        .id();
        let current = Path::new(&layout.cache_dir).join("generated/7/1/100/current/chat.json");
        let orphan = Path::new(&layout.cache_dir).join("generated/7/1/100/orphan/chat.json");
        fs::create_dir_all(current.parent().expect("current parent")).expect("current directory");
        fs::create_dir_all(orphan.parent().expect("orphan parent")).expect("orphan directory");
        let bytes = b"{\"schema\":\"gramdrive.chat\"}\n";
        fs::write(&current, bytes).expect("current bytes");
        fs::write(&orphan, b"stale generation").expect("orphan bytes");
        let content_version = ContentVersion::new("chat-json-v1").expect("version");
        let mut store = StateStore::open(&database).expect("state");
        let tx = store.write_txn().expect("write");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: document.clone(),
            parent: Some(root_id()),
            display_name: ".chat.json".to_owned(),
            safe_name: ".chat.json".to_owned(),
            metadata_version: gramdrive_model::version::MetadataVersion::new("chat-json-m1")
                .expect("metadata version"),
            content: Some(FileFacts {
                mime_type: Some("application/json".to_owned()),
                logical_size: Some(bytes.len() as u64),
                content_version: Some(content_version.clone()),
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: Some(10),
            modified_at_ms: Some(20),
            deleted_at_ms: None,
        })
        .expect("generated item");
        tx.upsert_cache_entry(&CacheEntryRecord {
            item: document.clone(),
            account: scope().account,
            content_version: content_version.clone(),
            kind: CacheKind::GeneratedDoc,
            size: bytes.len() as u64,
            blob_hash: None,
            verification: CacheVerification::Verified,
            pin: None,
            last_access_at_ms: 20,
            materialized_at_ms: 20,
            materialization_ref: Some(current.to_string_lossy().into_owned()),
        })
        .expect("cache entry");
        tx.commit().expect("commit");
        drop(store);

        let hydrator = Hydrator::shared(root.text()).expect("startup reconciliation");
        assert!(current.is_file(), "the referenced generation is preserved");
        assert!(!orphan.exists(), "the unreferenced generation is reclaimed");
        assert!(
            !orphan.parent().expect("orphan parent").exists(),
            "empty orphan generation directories are pruned"
        );
        let materialized = hydrator
            .hydrate_inner(
                7,
                document.text().to_owned(),
                Some(content_version.as_str().to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("generated cache hit");
        assert_eq!(materialized.path, current.to_string_lossy());
        assert_eq!(
            fs::read(&materialized.path).expect("materialized bytes"),
            bytes
        );
        assert_eq!(materialized.byte_count, bytes.len() as u64);
        assert!(
            !hydrator.sources.has_account(7),
            "generated hydration never requires a Telegram session"
        );

        // Make the next hit eligible for the coarsened touch so the test
        // exercises the zero-timeout write attempt rather than the ordinary
        // one-minute coarsening fast path.
        let mut reset = StateStore::open(&database).expect("state before contention");
        let reset_tx = reset.write_txn().expect("reset LRU timestamp");
        reset_tx
            .touch_cache_entry(&document, 1)
            .expect("reset cache entry timestamp");
        reset_tx.commit().expect("commit timestamp reset");
        drop(reset);

        let mut before = StateStore::open(&database).expect("state before contention");
        let last_access_before_contention = before
            .read_txn()
            .expect("read before contention")
            .cache_entry(&document)
            .expect("cache query")
            .expect("cache entry")
            .last_access_at_ms;
        drop(before);

        // This is the File Provider/agent WAL shape: another connection owns
        // an IMMEDIATE write transaction for longer than the ordinary five
        // second busy timeout. A generated document can only come from this
        // cache, so its already verified bytes must still be returned.
        let (writer_locked, writer_ready) = std::sync::mpsc::sync_channel(1);
        let writer_database = database.clone();
        let writer = std::thread::spawn(move || {
            let mut writer = StateStore::open(&writer_database).expect("contending writer");
            let held_writer = writer.write_txn().expect("hold writer lock");
            writer_locked.send(()).expect("signal held writer lock");
            std::thread::sleep(StdDuration::from_secs(6));
            drop(held_writer);
        });
        writer_ready
            .recv_timeout(StdDuration::from_secs(1))
            .expect("writer must hold the WAL lock before hydration");
        let started = Instant::now();
        let contended = hydrator
            .hydrate_inner(
                7,
                document.text().to_owned(),
                Some(content_version.as_str().to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("contended generated cache hit must not cannotSynchronize");
        assert!(
            started.elapsed() < StdDuration::from_secs(1),
            "best-effort LRU touch must not wait for SQLite's five-second busy timeout"
        );
        assert_eq!(contended.path, current.to_string_lossy());
        assert_eq!(
            fs::read(&contended.path).expect("contended cache bytes"),
            bytes
        );
        writer.join().expect("contending writer thread");

        let mut after = StateStore::open(&database).expect("state after contention");
        let read = after.read_txn().expect("read after contention");
        let cached = read
            .cache_entry(&document)
            .expect("cache query")
            .expect("cache entry");
        assert_eq!(
            cached.last_access_at_ms, last_access_before_contention,
            "the contended best-effort touch leaves durable LRU/accounting unchanged"
        );
        assert_eq!(
            read.cache_totals().expect("cache totals").total_bytes,
            bytes.len() as u64,
            "a skipped touch does not affect bounded cache accounting"
        );
        drop(read);
        drop(after);

        // The FFI hand-off owns two independent leases here — the ordinary
        // cache hit and the WAL-contended hit. Advance the durable publication
        // to a replacement generation, then prove neither lease exposes a
        // disappearing staged path and releasing the final reference reclaims
        // the old immutable generation.
        let replacement =
            Path::new(&layout.cache_dir).join("generated/7/1/100/replacement/chat.json");
        let replacement_bytes = b"{\"schema\":\"gramdrive.chat\",\"generation\":2}\n";
        fs::create_dir_all(replacement.parent().expect("replacement parent"))
            .expect("replacement directory");
        fs::write(&replacement, replacement_bytes).expect("replacement bytes");
        let replacement_version = ContentVersion::new("chat-json-v2").expect("replacement version");
        let mut publication = StateStore::open(&database).expect("state for replacement");
        let tx = publication.write_txn().expect("replacement transaction");
        tx.update_item_content(
            &document,
            Some(&content_version),
            &FileFacts {
                mime_type: Some("application/json".to_owned()),
                logical_size: Some(replacement_bytes.len() as u64),
                content_version: Some(replacement_version.clone()),
            },
            &gramdrive_model::version::MetadataVersion::new("chat-json-m2")
                .expect("replacement metadata version"),
            30,
        )
        .expect("advance generated publication");
        tx.upsert_cache_entry(&CacheEntryRecord {
            item: document.clone(),
            account: scope().account,
            content_version: replacement_version,
            kind: CacheKind::GeneratedDoc,
            size: replacement_bytes.len() as u64,
            blob_hash: None,
            verification: CacheVerification::Verified,
            pin: None,
            last_access_at_ms: 30,
            materialized_at_ms: 30,
            materialization_ref: Some(replacement.to_string_lossy().into_owned()),
        })
        .expect("replace generated cache locator");
        tx.commit().expect("commit replacement publication");
        let base = current
            .parent()
            .and_then(Path::parent)
            .expect("generated base")
            .to_path_buf();
        reclaim_unreferenced_generations(&mut publication, &base)
            .expect("reclaim after replacement");
        drop(publication);

        assert_eq!(
            fs::read(&current).expect("first staged bytes while leased"),
            bytes,
            "replacement cannot remove or change a leased File Provider source"
        );
        assert_eq!(
            fs::read(&replacement).expect("replacement bytes"),
            replacement_bytes
        );
        let first_lease = materialized
            .lease_id
            .clone()
            .expect("first generated lease id");
        let second_lease = contended
            .lease_id
            .clone()
            .expect("second generated lease id");
        hydrator
            .release_hydration_lease(first_lease.clone())
            .expect("release first generated hand-off");
        assert!(
            current.exists(),
            "one remaining native clone reference retains the obsolete generation"
        );
        hydrator
            .release_hydration_lease(second_lease)
            .expect("release final generated hand-off");
        assert!(
            !current.exists(),
            "releasing the last hand-off lease reclaims the obsolete generation"
        );
        assert!(
            replacement.exists(),
            "the current publication remains claimed"
        );
        hydrator
            .release_hydration_lease(first_lease)
            .expect("duplicate teardown is bounded and idempotent");
    }

    #[tokio::test]
    async fn same_base_reclaim_snapshots_before_reserving_and_hydration_stays_bounded() {
        use gramdrive_engine::render_pipeline::{
            set_reclaim_before_snapshot_hook, set_reclaim_before_unlink_hook,
        };

        let root = TempRoot::new();
        let database = seed(&root, b"attachment seed");
        let layout = shared_state_layout(root.text().to_owned()).expect("layout");
        let document = ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
            view: ChatListKind::Main,
            item: CanonicalKey::GeneratedDoc(GeneratedDocKey {
                chat: chat_key(),
                partition: DocPartition::Chat,
                format: DocFormat::Json,
                schema_family: SchemaFamily(1),
            }),
        })
        .id();
        let base = Path::new(&layout.cache_dir).join("generated/7/1/100");
        let current = base.join("current/chat.json");
        fs::create_dir_all(current.parent().expect("current parent")).expect("current directory");
        let bytes = b"{\"schema\":\"gramdrive.chat\",\"generation\":1}\n";
        fs::write(&current, bytes).expect("current bytes");
        let content_version = ContentVersion::new("chat-json-v1").expect("version");
        let mut store = StateStore::open(&database).expect("state");
        let tx = store.write_txn().expect("generated fixture transaction");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: document.clone(),
            parent: Some(root_id()),
            display_name: ".chat.json".to_owned(),
            safe_name: ".chat.json".to_owned(),
            metadata_version: gramdrive_model::version::MetadataVersion::new("chat-json-m1")
                .expect("metadata version"),
            content: Some(FileFacts {
                mime_type: Some("application/json".to_owned()),
                logical_size: Some(bytes.len() as u64),
                content_version: Some(content_version.clone()),
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: Some(10),
            modified_at_ms: Some(20),
            deleted_at_ms: None,
        })
        .expect("generated item");
        tx.upsert_cache_entry(&CacheEntryRecord {
            item: document.clone(),
            account: scope().account,
            content_version: content_version.clone(),
            kind: CacheKind::GeneratedDoc,
            size: bytes.len() as u64,
            blob_hash: None,
            verification: CacheVerification::Verified,
            pin: None,
            last_access_at_ms: 20,
            materialized_at_ms: 20,
            materialization_ref: Some(current.to_string_lossy().into_owned()),
        })
        .expect("generated cache entry");
        tx.commit().expect("commit generated fixture");
        drop(store);
        let hydrator = Hydrator::shared(root.text()).expect("production hydrator");

        let stale = base.join("stale-before-snapshot/chat.json");
        fs::create_dir_all(stale.parent().expect("stale parent")).expect("stale directory");
        fs::write(&stale, b"stale").expect("stale bytes");
        let (snapshot_arrived, snapshot_arrived_wait) = std::sync::mpsc::sync_channel(0);
        let (snapshot_release, snapshot_release_wait) = std::sync::mpsc::sync_channel(0);
        let snapshot_release_wait = Arc::new(Mutex::new(snapshot_release_wait));
        let hooked_base = base.clone();
        set_reclaim_before_snapshot_hook(Some(Arc::new(move |candidate_base| {
            if candidate_base == hooked_base {
                snapshot_arrived
                    .send(())
                    .expect("announce snapshot boundary");
                snapshot_release_wait
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .recv()
                    .expect("release snapshot boundary");
            }
        })));
        let reclaim_database = database.clone();
        let reclaim_base = base.clone();
        let reclaim = std::thread::spawn(move || {
            let mut store = StateStore::open(&reclaim_database).expect("reclaim state");
            reclaim_unreferenced_generations(&mut store, &reclaim_base)
                .expect("database-first reclaim");
        });
        snapshot_arrived_wait
            .recv_timeout(StdDuration::from_secs(1))
            .expect("reclaim reaches pre-SQLite snapshot boundary");

        let unrelated_base = Path::new(&layout.cache_dir).join("generated/7/1/200");
        let unrelated_stale = unrelated_base.join("stale/chat.json");
        fs::create_dir_all(unrelated_stale.parent().expect("unrelated parent"))
            .expect("unrelated directory");
        fs::write(&unrelated_stale, b"unrelated stale").expect("unrelated bytes");
        let mut unrelated_store = StateStore::open(&database).expect("unrelated state");
        reclaim_unreferenced_generations(&mut unrelated_store, &unrelated_base)
            .expect("unrelated background reclaim");
        assert!(
            !unrelated_stale.exists(),
            "unrelated background work progresses while the target snapshot is pinned"
        );

        let started = Instant::now();
        let hydrated = tokio::time::timeout(
            StdDuration::from_millis(500),
            hydrator.hydrate_inner(
                7,
                document.text().to_owned(),
                Some(content_version.as_str().to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("same-base hydration remains below 500 ms")
        .expect("pre-reservation hydration returns exact cache bytes");
        assert!(started.elapsed() < StdDuration::from_millis(500));
        assert_eq!(
            fs::read(&hydrated.path).expect("exact hydrated bytes"),
            bytes
        );
        snapshot_release.send(()).expect("release snapshot reclaim");
        reclaim.join().expect("snapshot reclaim thread");
        set_reclaim_before_snapshot_hook(None);
        assert!(!stale.exists(), "standalone reclaim eventually progresses");

        let reserved_stale = base.join("stale-under-reservation/chat.json");
        fs::create_dir_all(reserved_stale.parent().expect("reserved stale parent"))
            .expect("reserved stale directory");
        fs::write(&reserved_stale, b"reserved stale").expect("reserved stale bytes");
        let (unlink_arrived, unlink_arrived_wait) = std::sync::mpsc::sync_channel(0);
        let (unlink_release, unlink_release_wait) = std::sync::mpsc::sync_channel(0);
        let unlink_release_wait = Arc::new(Mutex::new(unlink_release_wait));
        let hooked_stale = reserved_stale.clone();
        set_reclaim_before_unlink_hook(Some(Arc::new(move |path| {
            if path == hooked_stale {
                unlink_arrived.send(()).expect("announce reserved unlink");
                unlink_release_wait
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .recv()
                    .expect("release reserved unlink");
            }
        })));
        let reclaim_database = database.clone();
        let reclaim_base = base.clone();
        let reserved_reclaim = std::thread::spawn(move || {
            let mut store = StateStore::open(&reclaim_database).expect("reserved reclaim state");
            reclaim_unreferenced_generations(&mut store, &reclaim_base).expect("reserved reclaim");
        });
        unlink_arrived_wait
            .recv_timeout(StdDuration::from_secs(1))
            .expect("reclaim owns same-base reservation");

        let busy_started = Instant::now();
        let busy = hydrator
            .hydrate_inner(
                7,
                document.text().to_owned(),
                Some(content_version.as_str().to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect_err("long reservation returns a typed refresh retry");
        assert!(matches!(busy, DriveError::VersionConflict { .. }));
        assert!(busy_started.elapsed() < StdDuration::from_millis(500));

        let cancel_token = CancellationToken::new();
        let cancel_from_thread = Arc::clone(&cancel_token);
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(StdDuration::from_millis(20));
            cancel_from_thread.cancel();
        });
        let cancel_started = Instant::now();
        let cancelled = hydrator
            .hydrate_inner(
                7,
                document.text().to_owned(),
                Some(content_version.as_str().to_owned()),
                Arc::new(NoopProgress),
                cancel_token,
            )
            .await
            .expect_err("cancellation interrupts the bounded reservation wait");
        assert!(matches!(cancelled, DriveError::Cancelled { .. }));
        assert!(cancel_started.elapsed() < StdDuration::from_millis(100));
        canceller.join().expect("canceller thread");

        unlink_release.send(()).expect("release reserved reclaim");
        reserved_reclaim.join().expect("reserved reclaim thread");
        set_reclaim_before_unlink_hook(None);
        assert!(
            !reserved_stale.exists(),
            "reserved reclaim eventually unlinks"
        );
        assert_eq!(fs::read(&current).expect("current generated bytes"), bytes);
        hydrator
            .release_hydration_lease(hydrated.lease_id.expect("generated lease id"))
            .expect("release exact-byte hydration lease");
    }

    #[test]
    fn generated_cache_miss_after_publication_advance_is_a_version_conflict() {
        let root = TempRoot::new();
        let database = seed(&root, b"attachment seed");
        let document = ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
            view: ChatListKind::Main,
            item: CanonicalKey::GeneratedDoc(GeneratedDocKey {
                chat: chat_key(),
                partition: DocPartition::Month {
                    year: 2026,
                    month: 7,
                },
                format: DocFormat::Ndjson,
                schema_family: SchemaFamily(1),
            }),
        })
        .id();
        let first_version = ContentVersion::new("month-v1").expect("first version");
        let second_version = ContentVersion::new("month-v2").expect("second version");
        let mut store = StateStore::open(&database).expect("state");
        let tx = store.write_txn().expect("write");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: document.clone(),
            parent: Some(root_id()),
            display_name: "Messages.ndjson".to_owned(),
            safe_name: "Messages.ndjson".to_owned(),
            metadata_version: gramdrive_model::version::MetadataVersion::new("month-m1")
                .expect("metadata version"),
            content: Some(FileFacts {
                mime_type: Some("application/x-ndjson".to_owned()),
                logical_size: Some(5),
                content_version: Some(first_version.clone()),
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: Some(10),
            modified_at_ms: Some(10),
            deleted_at_ms: None,
        })
        .expect("generated item");
        tx.commit().expect("commit first publication");

        let admitted =
            admission(&mut store, AccountId(7), &document, None).expect("initial admission");
        assert_eq!(admitted.version, first_version);
        assert!(admitted.cache_only);

        let tx = store.write_txn().expect("advance publication");
        tx.update_item_content(
            &document,
            Some(&first_version),
            &FileFacts {
                mime_type: Some("application/x-ndjson".to_owned()),
                logical_size: Some(6),
                content_version: Some(second_version),
            },
            &gramdrive_model::version::MetadataVersion::new("month-m2").expect("metadata version"),
            20,
        )
        .expect("advance item");
        tx.commit().expect("commit advance");

        assert!(matches!(
            generated_cache_miss_error(&mut store, AccountId(7), &document, &admitted.version,),
            DriveError::VersionConflict { .. }
        ));
    }

    #[tokio::test]
    async fn production_archive_worker_is_independent_from_audit_and_obeys_host_gates() {
        let audit_root = TempRoot::new();
        let audit_bytes = b"Audit metadata does not create download demand".to_vec();
        let audit_database = seed(&audit_root, &audit_bytes);
        configure_archive(&audit_database, RetentionMode::Audit, false);
        let audit_hydrator = Hydrator::shared(audit_root.text()).expect("Audit hydrator");
        let audit_source = Arc::new(BytesSource {
            bytes: audit_bytes,
            fetches: AtomicUsize::new(0),
        });
        audit_hydrator
            .sources
            .register(7, 1, Arc::clone(&audit_source) as Arc<dyn ContentSource>);
        assert!(
            !audit_hydrator
                .schedule_archive_backfill(
                    BackfillScheduler::with_defaults(),
                    scope(),
                    HostConditions::UNCONSTRAINED,
                )
                .expect("Audit-only scheduler")
        );
        assert_eq!(audit_source.fetches.load(Ordering::SeqCst), 0);

        let archive_root = TempRoot::new();
        let archive_bytes = b"Archive Mode eagerly materializes allowed bytes".to_vec();
        let archive_database = seed(&archive_root, &archive_bytes);
        configure_archive(&archive_database, RetentionMode::Audit, true);
        let archive_hydrator = Hydrator::shared(archive_root.text()).expect("Archive hydrator");
        let archive_source = Arc::new(BytesSource {
            bytes: archive_bytes,
            fetches: AtomicUsize::new(0),
        });
        archive_hydrator.sources.register(
            7,
            1,
            Arc::clone(&archive_source) as Arc<dyn ContentSource>,
        );
        assert!(
            !archive_hydrator
                .schedule_archive_backfill(
                    BackfillScheduler::with_defaults(),
                    scope(),
                    HostConditions {
                        network: gramdrive_engine::backfill::NetworkState::Online,
                        power: gramdrive_engine::backfill::PowerState::Unconstrained,
                        disk: gramdrive_engine::backfill::DiskState::Low,
                    },
                )
                .expect("low-disk scheduler")
        );
        assert_eq!(archive_source.fetches.load(Ordering::SeqCst), 0);
        assert!(
            archive_hydrator
                .schedule_archive_backfill(
                    BackfillScheduler::with_defaults(),
                    scope(),
                    HostConditions::UNCONSTRAINED,
                )
                .expect("Archive scheduler")
        );
        wait_for_fetches(&archive_source, 1).await;
        tokio::time::timeout(StdDuration::from_secs(3), async {
            loop {
                let mut store = StateStore::open(&archive_database).expect("state");
                if store
                    .read_txn()
                    .expect("read")
                    .cache_entry(&item_id())
                    .expect("cache")
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .expect("Archive materialization");

        let restricted_root = TempRoot::new();
        let restricted_bytes = b"restricted content must not fetch".to_vec();
        let restricted_database = seed(&restricted_root, &restricted_bytes);
        {
            let mut store = StateStore::open(&restricted_database).expect("state");
            let tx = store.write_txn().expect("restrict transaction");
            let mut facts = tx
                .read()
                .attachment(&attachment_key())
                .expect("attachment")
                .expect("facts")
                .facts;
            facts.can_be_saved = false;
            tx.upsert_attachment(&facts).expect("restrict attachment");
            tx.commit().expect("commit restriction");
        }
        configure_archive(&restricted_database, RetentionMode::Audit, true);
        let restricted_hydrator =
            Hydrator::shared(restricted_root.text()).expect("restricted hydrator");
        let restricted_source = Arc::new(BytesSource {
            bytes: restricted_bytes,
            fetches: AtomicUsize::new(0),
        });
        restricted_hydrator.sources.register(
            7,
            1,
            Arc::clone(&restricted_source) as Arc<dyn ContentSource>,
        );
        assert!(
            !restricted_hydrator
                .schedule_archive_backfill(
                    BackfillScheduler::with_defaults(),
                    scope(),
                    HostConditions::UNCONSTRAINED,
                )
                .expect("restricted scheduler")
        );
        assert_eq!(restricted_source.fetches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn production_archive_worker_fetches_allowed_persistent_story_only() {
        let root = TempRoot::new();
        let bytes = b"persistent profile story bytes".to_vec();
        let database = seed_story(&root, &bytes);
        let month_item = transition_story_to_month(&database);
        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("exclude attachment transaction");
            let mut attachment = tx
                .read()
                .attachment(&attachment_key())
                .expect("attachment")
                .expect("facts")
                .facts;
            attachment.availability = StateAttachmentAvailability::Unavailable;
            tx.upsert_attachment(&attachment)
                .expect("exclude attachment bytes");
            tx.tombstone_item(
                &item_id(),
                12,
                &gramdrive_model::version::MetadataVersion::new("attachment-removed")
                    .expect("version"),
            )
            .expect("remove attachment item");
            tx.commit().expect("commit exclusion");
        }
        configure_archive(&database, RetentionMode::Audit, true);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let source = Arc::new(BytesSource {
            bytes,
            fetches: AtomicUsize::new(0),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);
        assert!(
            hydrator
                .schedule_archive_backfill(
                    BackfillScheduler::with_defaults(),
                    scope(),
                    HostConditions::UNCONSTRAINED,
                )
                .expect("story scheduler")
        );
        wait_for_fetches(&source, 1).await;
        tokio::time::timeout(StdDuration::from_secs(3), async {
            loop {
                let mut store = StateStore::open(&database).expect("state");
                if store
                    .read_txn()
                    .expect("read")
                    .cache_entry(&month_item)
                    .expect("cache")
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(StdDuration::from_millis(10)).await;
            }
        })
        .await
        .expect("story materialization");
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn done_waits_for_its_own_cache_publication_despite_unrelated_notifications() {
        let root = TempRoot::new();
        let bytes = b"published only after the causal completion".to_vec();
        let database = seed(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let source = Arc::new(BytesSource {
            bytes: bytes.clone(),
            fetches: AtomicUsize::new(0),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);

        let (checkpoint_send, checkpoint_receive) = std::sync::mpsc::channel();
        let (release_send, release_receive) = std::sync::mpsc::sync_channel(0);
        *hydrator
            .promotion_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(BlockingPromotionProbe {
            checkpoints: checkpoint_send,
            release: Mutex::new(release_receive),
        }));

        let task_hydrator = Arc::clone(&hydrator);
        let hydration = tokio::spawn(async move {
            task_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        assert_eq!(
            checkpoint_receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("engine completion checkpoint"),
            TransferId(1)
        );
        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read");
        assert_eq!(
            read.transfer(TransferId(1))
                .expect("transfer query")
                .expect("transfer")
                .state,
            TransferState::Done
        );
        assert_eq!(read.cache_entry(&item_id()).expect("cache query"), None);
        drop(read);

        hydrator.changed.send_modify(|sequence| {
            *sequence = sequence.wrapping_add(1);
        });
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            !hydration.is_finished(),
            "an unrelated notification cannot make Done look published"
        );

        release_send.send(()).expect("release publication");
        let materialized = tokio::time::timeout(std::time::Duration::from_secs(2), hydration)
            .await
            .expect("hydration timeout")
            .expect("hydration task")
            .expect("hydration result");
        assert_eq!(fs::read(materialized.path).expect("cache bytes"), bytes);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn promotion_failure_resolves_waiter_without_publishing_partial_content() {
        let root = TempRoot::new();
        let bytes = b"publication must fail closed".to_vec();
        let database = seed(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        hydrator.sources.register(
            7,
            1,
            Arc::new(BytesSource {
                bytes,
                fetches: AtomicUsize::new(0),
            }),
        );
        let (checkpoint_send, checkpoint_receive) = std::sync::mpsc::channel();
        let (release_send, release_receive) = std::sync::mpsc::sync_channel(0);
        *hydrator
            .promotion_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(BlockingPromotionProbe {
            checkpoints: checkpoint_send,
            release: Mutex::new(release_receive),
        }));

        let task_hydrator = Arc::clone(&hydrator);
        let hydration = tokio::spawn(async move {
            task_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        let transfer = checkpoint_receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("engine completion checkpoint");
        let staging = StateStore::open(&database)
            .expect("state")
            .read_txn()
            .expect("read")
            .transfer(transfer)
            .expect("transfer query")
            .expect("transfer")
            .temp_ref
            .expect("staging reference");
        fs::remove_file(staging).expect("remove staged bytes");
        release_send.send(()).expect("release publication");

        let error = tokio::time::timeout(std::time::Duration::from_secs(2), hydration)
            .await
            .expect("failure timeout")
            .expect("hydration task")
            .expect_err("promotion must fail");
        assert!(matches!(error, DriveError::Integrity { .. }));
        let mut store = StateStore::open(&database).expect("state");
        assert_eq!(
            store
                .read_txn()
                .expect("read")
                .cache_entry(&item_id())
                .expect("cache query"),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn producer_first_success_reaches_reserved_waiter_and_cleans_registry() {
        let root = TempRoot::new();
        let first_bytes = b"unrelated driver bytes".to_vec();
        let second_bytes = b"producer-first exact bytes".to_vec();
        let database = seed(&root, &first_bytes);
        seed_second(&database, &second_bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let started = Arc::new(AtomicUsize::new(0));
        let first_gate = Arc::new(AsyncGate::default());
        hydrator.sources.register(
            7,
            1,
            Arc::new(TwoItemGatedSource {
                first_item: item_id().text().to_owned(),
                first_bytes: first_bytes.clone(),
                second_bytes: second_bytes.clone(),
                started: Arc::clone(&started),
                first_gate: Arc::clone(&first_gate),
            }),
        );

        let (bind_send, bind_receive) = std::sync::mpsc::channel();
        let (bind_release, bind_wait) = std::sync::mpsc::sync_channel(0);
        *hydrator
            .materialization_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(Arc::new(BlockingMaterializationProbe {
                target: TransferId(2),
                checkpoints: bind_send,
                release: Mutex::new(bind_wait),
            }));

        let first_hydrator = Arc::clone(&hydrator);
        let first = tokio::spawn(async move {
            first_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_test(|| started.load(Ordering::SeqCst) == 1).await;

        let second_hydrator = Arc::clone(&hydrator);
        let second = tokio::spawn(async move {
            second_hydrator
                .hydrate_inner(
                    7,
                    second_item_id().text().to_owned(),
                    Some("content-v2".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        assert_eq!(
            bind_receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("second opener reaches pre-bind checkpoint"),
            TransferId(2)
        );
        first_gate.release();

        wait_for_test(|| {
            let mut store = StateStore::open(&database).expect("state");
            store
                .read_txn()
                .expect("read")
                .cache_entry(&second_item_id())
                .expect("cache query")
                .is_some()
        })
        .await;
        assert!(
            !second.is_finished(),
            "the producer published before the opener bound its waiter"
        );
        bind_release.send(()).expect("release pre-bind checkpoint");

        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(first, second)
        })
        .await
        .expect("producer-first success timeout");
        let first = first
            .expect("first hydration task")
            .expect("first hydration result");
        let second = second
            .expect("second hydration task")
            .expect("second hydration result");
        assert_eq!(
            fs::read(first.path).expect("first cache bytes"),
            first_bytes
        );
        assert_eq!(
            fs::read(second.path).expect("second cache bytes"),
            second_bytes
        );
        let retained: Vec<_> = {
            let registry = hydrator
                .materializations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            assert!(
                registry
                    .by_transfer
                    .values()
                    .all(|entry| entry.is_resolved())
            );
            registry
                .by_transfer
                .iter()
                .map(|(transfer, completion)| (*transfer, Arc::downgrade(completion)))
                .collect()
        };
        for (transfer, completion) in retained {
            expire_materialization(&hydrator.materializations, transfer, &completion);
        }
        let registry = hydrator
            .materializations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(registry.by_transfer.is_empty());
        assert!(registry.pending.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn coalesced_opener_binds_the_retained_result_after_producer_completion() {
        let root = TempRoot::new();
        let bytes = b"one producer two late binders".to_vec();
        let database = seed(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let started = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(AsyncGate::default());
        let source = Arc::new(GatedSource {
            bytes: bytes.clone(),
            fetches: AtomicUsize::new(0),
            started: Arc::clone(&started),
            gate: Arc::clone(&gate),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);

        let first_hydrator = Arc::clone(&hydrator);
        let first = tokio::spawn(async move {
            first_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_test(|| started.load(Ordering::SeqCst) == 1).await;

        let (bind_send, bind_receive) = std::sync::mpsc::channel();
        let (bind_release, bind_wait) = std::sync::mpsc::sync_channel(0);
        *hydrator
            .materialization_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(Arc::new(BlockingMaterializationProbe {
                target: TransferId(1),
                checkpoints: bind_send,
                release: Mutex::new(bind_wait),
            }));
        let second_hydrator = Arc::clone(&hydrator);
        let second = tokio::spawn(async move {
            second_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        assert_eq!(
            bind_receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("coalesced opener reaches pre-bind checkpoint"),
            TransferId(1)
        );
        assert_eq!(hydrator.coordinator.reader_count(TransferId(1)), 2);
        gate.release();
        wait_for_test(|| {
            let mut store = StateStore::open(&database).expect("state");
            store
                .read_txn()
                .expect("read")
                .cache_entry(&item_id())
                .expect("cache query")
                .is_some()
        })
        .await;
        assert!(
            !second.is_finished(),
            "the coalesced opener remains paused after producer completion"
        );
        bind_release.send(()).expect("release coalesced bind");

        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(first, second)
        })
        .await
        .expect("coalesced producer-first timeout");
        let first = first.expect("first task").expect("first hydration");
        let second = second.expect("second task").expect("second hydration");
        assert_eq!(first.path, second.path);
        assert_eq!(fs::read(second.path).expect("cache bytes"), bytes);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn producer_first_failure_reaches_reserved_waiter_without_partial_publication() {
        let root = TempRoot::new();
        let first_bytes = b"unrelated successful bytes".to_vec();
        let second_bytes = b"producer-first failed bytes".to_vec();
        let database = seed(&root, &first_bytes);
        seed_second(&database, &second_bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let started = Arc::new(AtomicUsize::new(0));
        let first_gate = Arc::new(AsyncGate::default());
        hydrator.sources.register(
            7,
            1,
            Arc::new(TwoItemGatedSource {
                first_item: item_id().text().to_owned(),
                first_bytes,
                second_bytes,
                started: Arc::clone(&started),
                first_gate: Arc::clone(&first_gate),
            }),
        );

        let (bind_send, bind_receive) = std::sync::mpsc::channel();
        let (bind_release, bind_wait) = std::sync::mpsc::sync_channel(0);
        *hydrator
            .materialization_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            Some(Arc::new(BlockingMaterializationProbe {
                target: TransferId(2),
                checkpoints: bind_send,
                release: Mutex::new(bind_wait),
            }));
        let (promotion_send, promotion_receive) = std::sync::mpsc::channel();
        let (promotion_release, promotion_wait) = std::sync::mpsc::sync_channel(0);
        *hydrator
            .promotion_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(SelectivePromotionProbe {
            target: TransferId(2),
            checkpoints: promotion_send,
            release: Mutex::new(promotion_wait),
        }));

        let first_hydrator = Arc::clone(&hydrator);
        let first = tokio::spawn(async move {
            first_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_test(|| started.load(Ordering::SeqCst) == 1).await;

        let second_hydrator = Arc::clone(&hydrator);
        let second = tokio::spawn(async move {
            second_hydrator
                .hydrate_inner(
                    7,
                    second_item_id().text().to_owned(),
                    Some("content-v2".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        assert_eq!(
            bind_receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("second opener reaches pre-bind checkpoint"),
            TransferId(2)
        );
        first_gate.release();
        let transfer = promotion_receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("existing driver reaches second publication");
        assert_eq!(transfer, TransferId(2));
        let staging = StateStore::open(&database)
            .expect("state")
            .read_txn()
            .expect("read")
            .transfer(transfer)
            .expect("transfer query")
            .expect("transfer")
            .temp_ref
            .expect("staging reference");
        fs::remove_file(&staging).expect("remove staged bytes");
        promotion_release
            .send(())
            .expect("release failed publication");
        wait_for_test(|| {
            let registry = hydrator
                .materializations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            registry
                .by_transfer
                .get(&TransferId(2))
                .is_some_and(|completion| completion.is_resolved())
        })
        .await;
        assert!(
            !second.is_finished(),
            "the producer failed before the opener bound its waiter"
        );
        bind_release.send(()).expect("release pre-bind checkpoint");

        let first = tokio::time::timeout(std::time::Duration::from_secs(2), first)
            .await
            .expect("first hydration timeout")
            .expect("first hydration task")
            .expect("first hydration result");
        assert_eq!(
            fs::read(first.path).expect("first cache bytes"),
            b"unrelated successful bytes"
        );
        let error = tokio::time::timeout(std::time::Duration::from_secs(2), second)
            .await
            .expect("producer-first failure timeout")
            .expect("second hydration task")
            .expect_err("second publication must fail");
        assert!(matches!(error, DriveError::Integrity { .. }));

        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read");
        assert_eq!(
            read.cache_entry(&second_item_id()).expect("cache query"),
            None
        );
        assert_eq!(
            read.attachment(&second_attachment_key())
                .expect("attachment query")
                .expect("attachment")
                .blob_hash,
            None
        );
        assert!(!Path::new(&staging).exists());
    }

    #[tokio::test]
    async fn cached_bytes_obey_later_chat_protection_deletion_and_leave_policy() {
        let root = TempRoot::new();
        let bytes = b"cached before policy changed".to_vec();
        let database = seed(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let source = Arc::new(BytesSource {
            bytes,
            fetches: AtomicUsize::new(0),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);
        hydrator
            .hydrate_inner(
                7,
                item_id().text().to_owned(),
                Some("content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("initial materialization");

        let update_chat = |protected: bool, left: Option<i64>, deleted: Option<i64>| {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("write");
            let mut chat = tx.read().chat(&chat_key()).expect("query").expect("chat");
            chat.is_protected = protected;
            chat.left_at_ms = left;
            chat.deleted_at_ms = deleted;
            tx.upsert_chat(&chat).expect("update chat policy");
            tx.touch_cache_entry(&item_id(), 7)
                .expect("set cache access sentinel");
            tx.commit().expect("commit chat policy");
        };
        let assert_untouched = || {
            let mut store = StateStore::open(&database).expect("state");
            assert_eq!(
                store
                    .read_txn()
                    .expect("read")
                    .cache_entry(&item_id())
                    .expect("cache query")
                    .expect("cache entry")
                    .last_access_at_ms,
                7
            );
            assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
        };

        update_chat(true, None, None);
        assert!(matches!(
            hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await,
            Err(DriveError::Restricted { .. })
        ));
        assert_untouched();

        update_chat(false, Some(10), None);
        assert!(matches!(
            hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await,
            Err(DriveError::SourceUnavailable { .. })
        ));
        assert_untouched();

        update_chat(false, None, Some(11));
        assert!(matches!(
            hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await,
            Err(DriveError::SourceUnavailable { .. })
        ));
        assert_untouched();
    }

    #[tokio::test]
    async fn authoritative_restrictions_purge_same_index_audit_history() {
        for archive_mode in [false, true] {
            for restriction in 0_u8..4 {
                let root = TempRoot::new();
                let bytes = format!(
                    "Audit same-index history archive={archive_mode} restriction={restriction}"
                )
                .into_bytes();
                let database = seed(&root, &bytes);
                configure_archive(&database, RetentionMode::Audit, archive_mode);
                let hydrator = Hydrator::shared(root.text()).expect("hydrator");
                hydrator.sources.register(
                    7,
                    1,
                    Arc::new(BytesSource {
                        bytes,
                        fetches: AtomicUsize::new(0),
                    }),
                );
                let materialized = hydrator
                    .hydrate_inner(
                        7,
                        item_id().text().to_owned(),
                        Some("content-v1".to_owned()),
                        Arc::new(NoopProgress),
                        CancellationToken::new(),
                    )
                    .await
                    .expect("materialize first allowed Audit version");

                {
                    let mut store = StateStore::open(&database).expect("state");
                    let tx = store.write_txn().expect("Audit replacements");
                    let mut second = tx
                        .read()
                        .attachment(&attachment_key())
                        .expect("attachment query")
                        .expect("first attachment")
                        .facts;
                    second.content_version =
                        ContentVersion::new("content-v2").expect("second content version");
                    second.source_name = Some("payload-v2.bin".to_owned());
                    tx.replace_message_attachments(
                        &attachment_key().message,
                        std::slice::from_ref(&second),
                        20,
                    )
                    .expect("retain materialized first version");
                    let mut third = second;
                    third.content_version =
                        ContentVersion::new("content-v3").expect("third content version");
                    third.source_name = Some("payload-v3.bin".to_owned());
                    tx.replace_message_attachments(
                        &attachment_key().message,
                        std::slice::from_ref(&third),
                        21,
                    )
                    .expect("retain metadata-only second version");
                    match restriction {
                        0 => {
                            let mut chat = tx
                                .read()
                                .chat(&chat_key())
                                .expect("chat query")
                                .expect("chat");
                            chat.is_protected = true;
                            tx.upsert_chat(&chat).expect("protect chat");
                        }
                        1 => {
                            third.can_be_saved = false;
                            tx.upsert_attachment(&third)
                                .expect("can_be_saved restriction");
                        }
                        2 => {
                            third.availability = StateAttachmentAvailability::ViewOnce;
                            tx.upsert_attachment(&third).expect("view-once restriction");
                        }
                        3 => {
                            let mut item = tx
                                .read()
                                .item(&item_id())
                                .expect("item query")
                                .expect("attachment item");
                            item.availability = ItemAvailability::Restricted;
                            item.metadata_version = gramdrive_model::version::MetadataVersion::new(
                                "restricted-history",
                            )
                            .expect("metadata version");
                            tx.upsert_item(&item).expect("restricted item projection");
                        }
                        _ => unreachable!("bounded restriction fixture"),
                    }
                    tx.commit().expect("commit restriction");

                    let read = store.read_txn().expect("read pre-relaunch state");
                    let retained = read
                        .retained_attachment_versions(&attachment_key())
                        .expect("retained versions");
                    assert_eq!(retained.len(), 2);
                    assert_eq!(
                        retained
                            .iter()
                            .filter(|version| version.materialization_ref.is_some())
                            .count(),
                        1
                    );
                    assert!(Path::new(&materialized.path).is_file());
                    assert!(
                        read.retention_purge_queue(scope().account, 10)
                            .expect("purge queue")
                            .is_empty(),
                        "source commit precedes production startup policy replay"
                    );
                }
                drop(hydrator);

                let relaunched =
                    Hydrator::shared(root.text()).expect("production relaunch applies restriction");
                assert!(
                    !Path::new(&materialized.path).exists(),
                    "archive_mode={archive_mode} restriction={restriction}"
                );
                let mut store = StateStore::open(&database).expect("state");
                let read = store.read_txn().expect("read restricted state");
                assert!(
                    read.retained_attachment_versions(&attachment_key())
                        .expect("retained versions")
                        .is_empty()
                );
                assert!(
                    read.retained_attachment_keys(scope().account)
                        .expect("retained attachment keys")
                        .is_empty()
                );
                assert!(
                    read.retention_purge_queue(scope().account, 10)
                        .expect("purge queue")
                        .is_empty()
                );
                let account = read
                    .account(scope().account)
                    .expect("account query")
                    .expect("account");
                assert_eq!(account.retention_mode, RetentionMode::Audit);
                assert_eq!(account.archive_mode, archive_mode);
                drop(read);
                drop(store);
                drop(relaunched);

                let _idempotent_relaunch =
                    Hydrator::shared(root.text()).expect("idempotent restriction replay");
                assert!(!Path::new(&materialized.path).exists());
            }
        }
    }

    #[tokio::test]
    async fn authoritative_attachment_restrictions_purge_materialized_bytes_in_every_mode() {
        for restriction in 0_u8..5 {
            let root = TempRoot::new();
            let bytes = format!("restricted attachment fixture {restriction}").into_bytes();
            let database = seed(&root, &bytes);
            if restriction != 0 {
                let mut store = StateStore::open(&database).expect("state");
                let tx = store.write_txn().expect("Audit plus Archive transaction");
                tx.set_retention_mode(scope().account, RetentionMode::Audit, None, 10)
                    .expect("enable Audit");
                tx.set_archive_mode(scope().account, true, 11)
                    .expect("enable Archive Mode");
                tx.commit().expect("commit Audit plus Archive");
            }

            let hydrator = Hydrator::shared(root.text()).expect("hydrator");
            hydrator.sources.register(
                7,
                1,
                Arc::new(BytesSource {
                    bytes,
                    fetches: AtomicUsize::new(0),
                }),
            );
            let materialized = hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
                .expect("materialize allowed bytes");
            let hash = {
                let mut store = StateStore::open(&database).expect("state");
                let read = store.read_txn().expect("read materialized state");
                if restriction != 0 {
                    assert!(read.pin(&item_id()).expect("Archive pin").is_some());
                }
                read.attachment(&attachment_key())
                    .expect("attachment query")
                    .expect("attachment")
                    .blob_hash
                    .expect("verified attachment blob")
            };

            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("restriction transaction");
            match restriction {
                0 => {
                    let mut facts = tx
                        .read()
                        .attachment(&attachment_key())
                        .expect("attachment query")
                        .expect("attachment")
                        .facts;
                    facts.can_be_saved = false;
                    tx.upsert_attachment(&facts)
                        .expect("can_be_saved restriction");
                }
                1 => {
                    let mut facts = tx
                        .read()
                        .attachment(&attachment_key())
                        .expect("attachment query")
                        .expect("attachment")
                        .facts;
                    facts.availability = StateAttachmentAvailability::Restricted;
                    tx.upsert_attachment(&facts)
                        .expect("restricted availability");
                }
                2 => {
                    let mut facts = tx
                        .read()
                        .attachment(&attachment_key())
                        .expect("attachment query")
                        .expect("attachment")
                        .facts;
                    facts.availability = StateAttachmentAvailability::ViewOnce;
                    tx.upsert_attachment(&facts).expect("view-once restriction");
                }
                3 => {
                    let mut chat = tx
                        .read()
                        .chat(&chat_key())
                        .expect("chat query")
                        .expect("chat");
                    chat.is_protected = true;
                    tx.upsert_chat(&chat).expect("chat protection");
                }
                4 => {
                    let mut item = tx
                        .read()
                        .item(&item_id())
                        .expect("item query")
                        .expect("attachment item");
                    item.availability = ItemAvailability::Restricted;
                    item.metadata_version =
                        gramdrive_model::version::MetadataVersion::new("restricted-item")
                            .expect("metadata version");
                    tx.upsert_item(&item).expect("restricted item projection");
                }
                _ => unreachable!("bounded restriction fixture"),
            }
            tx.commit().expect("commit restriction");
            drop(store);

            hydrator
                .purge_disallowed_attachment_materializations(scope().account)
                .expect("enforce attachment restriction");
            assert!(!Path::new(&materialized.path).exists());

            let mut store = StateStore::open(&database).expect("state");
            let read = store.read_txn().expect("read purged state");
            assert_eq!(read.cache_entry(&item_id()).expect("cache query"), None);
            assert_eq!(read.pin(&item_id()).expect("pin query"), None);
            assert_eq!(
                read.attachment(&attachment_key())
                    .expect("attachment query")
                    .expect("restricted metadata remains")
                    .blob_hash,
                None
            );
            assert_eq!(read.blob(scope().account, &hash).expect("blob query"), None);
            assert!(
                read.retention_purge_queue(scope().account, 10)
                    .expect("purge queue")
                    .is_empty()
            );
            let account = read
                .account(scope().account)
                .expect("account query")
                .expect("account");
            assert_eq!(
                account.retention_mode,
                if restriction == 0 {
                    RetentionMode::Mirror
                } else {
                    RetentionMode::Audit
                }
            );
            assert_eq!(account.archive_mode, restriction != 0);
            drop(read);
            hydrator
                .purge_disallowed_attachment_materializations(scope().account)
                .expect("idempotent restriction replay");
        }
    }

    #[tokio::test]
    async fn allowed_to_expired_purges_materialized_bytes_in_every_retention_archive_mode() {
        for (mode, archive_mode) in [
            (RetentionMode::Mirror, false),
            (RetentionMode::Audit, false),
            (RetentionMode::Audit, true),
        ] {
            let root = TempRoot::new();
            let bytes = format!("expired fixture {mode:?} archive={archive_mode}").into_bytes();
            let database = seed(&root, &bytes);
            {
                let mut store = StateStore::open(&database).expect("state");
                let tx = store.write_txn().expect("policy transaction");
                tx.set_retention_mode(scope().account, mode, None, 10)
                    .expect("set retention");
                tx.set_archive_mode(scope().account, archive_mode, 11)
                    .expect("set Archive Mode");
                tx.commit().expect("commit policy");
            }

            let hydrator = Hydrator::shared(root.text()).expect("hydrator");
            hydrator.sources.register(
                7,
                1,
                Arc::new(BytesSource {
                    bytes,
                    fetches: AtomicUsize::new(0),
                }),
            );
            let materialized = hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
                .expect("materialize allowed bytes");
            let hash = {
                let mut store = StateStore::open(&database).expect("state");
                let tx = store.write_txn().expect("expiry transaction");
                let mut attachment = tx
                    .read()
                    .attachment(&attachment_key())
                    .expect("attachment query")
                    .expect("attachment");
                let hash = attachment.blob_hash.expect("verified blob");
                attachment.facts.availability = StateAttachmentAvailability::Unavailable;
                tx.replace_message_attachments(
                    &attachment_key().message,
                    std::slice::from_ref(&attachment.facts),
                    20,
                )
                .expect("apply authoritative expiry");
                tx.commit().expect("commit expiry");
                hash
            };

            assert!(
                Path::new(&materialized.path).is_file(),
                "the durable queue is the crash boundary before physical replay"
            );
            {
                let mut store = StateStore::open(&database).expect("state");
                let read = store.read_txn().expect("read expiry state");
                assert_eq!(read.cache_entry(&item_id()).expect("cache query"), None);
                assert_eq!(read.pin(&item_id()).expect("pin query"), None);
                let attachment = read
                    .attachment(&attachment_key())
                    .expect("attachment query")
                    .expect("expired metadata remains");
                assert_eq!(
                    attachment.facts.availability,
                    StateAttachmentAvailability::Unavailable
                );
                assert_eq!(attachment.blob_hash, None);
                assert_eq!(read.blob(scope().account, &hash).expect("blob query"), None);
                assert_eq!(
                    read.retention_purge_queue(scope().account, 10)
                        .expect("purge queue")
                        .len(),
                    1
                );
            }
            drop(hydrator);

            let _relaunched =
                Hydrator::shared(root.text()).expect("production relaunch replays expiry purge");
            assert!(!Path::new(&materialized.path).exists());
            let mut store = StateStore::open(&database).expect("state");
            let read = store.read_txn().expect("read relaunched state");
            assert!(
                read.retention_purge_queue(scope().account, 10)
                    .expect("purge queue")
                    .is_empty()
            );
            let account = read
                .account(scope().account)
                .expect("account query")
                .expect("account");
            assert_eq!(account.retention_mode, mode);
            assert_eq!(account.archive_mode, archive_mode);
        }
    }

    #[tokio::test]
    async fn audit_live_deletion_retains_already_materialized_allowed_bytes_across_relaunch() {
        let root = TempRoot::new();
        let bytes = b"prospectively retained Audit deletion".to_vec();
        let database = seed(&root, &bytes);
        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("Audit transaction");
            tx.set_retention_mode(scope().account, RetentionMode::Audit, None, 10)
                .expect("enable Audit");
            tx.commit().expect("commit Audit");
        }
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        hydrator.sources.register(
            7,
            1,
            Arc::new(BytesSource {
                bytes,
                fetches: AtomicUsize::new(0),
            }),
        );
        let materialized = hydrator
            .hydrate_inner(
                7,
                item_id().text().to_owned(),
                Some("content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("materialize allowed Audit bytes");
        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("Audit deletion transaction");
            tx.apply_message_changes(
                &chat_key(),
                &[MessageChange::Deleted {
                    message_id: attachment_key().message.message_id,
                    observed_at_ms: 20,
                }],
            )
            .expect("apply Audit deletion");
            tx.commit().expect("commit Audit deletion");
            let read = store.read_txn().expect("read Audit state");
            assert!(read.cache_entry(&item_id()).expect("cache query").is_some());
            assert!(
                read.attachment(&attachment_key())
                    .expect("attachment query")
                    .expect("Audit metadata")
                    .blob_hash
                    .is_some()
            );
            assert!(
                read.retention_purge_queue(scope().account, 10)
                    .expect("purge queue")
                    .is_empty()
            );
        }
        drop(hydrator);

        let _relaunched =
            Hydrator::shared(root.text()).expect("production relaunch preserves Audit bytes");
        assert!(Path::new(&materialized.path).is_file());
        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read relaunched Audit state");
        assert!(read.cache_entry(&item_id()).expect("cache query").is_some());
        assert!(
            read.retention_purge_queue(scope().account, 10)
                .expect("purge queue")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn audit_attachment_removal_by_edit_survives_relaunch_without_refetch() {
        for archive_mode in [false, true] {
            let root = TempRoot::new();
            let bytes = format!("Audit removed-index bytes archive={archive_mode}").into_bytes();
            let expected_size = bytes.len() as u64;
            let database = seed(&root, &bytes);
            configure_archive(&database, RetentionMode::Audit, archive_mode);
            let hydrator = Hydrator::shared(root.text()).expect("hydrator");
            let source = Arc::new(BytesSource {
                bytes,
                fetches: AtomicUsize::new(0),
            });
            hydrator
                .sources
                .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);
            let materialized = hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
                .expect("materialize allowed Audit bytes");
            assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
            let shared_owner = if archive_mode {
                let other_scope = AccountScope {
                    account: AccountKey {
                        account_id: AccountId(8),
                    },
                    namespace_version: NamespaceVersion(1),
                };
                let other_root =
                    ItemKey::Canonical(CanonicalKey::Account(other_scope.account)).id();
                let other_item = ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
                    message: MessageKey {
                        chat: ChatKey {
                            scope: other_scope,
                            chat_id: ChatId(200),
                        },
                        message_id: MessageId(15),
                    },
                    index: AttachmentIndex(0),
                }))
                .id();
                let (hash, reference) = {
                    let mut store = StateStore::open(&database).expect("state");
                    let read = store.read_txn().expect("read cache owner");
                    let cache = read
                        .cache_entry(&item_id())
                        .expect("cache query")
                        .expect("cache entry");
                    (
                        cache.blob_hash.expect("verified blob hash"),
                        cache
                            .materialization_ref
                            .expect("materialization reference"),
                    )
                };
                let mut store = StateStore::open(&database).expect("state");
                let tx = store.write_txn().expect("shared owner transaction");
                tx.upsert_account(&AccountRecord {
                    account: other_scope.account,
                    source_kind: SourceKind::LocalTdlib,
                    display_name: "Shared Owner".to_owned(),
                    auth_state: "authorized".to_owned(),
                    namespace_version: other_scope.namespace_version,
                    display_timezone: "UTC".to_owned(),
                    retention_mode: RetentionMode::Mirror,
                    archive_mode: false,
                    secret_ref: None,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                })
                .expect("other account");
                tx.upsert_item(&ItemRecord {
                    aggregate_size: None,
                    id: other_root.clone(),
                    parent: None,
                    display_name: "Shared Owner".to_owned(),
                    safe_name: "Shared Owner".to_owned(),
                    metadata_version: gramdrive_model::version::MetadataVersion::new("shared-root")
                        .expect("metadata version"),
                    content: None,
                    availability: ItemAvailability::Fetchable,
                    created_at_ms: Some(1),
                    modified_at_ms: Some(1),
                    deleted_at_ms: None,
                })
                .expect("other root");
                tx.upsert_item(&ItemRecord {
                    aggregate_size: None,
                    id: other_item.clone(),
                    parent: Some(other_root),
                    display_name: "shared.bin".to_owned(),
                    safe_name: "shared.bin".to_owned(),
                    metadata_version: gramdrive_model::version::MetadataVersion::new("shared-item")
                        .expect("metadata version"),
                    content: Some(FileFacts {
                        mime_type: Some("application/octet-stream".to_owned()),
                        logical_size: Some(expected_size),
                        content_version: Some(
                            ContentVersion::new("shared-content-v1").expect("content version"),
                        ),
                    }),
                    availability: ItemAvailability::Fetchable,
                    created_at_ms: Some(1),
                    modified_at_ms: Some(1),
                    deleted_at_ms: None,
                })
                .expect("other item");
                tx.record_blob(other_scope.account, &hash, expected_size, 1)
                    .expect("other blob owner");
                tx.upsert_cache_entry(&CacheEntryRecord {
                    item: other_item.clone(),
                    account: other_scope.account,
                    content_version: ContentVersion::new("shared-content-v1")
                        .expect("content version"),
                    kind: CacheKind::Blob,
                    size: expected_size,
                    blob_hash: Some(hash),
                    verification: CacheVerification::Verified,
                    pin: Some(PinOrigin::User),
                    last_access_at_ms: 1,
                    materialized_at_ms: 1,
                    materialization_ref: Some(reference),
                })
                .expect("other cache owner");
                tx.pin_item(&other_item, PinOrigin::User, 1)
                    .expect("other pin");
                tx.commit().expect("commit shared owner");
                Some((other_scope.account, other_item))
            } else {
                None
            };

            {
                let mut store = StateStore::open(&database).expect("state");
                let tx = store.write_txn().expect("Audit edit transaction");
                tx.replace_message_attachments(&attachment_key().message, &[], 20)
                    .expect("remove attachment index");
                tx.tombstone_item(
                    &item_id(),
                    20,
                    &gramdrive_model::version::MetadataVersion::new(
                        "removed-attachment-projection",
                    )
                    .expect("metadata version"),
                )
                .expect("tombstone removed attachment item");
                tx.commit().expect("commit Audit edit");
                let read = store.read_txn().expect("read retained edit");
                assert!(
                    read.attachment(&attachment_key())
                        .expect("attachment query")
                        .is_none()
                );
                let retained = read
                    .retained_attachment_versions(&attachment_key())
                    .expect("retained versions");
                assert_eq!(retained.len(), 1);
                assert_eq!(
                    retained[0].materialization_ref.as_deref(),
                    Some(materialized.path.as_str())
                );
                assert_eq!(
                    read.cache_totals().expect("cache totals").pinned_bytes,
                    expected_size * if shared_owner.is_some() { 2 } else { 1 }
                );
                assert!(
                    read.archive_backfill_candidates(scope().account, 10)
                        .expect("Archive worklist")
                        .is_empty()
                );
                assert!(
                    read.retention_purge_queue(scope().account, 10)
                        .expect("purge queue")
                        .is_empty()
                );
            }
            assert!(
                !hydrator
                    .schedule_archive_backfill(
                        BackfillScheduler::with_defaults(),
                        scope(),
                        HostConditions::UNCONSTRAINED,
                    )
                    .expect("removed-index scheduler")
            );
            assert_eq!(
                source.fetches.load(Ordering::SeqCst),
                1,
                "Audit retention must never refetch a removed attachment"
            );
            drop(hydrator);

            let relaunched =
                Hydrator::shared(root.text()).expect("production relaunch preserves Audit bytes");
            assert!(Path::new(&materialized.path).is_file());
            let mut store = StateStore::open(&database).expect("state");
            let read = store.read_txn().expect("read relaunched Audit state");
            assert_eq!(
                read.retained_attachment_versions(&attachment_key())
                    .expect("retained versions")
                    .len(),
                1
            );
            assert!(
                read.eviction_candidates_after(None, 10)
                    .expect("eviction candidates")
                    .is_empty()
            );
            drop(read);

            let tx = store.write_txn().expect("chat protection transaction");
            let mut chat = tx
                .read()
                .chat(&chat_key())
                .expect("chat query")
                .expect("chat");
            chat.is_protected = true;
            tx.upsert_chat(&chat).expect("protect chat");
            tx.commit().expect("commit chat protection");
            drop(store);
            drop(relaunched);

            let purging_relaunch =
                Hydrator::shared(root.text()).expect("production relaunch applies protection");
            assert_eq!(
                Path::new(&materialized.path).exists(),
                shared_owner.is_some(),
                "physical bytes survive only while another account owns the shared object"
            );
            let mut store = StateStore::open(&database).expect("state");
            let read = store.read_txn().expect("read converged purge");
            assert!(
                read.retained_attachment_versions(&attachment_key())
                    .expect("retained versions")
                    .is_empty()
            );
            assert!(
                read.retention_purge_queue(scope().account, 10)
                    .expect("purge queue")
                    .is_empty()
            );
            if let Some((other_account, ref other_item)) = shared_owner {
                assert!(
                    read.cache_entry(other_item)
                        .expect("shared cache query")
                        .is_some()
                );
                assert!(read.pin(other_item).expect("shared pin query").is_some());
                assert!(
                    read.retention_purge_queue(other_account, 10)
                        .expect("other purge queue")
                        .is_empty()
                );
            }
            drop(read);
            drop(store);
            drop(purging_relaunch);

            let _idempotent_relaunch =
                Hydrator::shared(root.text()).expect("idempotent protection replay");
            assert_eq!(
                Path::new(&materialized.path).exists(),
                shared_owner.is_some()
            );
        }
    }

    #[tokio::test]
    async fn mirror_live_deletion_replays_physical_purge_and_preserves_shared_account_owner() {
        for shared_owner in [false, true] {
            let root = TempRoot::new();
            let bytes = format!("mirror deletion shared={shared_owner}").into_bytes();
            let database = seed(&root, &bytes);
            let hydrator = Hydrator::shared(root.text()).expect("hydrator");
            hydrator.sources.register(
                7,
                1,
                Arc::new(BytesSource {
                    bytes,
                    fetches: AtomicUsize::new(0),
                }),
            );
            let materialized = hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
                .expect("materialize Mirror bytes");
            let (hash, materialization_ref, size) = {
                let mut store = StateStore::open(&database).expect("state");
                let read = store.read_txn().expect("read materialization");
                let cache = read
                    .cache_entry(&item_id())
                    .expect("cache query")
                    .expect("cache entry");
                (
                    cache.blob_hash.expect("cache blob"),
                    cache
                        .materialization_ref
                        .expect("materialization reference"),
                    cache.size,
                )
            };

            let shared_item = if shared_owner {
                let other_scope = AccountScope {
                    account: AccountKey {
                        account_id: AccountId(8),
                    },
                    namespace_version: NamespaceVersion(1),
                };
                let other_root =
                    ItemKey::Canonical(CanonicalKey::Account(other_scope.account)).id();
                let other_item = ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
                    message: MessageKey {
                        chat: ChatKey {
                            scope: other_scope,
                            chat_id: ChatId(200),
                        },
                        message_id: MessageId(15),
                    },
                    index: AttachmentIndex(0),
                }))
                .id();
                let mut store = StateStore::open(&database).expect("state");
                let tx = store.write_txn().expect("shared owner transaction");
                tx.upsert_account(&AccountRecord {
                    account: other_scope.account,
                    source_kind: SourceKind::LocalTdlib,
                    display_name: "Shared Owner".to_owned(),
                    auth_state: "authorized".to_owned(),
                    namespace_version: other_scope.namespace_version,
                    display_timezone: "UTC".to_owned(),
                    retention_mode: RetentionMode::Audit,
                    archive_mode: false,
                    secret_ref: None,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                })
                .expect("other account");
                tx.upsert_item(&ItemRecord {
                    aggregate_size: None,
                    id: other_root.clone(),
                    parent: None,
                    display_name: "Shared Owner".to_owned(),
                    safe_name: "Shared Owner".to_owned(),
                    metadata_version: gramdrive_model::version::MetadataVersion::new("shared-root")
                        .expect("metadata version"),
                    content: None,
                    availability: ItemAvailability::Fetchable,
                    created_at_ms: Some(1),
                    modified_at_ms: Some(1),
                    deleted_at_ms: None,
                })
                .expect("other root");
                tx.upsert_item(&ItemRecord {
                    aggregate_size: None,
                    id: other_item.clone(),
                    parent: Some(other_root),
                    display_name: "shared.bin".to_owned(),
                    safe_name: "shared.bin".to_owned(),
                    metadata_version: gramdrive_model::version::MetadataVersion::new("shared-item")
                        .expect("metadata version"),
                    content: Some(FileFacts {
                        mime_type: Some("application/octet-stream".to_owned()),
                        logical_size: Some(size),
                        content_version: Some(
                            ContentVersion::new("shared-content-v1").expect("content version"),
                        ),
                    }),
                    availability: ItemAvailability::Fetchable,
                    created_at_ms: Some(1),
                    modified_at_ms: Some(1),
                    deleted_at_ms: None,
                })
                .expect("other item");
                tx.record_blob(other_scope.account, &hash, size, 1)
                    .expect("other blob owner");
                tx.upsert_cache_entry(&CacheEntryRecord {
                    item: other_item.clone(),
                    account: other_scope.account,
                    content_version: ContentVersion::new("shared-content-v1")
                        .expect("content version"),
                    kind: CacheKind::Blob,
                    size,
                    blob_hash: Some(hash),
                    verification: CacheVerification::Verified,
                    pin: Some(PinOrigin::User),
                    last_access_at_ms: 1,
                    materialized_at_ms: 1,
                    materialization_ref: Some(materialization_ref.clone()),
                })
                .expect("other cache owner");
                tx.pin_item(&other_item, PinOrigin::User, 1)
                    .expect("other pin");
                tx.commit().expect("commit shared owner");
                Some((other_scope.account, other_item))
            } else {
                None
            };

            {
                let mut store = StateStore::open(&database).expect("state");
                let tx = store.write_txn().expect("Mirror deletion transaction");
                tx.apply_message_changes(
                    &chat_key(),
                    &[MessageChange::Deleted {
                        message_id: attachment_key().message.message_id,
                        observed_at_ms: 20,
                    }],
                )
                .expect("apply live deletion");
                tx.commit().expect("commit live deletion");
                let read = store.read_txn().expect("read deletion state");
                assert_eq!(read.cache_entry(&item_id()).expect("cache query"), None);
                assert_eq!(read.pin(&item_id()).expect("pin query"), None);
                assert!(
                    read.attachment(&attachment_key())
                        .expect("attachment query")
                        .is_none()
                );
                assert_eq!(read.blob(scope().account, &hash).expect("blob query"), None);
                assert_eq!(
                    read.retention_purge_queue(scope().account, 10)
                        .expect("purge queue")
                        .len(),
                    1
                );
            }
            drop(hydrator);

            let _relaunched =
                Hydrator::shared(root.text()).expect("production relaunch replays deletion purge");
            assert_eq!(
                Path::new(&materialized.path).exists(),
                shared_owner,
                "physical bytes survive only while another account still owns the shared object"
            );
            let mut store = StateStore::open(&database).expect("state");
            let read = store.read_txn().expect("read relaunched state");
            assert!(
                read.retention_purge_queue(scope().account, 10)
                    .expect("purge queue")
                    .is_empty()
            );
            if let Some((other_account, other_item)) = shared_item {
                assert!(
                    read.cache_entry(&other_item)
                        .expect("shared cache query")
                        .is_some()
                );
                assert!(read.pin(&other_item).expect("shared pin query").is_some());
                assert!(
                    read.retention_purge_queue(other_account, 10)
                        .expect("other purge queue")
                        .is_empty()
                );
            }
        }
    }

    #[tokio::test]
    async fn restricted_attachment_cleanup_is_account_scoped() {
        let root = TempRoot::new();
        let database = seed(&root, b"account seven bytes");
        let other_scope = AccountScope {
            account: AccountKey {
                account_id: AccountId(8),
            },
            namespace_version: NamespaceVersion(1),
        };
        let other_root = ItemKey::Canonical(CanonicalKey::Account(other_scope.account)).id();
        let other_chat = ChatKey {
            scope: other_scope,
            chat_id: ChatId(200),
        };
        let other_attachment = AttachmentKey {
            message: MessageKey {
                chat: other_chat,
                message_id: MessageId(15),
            },
            index: AttachmentIndex(0),
        };
        let other_item = ItemKey::Canonical(CanonicalKey::Attachment(other_attachment)).id();
        let other_bytes = b"account eight bytes".to_vec();
        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("other account transaction");
            tx.upsert_account(&AccountRecord {
                account: other_scope.account,
                source_kind: SourceKind::LocalTdlib,
                display_name: "Other".to_owned(),
                auth_state: "authorized".to_owned(),
                namespace_version: other_scope.namespace_version,
                display_timezone: "UTC".to_owned(),
                retention_mode: RetentionMode::Audit,
                archive_mode: false,
                secret_ref: None,
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .expect("other account");
            tx.upsert_item(&ItemRecord {
                aggregate_size: None,
                id: other_root.clone(),
                parent: None,
                display_name: "Other".to_owned(),
                safe_name: "Other".to_owned(),
                metadata_version: gramdrive_model::version::MetadataVersion::new("other-root")
                    .expect("version"),
                content: None,
                availability: ItemAvailability::Fetchable,
                created_at_ms: Some(1),
                modified_at_ms: Some(1),
                deleted_at_ms: None,
            })
            .expect("other root");
            tx.upsert_chat(&ChatRecord {
                key: other_chat,
                chat_type: ChatType::Private,
                title: "Other Chat".to_owned(),
                username: None,
                is_protected: false,
                archive_mode: true,
                metadata_version: gramdrive_model::version::MetadataVersion::new("other-chat")
                    .expect("version"),
                left_at_ms: None,
                deleted_at_ms: None,
                last_update_at_ms: Some(1),
            })
            .expect("other chat");
            tx.apply_message_changes(
                &other_chat,
                &[MessageChange::Observed(MessageRevision {
                    message_id: MessageId(15),
                    sender_id: Some(84),
                    sent_at_ms: 1,
                    edited_at_ms: None,
                    observed_at_ms: 1,
                    payload_schema: SchemaFamily(1),
                    payload: vec![8],
                })],
            )
            .expect("other message");
            tx.upsert_item(&ItemRecord {
                aggregate_size: None,
                id: other_item.clone(),
                parent: Some(other_root),
                display_name: "other.bin".to_owned(),
                safe_name: "other.bin".to_owned(),
                metadata_version: gramdrive_model::version::MetadataVersion::new("other-item")
                    .expect("version"),
                content: Some(FileFacts {
                    mime_type: Some("application/octet-stream".to_owned()),
                    logical_size: Some(other_bytes.len() as u64),
                    content_version: Some(
                        ContentVersion::new("other-content-v1").expect("content version"),
                    ),
                }),
                availability: ItemAvailability::Fetchable,
                created_at_ms: Some(1),
                modified_at_ms: Some(1),
                deleted_at_ms: None,
            })
            .expect("other item");
            tx.upsert_attachment(&AttachmentFacts {
                key: other_attachment,
                logical_kind: AttachmentLogicalKind::Document,
                telegram_representation: TelegramRepresentation::OriginalDocument,
                fidelity: AttachmentFidelity::Original,
                source_name: Some("other.bin".to_owned()),
                mime_type: Some("application/octet-stream".to_owned()),
                exact_size: Some(other_bytes.len() as u64),
                content_version: ContentVersion::new("other-content-v1").expect("version"),
                telegram_unique_id: Some("other-stable-id".to_owned()),
                telegram_local_file_id: Some(800),
                telegram_file_id: Some("other-remote".to_owned()),
                file_reference: None,
                availability: StateAttachmentAvailability::Fetchable,
                can_be_saved: true,
            })
            .expect("other attachment");
            tx.set_archive_mode(other_scope.account, true, 2)
                .expect("other Archive Mode");
            tx.commit().expect("commit other account");
        }

        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        hydrator.sources.register(
            7,
            1,
            Arc::new(BytesSource {
                bytes: b"account seven bytes".to_vec(),
                fetches: AtomicUsize::new(0),
            }),
        );
        hydrator.sources.register(
            8,
            2,
            Arc::new(BytesSource {
                bytes: other_bytes,
                fetches: AtomicUsize::new(0),
            }),
        );
        let restricted = hydrator
            .hydrate_inner(
                7,
                item_id().text().to_owned(),
                Some("content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("materialize restricted account before policy");
        let retained = hydrator
            .hydrate_inner(
                8,
                other_item.text().to_owned(),
                Some("other-content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("materialize other account");

        let mut store = StateStore::open(&database).expect("state");
        let tx = store.write_txn().expect("protect account seven");
        let mut chat = tx
            .read()
            .chat(&chat_key())
            .expect("chat query")
            .expect("chat");
        chat.is_protected = true;
        tx.upsert_chat(&chat).expect("protect chat");
        tx.commit().expect("commit protection");
        drop(store);

        hydrator
            .purge_disallowed_attachment_materializations(scope().account)
            .expect("account-scoped cleanup");
        assert!(!Path::new(&restricted.path).exists());
        assert!(Path::new(&retained.path).is_file());
        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read isolation state");
        assert_eq!(
            read.cache_entry(&item_id()).expect("restricted cache"),
            None
        );
        assert!(
            read.cache_entry(&other_item)
                .expect("other cache")
                .is_some()
        );
        assert!(
            read.attachment(&other_attachment)
                .expect("other attachment query")
                .expect("other attachment")
                .blob_hash
                .is_some()
        );
        assert!(
            read.retention_purge_queue(other_scope.account, 10)
                .expect("other queue")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn production_hydrator_relaunch_automatically_drains_audit_to_mirror_purge() {
        let root = TempRoot::new();
        let bytes = b"crash retained audit bytes".to_vec();
        let database = seed(&root, &bytes);
        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("Audit transaction");
            tx.set_retention_mode(scope().account, RetentionMode::Audit, None, 10)
                .expect("enable Audit");
            tx.commit().expect("commit Audit");
        }
        let hydrator = Hydrator::shared(root.text()).expect("first production hydrator");
        hydrator.sources.register(
            7,
            1,
            Arc::new(BytesSource {
                bytes,
                fetches: AtomicUsize::new(0),
            }),
        );
        let materialized = hydrator
            .hydrate_inner(
                7,
                item_id().text().to_owned(),
                Some("content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("materialize Audit bytes");

        let mut store = StateStore::open(&database).expect("state");
        let tx = store.write_txn().expect("Audit deletion");
        tx.apply_message_changes(
            &chat_key(),
            &[MessageChange::Deleted {
                message_id: MessageId(5),
                observed_at_ms: 20,
            }],
        )
        .expect("retain Audit deletion");
        tx.commit().expect("commit Audit deletion");
        let confirmation = AuditToMirrorConfirmation::parse(
            scope().account,
            &AuditToMirrorConfirmation::expected_phrase(scope().account),
        )
        .expect("typed confirmation");
        let tx = store.write_txn().expect("destructive transition");
        let change = tx
            .set_retention_mode(
                scope().account,
                RetentionMode::Mirror,
                Some(confirmation),
                30,
            )
            .expect("commit Audit-to-Mirror effects");
        tx.commit().expect("commit destructive transition");
        assert_eq!(change.queued_file_purges, 1);
        assert!(Path::new(&materialized.path).is_file());
        assert_eq!(
            store
                .read_txn()
                .expect("read crash boundary")
                .retention_purge_queue(scope().account, 10)
                .expect("pending queue")
                .len(),
            1
        );
        drop(store);
        drop(hydrator);

        let _relaunched =
            Hydrator::shared(root.text()).expect("production relaunch converges purge");
        assert!(!Path::new(&materialized.path).exists());
        let mut store = StateStore::open(&database).expect("reopen state");
        assert!(
            store
                .read_txn()
                .expect("read relaunched state")
                .retention_purge_queue(scope().account, 10)
                .expect("relaunch queue")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn concurrent_same_item_requests_coalesce_and_both_complete() {
        let root = TempRoot::new();
        let bytes = b"one Telegram object for two readers".to_vec();
        seed(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let started = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(AsyncGate::default());
        let source = Arc::new(GatedSource {
            bytes: bytes.clone(),
            fetches: AtomicUsize::new(0),
            started: Arc::clone(&started),
            gate: Arc::clone(&gate),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);

        let first_hydrator = Arc::clone(&hydrator);
        let first = tokio::spawn(async move {
            first_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_test(|| started.load(Ordering::SeqCst) == 1).await;

        let second_hydrator = Arc::clone(&hydrator);
        let second = tokio::spawn(async move {
            second_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_test(|| hydrator.coordinator.reader_count(TransferId(1)) == 2).await;
        gate.release();

        let first = first.await.expect("first task").expect("first hydration");
        let second = second
            .await
            .expect("second task")
            .expect("second hydration");
        assert_eq!(first.path, second.path);
        assert_eq!(fs::read(&first.path).expect("cache bytes"), bytes);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_one_coalesced_zero_byte_request_keeps_transfer_alive() {
        let root = TempRoot::new();
        let database = seed(&root, &[]);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let source = Arc::new(BytesSource {
            bytes: Vec::new(),
            fetches: AtomicUsize::new(0),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);
        let (checkpoint_send, checkpoint_receive) = std::sync::mpsc::channel();
        let (release_send, release_receive) = std::sync::mpsc::sync_channel(0);
        *hydrator
            .driver_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(BlockingDriverProbe {
            remaining: AtomicUsize::new(2),
            checkpoints: checkpoint_send,
            releases: Mutex::new(release_receive),
        }));

        let first_token = CancellationToken::new();
        let first_cancel = Arc::clone(&first_token);
        let first_hydrator = Arc::clone(&hydrator);
        let first = tokio::spawn(async move {
            first_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    first_token,
                )
                .await
        });
        checkpoint_receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("first driver pauses before claim");
        wait_for_test(|| hydrator.coordinator.reader_count(TransferId(1)) == 1).await;

        let second_hydrator = Arc::clone(&hydrator);
        let second = tokio::spawn(async move {
            second_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        checkpoint_receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("second driver pauses before claim");
        wait_for_test(|| hydrator.coordinator.reader_count(TransferId(1)) == 2).await;

        first_cancel.cancel();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), first)
            .await
            .expect("first cancellation timeout")
            .expect("first hydration task");
        assert!(matches!(first, Err(DriveError::Cancelled { .. })));
        assert_eq!(hydrator.coordinator.reader_count(TransferId(1)), 1);
        assert_eq!(
            StateStore::open(&database)
                .expect("state")
                .read_txn()
                .expect("read")
                .transfer(TransferId(1))
                .expect("transfer query")
                .expect("transfer")
                .state,
            TransferState::Queued,
            "one detached subscriber cannot cancel shared queued work"
        );

        release_send.send(()).expect("release first driver");
        release_send.send(()).expect("release second driver");
        let materialized = tokio::time::timeout(std::time::Duration::from_secs(2), second)
            .await
            .expect("remaining hydration timeout")
            .expect("remaining hydration task")
            .expect("remaining hydration succeeds");
        assert_eq!(materialized.byte_count, 0);
        assert_eq!(
            fs::metadata(&materialized.path)
                .expect("empty cache file")
                .len(),
            0
        );
        assert_eq!(source.fetches.load(Ordering::SeqCst), 0);

        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read");
        assert_eq!(
            read.transfer(TransferId(1))
                .expect("transfer query")
                .expect("transfer")
                .state,
            TransferState::Done
        );
        assert!(read.cache_entry(&item_id()).expect("cache query").is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_final_zero_byte_request_publishes_nothing() {
        let root = TempRoot::new();
        let database = seed(&root, &[]);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let source = Arc::new(BytesSource {
            bytes: Vec::new(),
            fetches: AtomicUsize::new(0),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);
        let (checkpoint_send, checkpoint_receive) = std::sync::mpsc::channel();
        let (release_send, release_receive) = std::sync::mpsc::sync_channel(0);
        *hydrator
            .driver_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(BlockingDriverProbe {
            remaining: AtomicUsize::new(1),
            checkpoints: checkpoint_send,
            releases: Mutex::new(release_receive),
        }));

        let token = CancellationToken::new();
        let cancellation = Arc::clone(&token);
        let task_hydrator = Arc::clone(&hydrator);
        let hydration = tokio::spawn(async move {
            task_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    token,
                )
                .await
        });
        checkpoint_receive
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("driver pauses before claim");
        wait_for_test(|| hydrator.coordinator.reader_count(TransferId(1)) == 1).await;

        cancellation.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), hydration)
            .await
            .expect("cancellation timeout")
            .expect("hydration task");
        assert!(matches!(result, Err(DriveError::Cancelled { .. })));
        assert_eq!(hydrator.coordinator.reader_count(TransferId(1)), 0);

        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read");
        let transfer = read
            .transfer(TransferId(1))
            .expect("transfer query")
            .expect("transfer");
        assert_eq!(transfer.state, TransferState::Cancelled);
        assert_eq!(transfer.temp_ref, None);
        assert!(transfer.completed_ranges.is_empty());
        assert_eq!(read.cache_entry(&item_id()).expect("cache query"), None);
        drop(read);

        let mut driver_done = hydrator.changed.subscribe();
        release_send.send(()).expect("release cancelled driver");
        tokio::time::timeout(std::time::Duration::from_secs(2), driver_done.changed())
            .await
            .expect("cancelled driver completion timeout")
            .expect("driver completion signal");
        assert_eq!(source.fetches.load(Ordering::SeqCst), 0);
        assert_eq!(
            StateStore::open(&database)
                .expect("state")
                .read_txn()
                .expect("read")
                .cache_entry(&item_id())
                .expect("cache query"),
            None
        );
        let layout = shared_state_layout(root.text().to_owned()).expect("layout");
        assert_eq!(
            fs::read_dir(Path::new(&layout.cache_dir).join("transfers"))
                .expect("staging directory")
                .count(),
            0,
            "final zero-byte cancellation leaves no unpublished staging"
        );
    }

    #[tokio::test]
    async fn cancelling_one_coalesced_reader_keeps_the_shared_transfer_alive() {
        let root = TempRoot::new();
        let bytes = b"shared transfer survives one close".to_vec();
        seed(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let started = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(AsyncGate::default());
        let source = Arc::new(GatedSource {
            bytes: bytes.clone(),
            fetches: AtomicUsize::new(0),
            started: Arc::clone(&started),
            gate: Arc::clone(&gate),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);
        let first_token = CancellationToken::new();
        let first_cancel = Arc::clone(&first_token);

        let first_hydrator = Arc::clone(&hydrator);
        let first = tokio::spawn(async move {
            first_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    first_token,
                )
                .await
        });
        wait_for_test(|| started.load(Ordering::SeqCst) == 1).await;
        let second_hydrator = Arc::clone(&hydrator);
        let second = tokio::spawn(async move {
            second_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_test(|| hydrator.coordinator.reader_count(TransferId(1)) == 2).await;

        first_cancel.cancel();
        wait_for_test(|| hydrator.coordinator.reader_count(TransferId(1)) == 1).await;
        gate.release();

        assert!(matches!(
            first.await.expect("first task"),
            Err(DriveError::Cancelled { .. })
        ));
        let materialized = second
            .await
            .expect("second task")
            .expect("remaining reader hydrates");
        assert_eq!(fs::read(materialized.path).expect("cache bytes"), bytes);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_delay_for_one_item_does_not_block_an_unrelated_hydration() {
        let root = TempRoot::new();
        let first_bytes = b"retry later".to_vec();
        let second_bytes = b"unrelated bytes now".to_vec();
        let database = seed(&root, &first_bytes);
        seed_second(&database, &second_bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let source = Arc::new(RetryOneSource {
            first_item: item_id().text().to_owned(),
            first_bytes,
            second_bytes: second_bytes.clone(),
            first_attempts: AtomicUsize::new(0),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);
        let first_token = CancellationToken::new();
        let first_cancel = Arc::clone(&first_token);
        let first_hydrator = Arc::clone(&hydrator);
        let first = tokio::spawn(async move {
            first_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    first_token,
                )
                .await
        });
        wait_for_test(|| source.first_attempts.load(Ordering::SeqCst) == 1).await;
        wait_for_test(|| {
            let mut store = StateStore::open(&database).expect("state");
            store
                .read_txn()
                .expect("read")
                .transfer(TransferId(1))
                .expect("transfer query")
                .is_some_and(|row| row.next_retry_at_ms.is_some())
        })
        .await;

        let second = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            hydrator.hydrate_inner(
                7,
                second_item_id().text().to_owned(),
                Some("content-v2".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("unrelated hydration must beat the first retry delay")
        .expect("unrelated hydration");
        assert_eq!(
            fs::read(second.path).expect("second cache bytes"),
            second_bytes
        );

        first_cancel.cancel();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(2), first)
                .await
                .expect("first cancellation timeout")
                .expect("first task"),
            Err(DriveError::Cancelled { .. })
        ));
    }

    #[tokio::test]
    async fn cancellation_drops_source_marks_journal_and_publishes_no_partial() {
        let root = TempRoot::new();
        let bytes = b"never published";
        let database = seed(&root, bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        hydrator.sources.register(
            7,
            1,
            Arc::new(HangingSource {
                started: Arc::clone(&started),
                dropped: Arc::clone(&dropped),
            }),
        );
        let token = CancellationToken::new();
        let cancellation = Arc::clone(&token);

        let operation = hydrator.hydrate_inner(
            7,
            item_id().text().to_owned(),
            Some("content-v1".to_owned()),
            Arc::new(NoopProgress),
            token,
        );
        let cancel = async {
            while started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            cancellation.cancel();
        };
        let (result, ()) = tokio::join!(operation, cancel);
        assert!(matches!(result, Err(DriveError::Cancelled { .. })));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);

        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read");
        let row = read
            .transfer(TransferId(1))
            .expect("query")
            .expect("transfer");
        assert_eq!(row.state, TransferState::Cancelled);
        assert!(row.completed_ranges.is_empty());
        assert_eq!(row.temp_ref, None);
        assert_eq!(read.cache_entry(&item_id()).expect("cache query"), None);
        drop(read);
        let layout = shared_state_layout(root.text().to_owned()).expect("layout");
        assert_eq!(
            fs::read_dir(Path::new(&layout.cache_dir).join("transfers"))
                .expect("staging directory")
                .count(),
            0,
            "cancellation disposes unpublished staging"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn simultaneous_first_opens_keep_cancellation_bound_across_generations() {
        let root = TempRoot::new();
        let bytes = b"never published";
        let database = seed(&root, bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let started = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        hydrator.sources.register(
            7,
            1,
            Arc::new(HangingSource {
                started: Arc::clone(&started),
                dropped: Arc::clone(&dropped),
            }),
        );

        race_first_opens_then_cancel(&hydrator, &database, &started, &dropped, TransferId(1), 1)
            .await;
        race_first_opens_then_cancel(&hydrator, &database, &started, &dropped, TransferId(2), 2)
            .await;
    }

    #[tokio::test]
    async fn story_transition_reuses_one_canonical_blob_and_mirror_removal_purges_it() {
        let root = TempRoot::new();
        let bytes = b"one allowed story object".to_vec();
        let database = seed_story(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let source = Arc::new(BytesSource {
            bytes: bytes.clone(),
            fetches: AtomicUsize::new(0),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);

        let active = hydrator
            .hydrate_inner(
                7,
                story_item(StoryAppearanceLocation::Active)
                    .text()
                    .to_owned(),
                Some("story-content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("hydrate active story");
        assert_eq!(fs::read(&active.path).expect("story bytes"), bytes);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);

        let month_item = transition_story_to_month(&database);
        let month = hydrator
            .hydrate_inner(
                7,
                month_item.text().to_owned(),
                Some("story-content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("reuse story in month");
        assert_eq!(month.path, active.path);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
        {
            let mut store = StateStore::open(&database).expect("state");
            let read = store.read_txn().expect("read");
            assert!(
                read.story(&story_key())
                    .expect("story query")
                    .expect("canonical story")
                    .blob_hash
                    .is_some()
            );
            drop(read);
            assert_eq!(
                store
                    .connection()
                    .query_row("SELECT count(*) FROM cache_entries", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .expect("cache row count"),
                1,
                "the month appearance reuses the active materialization row"
            );
        }

        let mut store = StateStore::open(&database).expect("state");
        let tx = store.write_txn().expect("mirror removal transaction");
        tx.remove_profile_story(&story_key(), RetentionMode::Mirror, 3_000)
            .expect("mirror story removal");
        tx.tombstone_item(
            &month_item,
            3_000,
            &gramdrive_model::version::MetadataVersion::new("story-month-removed")
                .expect("metadata version"),
        )
        .expect("remove month item");
        tx.commit().expect("mirror removal commit");
        drop(store);
        hydrator
            .purge_disallowed_story_materializations(scope().account)
            .expect("purge Mirror story bytes");
        assert!(!Path::new(&active.path).exists());
        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read purged state");
        assert!(read.story(&story_key()).expect("story query").is_none());
        assert_eq!(
            read.cache_entry(&story_item(StoryAppearanceLocation::Active))
                .expect("cache query"),
            None
        );
    }

    #[tokio::test]
    async fn audit_removed_story_serves_only_observed_bytes_without_another_fetch() {
        let root = TempRoot::new();
        let bytes = b"audit-retained story object".to_vec();
        let database = seed_story(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let source = Arc::new(BytesSource {
            bytes,
            fetches: AtomicUsize::new(0),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);
        let active = hydrator
            .hydrate_inner(
                7,
                story_item(StoryAppearanceLocation::Active)
                    .text()
                    .to_owned(),
                Some("story-content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("hydrate before removal");
        let month_item = transition_story_to_month(&database);
        let mut store = StateStore::open(&database).expect("state");
        let tx = store.write_txn().expect("audit removal transaction");
        tx.remove_profile_story(&story_key(), RetentionMode::Audit, 4_000)
            .expect("audit removal");
        tx.commit().expect("audit removal commit");
        drop(store);

        hydrator
            .purge_disallowed_story_materializations(scope().account)
            .expect("audit cleanup pass");
        let retained = hydrator
            .hydrate_inner(
                7,
                month_item.text().to_owned(),
                Some("story-content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("serve observed Audit bytes");
        assert_eq!(retained.path, active.path);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
        assert!(Path::new(&retained.path).is_file());
        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read retained state");
        assert!(read.story(&story_key()).expect("story query").is_some());
        assert!(
            read.story_appearances(&story_key()).expect("appearances")[0]
                .removed_at_ms
                .is_some()
        );
    }

    #[tokio::test]
    async fn audit_inaccessible_profile_story_keeps_materialized_bytes_without_refetch() {
        let root = TempRoot::new();
        let bytes = b"audit inaccessible story object".to_vec();
        let database = seed_story(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let source = Arc::new(BytesSource {
            bytes,
            fetches: AtomicUsize::new(0),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);

        let active = hydrator
            .hydrate_inner(
                7,
                story_item(StoryAppearanceLocation::Active)
                    .text()
                    .to_owned(),
                Some("story-content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("materialize active story");
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);

        let month_item = transition_story_to_month(&database);
        let mut store = StateStore::open(&database).expect("state");
        let tx = store.write_txn().expect("Audit policy and membership");
        tx.set_retention_mode(scope().account, RetentionMode::Audit, None, 3_000)
            .expect("set Audit");
        tx.upsert_chat_list_entry(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &ChatListEntry {
                chat_id: chat_key().chat_id,
                sort_order: 1,
                pinned: false,
            },
        )
        .expect("main membership");
        tx.commit().expect("commit Audit policy and membership");
        crate::namespace::apply_story_commit_and_rebuild_for_test(
            &mut store,
            scope(),
            &StoryCommit::Inaccessible {
                poster_chat_id: chat_key().chat_id.0,
                story_id: story_key().story_id.0,
            },
            4_000,
        )
        .expect("apply inaccessible story and rebuild projection");
        drop(store);

        hydrator
            .purge_disallowed_story_materializations(scope().account)
            .expect("clean retained Audit materializations");
        assert!(Path::new(&active.path).is_file());

        let retained = hydrator
            .hydrate_inner(
                7,
                month_item.text().to_owned(),
                Some("story-content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("serve retained inaccessible bytes");
        assert_eq!(retained.path, active.path);
        assert_eq!(
            fs::read(&retained.path).expect("retained bytes"),
            source.bytes
        );
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);

        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read retained state");
        let story = read
            .story(&story_key())
            .expect("story query")
            .expect("Audit story");
        assert_eq!(story.facts.content_state, StoryContentState::Inaccessible);
        assert!(story.blob_hash.is_some());
        assert!(
            read.story_appearances(&story_key()).expect("appearances")[0]
                .removed_at_ms
                .is_some()
        );
        assert_eq!(
            read.cache_entries_for_account(scope().account)
                .expect("cache entries")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn protection_becoming_authoritative_purges_previously_materialized_story_bytes() {
        let root = TempRoot::new();
        let bytes = b"must not survive protection".to_vec();
        let database = seed_story(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        hydrator.sources.register(
            7,
            1,
            Arc::new(BytesSource {
                bytes,
                fetches: AtomicUsize::new(0),
            }),
        );
        let active_item = story_item(StoryAppearanceLocation::Active);
        let materialized = hydrator
            .hydrate_inner(
                7,
                active_item.text().to_owned(),
                Some("story-content-v1".to_owned()),
                Arc::new(NoopProgress),
                CancellationToken::new(),
            )
            .await
            .expect("materialize allowed story");
        assert!(Path::new(&materialized.path).is_file());

        let mut store = StateStore::open(&database).expect("state");
        let tx = store.write_txn().expect("protect story transaction");
        tx.upsert_story(&StoryFacts {
            key: story_key(),
            source_timestamp_ms: 1_721_555_200_000,
            mime_type: None,
            exact_size: None,
            content_version: ContentVersion::new("story-protected/100/91")
                .expect("protected version"),
            availability: StateAttachmentAvailability::Restricted,
            can_be_forwarded: false,
            content_state: StoryContentState::Protected,
        })
        .expect("protect canonical story");
        tx.commit().expect("commit protection");
        drop(store);

        hydrator
            .purge_disallowed_story_materializations(scope().account)
            .expect("purge protected bytes");
        assert!(!Path::new(&materialized.path).exists());
        let mut store = StateStore::open(&database).expect("state");
        assert_eq!(
            store
                .read_txn()
                .expect("read")
                .cache_entry(&active_item)
                .expect("cache query"),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn open_during_last_close_cancels_only_the_abandoned_generation() {
        let root = TempRoot::new();
        let bytes = b"replacement generation materializes".to_vec();
        let database = seed(&root, &bytes);
        let hydrator = Hydrator::shared(root.text()).expect("hydrator");
        let first_started = Arc::new(AtomicBool::new(false));
        let first_dropped = Arc::new(AtomicUsize::new(0));
        let replacement_gate = Arc::new(AsyncGate::default());
        let source = Arc::new(ReplacingSource {
            bytes: bytes.clone(),
            fetches: AtomicUsize::new(0),
            first_started: Arc::clone(&first_started),
            first_dropped: Arc::clone(&first_dropped),
            replacement_gate: Arc::clone(&replacement_gate),
        });
        hydrator
            .sources
            .register(7, 1, Arc::clone(&source) as Arc<dyn ContentSource>);

        let (checkpoint_send, checkpoint_receive) = std::sync::mpsc::channel();
        let (last_reader_release, last_reader_wait) = std::sync::mpsc::sync_channel(0);
        let (source_cancel_release, source_cancel_wait) = std::sync::mpsc::sync_channel(0);
        *hydrator
            .cancel_probe
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::new(BlockingCancelProbe {
            checkpoints: checkpoint_send,
            last_reader_release: Mutex::new(last_reader_wait),
            source_cancel_release: Mutex::new(source_cancel_wait),
        }));

        let first_token = CancellationToken::new();
        let first_cancel = Arc::clone(&first_token);
        let first_hydrator = Arc::clone(&hydrator);
        let first = tokio::spawn(async move {
            first_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    first_token,
                )
                .await
        });
        wait_for_test(|| first_started.load(Ordering::Acquire)).await;

        first_cancel.cancel();
        assert_eq!(
            checkpoint_receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("last-reader checkpoint"),
            CancelCheckpoint::LastReaderClosed
        );
        assert_eq!(hydrator.coordinator.reader_count(TransferId(1)), 0);

        let item = item_id();
        let version = ContentVersion::new("content-v1").expect("version");
        let admission = hydrator.admission_for(&item, &version);
        let second_entered = Arc::new(AtomicBool::new(false));
        let second_started = Arc::clone(&second_entered);
        let second_hydrator = Arc::clone(&hydrator);
        let second = tokio::spawn(async move {
            second_started.store(true, Ordering::Release);
            second_hydrator
                .hydrate_inner(
                    7,
                    item_id().text().to_owned(),
                    Some("content-v1".to_owned()),
                    Arc::new(NoopProgress),
                    CancellationToken::new(),
                )
                .await
        });
        wait_for_test(|| {
            second_entered.load(Ordering::Acquire) && Arc::strong_count(&admission) >= 3
        })
        .await;
        assert_eq!(
            hydrator.coordinator.reader_count(TransferId(1)),
            0,
            "the new opener cannot attach after the last close"
        );
        assert!(
            StateStore::open(&database)
                .expect("state")
                .read_txn()
                .expect("read")
                .transfer(TransferId(2))
                .expect("second transfer query")
                .is_none(),
            "the new generation cannot be created before durable cancellation"
        );

        last_reader_release
            .send(())
            .expect("release last-reader checkpoint");
        assert_eq!(
            checkpoint_receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("source-cancel checkpoint"),
            CancelCheckpoint::SourceGenerationCancelled
        );
        assert_eq!(source.fetches.load(Ordering::SeqCst), 1);
        assert!(
            StateStore::open(&database)
                .expect("state")
                .read_txn()
                .expect("read")
                .transfer(TransferId(2))
                .expect("second transfer query")
                .is_none(),
            "the new generation cannot reset the old signal while cancellation owns admission"
        );
        source_cancel_release
            .send(())
            .expect("release source-cancel checkpoint");
        wait_for_test(|| {
            source.fetches.load(Ordering::SeqCst) == 2
                && hydrator.coordinator.reader_count(TransferId(2)) == 1
        })
        .await;
        replacement_gate.release();

        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(first, second)
        })
        .await
        .expect("cancellation race resolves");
        assert!(matches!(
            first.expect("first task"),
            Err(DriveError::Cancelled { .. })
        ));
        let materialized = second
            .expect("second task")
            .expect("replacement generation hydrates");
        assert_eq!(fs::read(materialized.path).expect("cache bytes"), bytes);
        assert_eq!(source.fetches.load(Ordering::SeqCst), 2);
        assert_eq!(first_dropped.load(Ordering::SeqCst), 1);

        let mut store = StateStore::open(&database).expect("state");
        let read = store.read_txn().expect("read");
        let abandoned = read
            .transfer(TransferId(1))
            .expect("first transfer query")
            .expect("first transfer");
        assert_eq!(abandoned.state, TransferState::Cancelled);
        assert!(abandoned.completed_ranges.is_empty());
        assert_eq!(abandoned.temp_ref, None);
        let replacement = read
            .transfer(TransferId(2))
            .expect("second transfer query")
            .expect("second transfer");
        assert_eq!(replacement.state, TransferState::Done);
        assert!(read.cache_entry(&item_id()).expect("cache query").is_some());
        drop(read);

        let layout = shared_state_layout(root.text().to_owned()).expect("layout");
        assert_eq!(
            fs::read_dir(Path::new(&layout.cache_dir).join("transfers"))
                .expect("staging directory")
                .count(),
            0,
            "neither generation leaves unpublished staging"
        );
    }

    #[test]
    fn state_catalog_refuses_every_persisted_content_gate_and_preserves_refresh_identity() {
        let root = TempRoot::new();
        let bytes = b"payload";
        let database = seed(&root, bytes);
        let catalog = StateFetchCatalog::open(&database, AccountId(7)).expect("catalog");

        let assert_availability = |expected| {
            let Some(CatalogEntry::File(target)) = catalog.resolve(&item_id()) else {
                panic!("attachment target")
            };
            assert_eq!(target.availability, expected);
            assert_eq!(
                target.remote_file_type,
                Some(RemoteFileType::Document),
                "persisted representation supplies getRemoteFile's exact type"
            );
        };
        assert_availability(SourceAvailability::Fetchable);

        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("write");
            let mut facts = tx
                .read()
                .attachment(&attachment_key())
                .expect("query")
                .expect("attachment")
                .facts;
            facts.can_be_saved = false;
            tx.upsert_attachment(&facts).expect("restrict save");
            tx.commit().expect("commit");
        }
        assert_availability(SourceAvailability::Restricted);

        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("write");
            let mut facts = tx
                .read()
                .attachment(&attachment_key())
                .expect("query")
                .expect("attachment")
                .facts;
            facts.can_be_saved = true;
            facts.availability = StateAttachmentAvailability::ViewOnce;
            tx.upsert_attachment(&facts).expect("view once");
            tx.commit().expect("commit");
        }
        assert_availability(SourceAvailability::ViewOnce);

        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("write");
            let mut facts = tx
                .read()
                .attachment(&attachment_key())
                .expect("query")
                .expect("attachment")
                .facts;
            facts.availability = StateAttachmentAvailability::Fetchable;
            tx.upsert_attachment(&facts).expect("restore attachment");
            let mut chat = tx.read().chat(&chat_key()).expect("query").expect("chat");
            chat.is_protected = true;
            tx.upsert_chat(&chat).expect("protect chat");
            tx.commit().expect("commit");
        }
        assert_availability(SourceAvailability::Restricted);

        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("write");
            let mut chat = tx.read().chat(&chat_key()).expect("query").expect("chat");
            chat.is_protected = false;
            tx.upsert_chat(&chat).expect("restore chat");
            tx.commit().expect("commit");
        }
        catalog
            .persist_refresh(
                &item_id(),
                &RefreshedFileTarget {
                    file_id: 1700,
                    remote_id: Some("remote-new".to_owned()),
                    remote_unique_id: Some("stable-content-id".to_owned()),
                    size: Some(bytes.len() as u64),
                    availability: SourceAvailability::Fetchable,
                    can_be_saved: true,
                },
            )
            .expect("locator refresh");
        let mut store = StateStore::open(&database).expect("state");
        let stored = store
            .read_txn()
            .expect("read")
            .attachment(&attachment_key())
            .expect("query")
            .expect("attachment")
            .facts;
        assert_eq!(stored.telegram_file_id.as_deref(), Some("remote-new"));
        assert_eq!(stored.telegram_local_file_id, Some(1700));
        assert_eq!(stored.content_version.as_str(), "content-v1");

        let missing_identity = catalog
            .persist_refresh(
                &item_id(),
                &RefreshedFileTarget {
                    file_id: 700,
                    remote_id: Some("remote-newer".to_owned()),
                    remote_unique_id: None,
                    size: Some(bytes.len() as u64),
                    availability: SourceAvailability::Fetchable,
                    can_be_saved: true,
                },
            )
            .expect_err("known stable identity may not disappear");
        assert!(matches!(
            missing_identity,
            SourceError::VersionConflict { .. }
        ));
        let missing_extent = catalog
            .persist_refresh(
                &item_id(),
                &RefreshedFileTarget {
                    file_id: 700,
                    remote_id: Some("remote-newer".to_owned()),
                    remote_unique_id: Some("stable-content-id".to_owned()),
                    size: None,
                    availability: SourceAvailability::Fetchable,
                    can_be_saved: true,
                },
            )
            .expect_err("known exact extent may not disappear");
        assert!(matches!(
            missing_extent,
            SourceError::VersionConflict { .. }
        ));
    }

    #[test]
    fn state_catalog_resolves_and_persists_allowed_story_locator_refresh() {
        let root = TempRoot::new();
        let bytes = b"story catalog bytes";
        let database = seed_story(&root, bytes);
        let catalog = StateFetchCatalog::open(&database, AccountId(7)).expect("catalog");
        let active_item = story_item(StoryAppearanceLocation::Active);
        let Some(CatalogEntry::File(target)) = catalog.resolve(&active_item) else {
            panic!("allowed story target");
        };
        assert_eq!(target.file_id, 791);
        assert_eq!(
            target.remote_file_type,
            Some(RemoteFileType::VideoStory),
            "story role supplies getRemoteFile's exact type"
        );
        assert_eq!(
            target.remote_id.as_deref(),
            Some("story-remote"),
            "durable story locator is available for current-session rebind"
        );
        assert_eq!(
            target.refresh,
            RefreshTarget::Story {
                poster_chat_id: 100,
                story_id: 91,
            }
        );
        assert_eq!(target.availability, SourceAvailability::Fetchable);
        assert_eq!(target.remote_unique_id.as_deref(), Some("story-unique"));
        assert_eq!(target.size, Some(bytes.len() as u64));
        assert_eq!(target.version.as_str(), "story-content-v1");

        catalog
            .persist_refresh(
                &active_item,
                &RefreshedFileTarget {
                    file_id: 1791,
                    remote_id: Some("story-remote-refreshed".to_owned()),
                    remote_unique_id: Some("story-unique".to_owned()),
                    size: Some(bytes.len() as u64),
                    availability: SourceAvailability::Fetchable,
                    can_be_saved: true,
                },
            )
            .expect("persist story refresh");
        let mut store = StateStore::open(&database).expect("state");
        let story = store
            .read_txn()
            .expect("read")
            .story(&story_key())
            .expect("story query")
            .expect("story");
        let primary = story
            .locators
            .into_iter()
            .find(|locator| locator.is_primary)
            .expect("primary locator");
        assert_eq!(
            primary.remote_file_id.as_deref(),
            Some("story-remote-refreshed")
        );
        assert_eq!(primary.local_file_id, Some(1791));
        assert_eq!(primary.content_version.as_str(), "story-content-v1");
    }

    #[test]
    fn startup_reconciles_interrupted_transfer_without_losing_ranges() {
        let root = TempRoot::new();
        let bytes = b"relaunch payload";
        let database = seed(&root, bytes);
        let layout = shared_state_layout(root.text().to_owned()).expect("layout");
        fs::create_dir_all(Path::new(&layout.cache_dir).join("transfers")).expect("staging dir");
        let staging = Path::new(&layout.cache_dir)
            .join("transfers")
            .join("interrupted.partial");
        fs::write(&staging, &bytes[..4]).expect("staged prefix");

        let transfer = {
            let mut store = StateStore::open(&database).expect("state");
            let machine = TransferMachine::default();
            let requested = ByteRange::new(0, bytes.len() as u64).expect("range");
            let outcome = machine
                .request(
                    &mut store,
                    &item_id(),
                    &[requested],
                    Priority::FOREGROUND,
                    1,
                )
                .expect("request");
            let transfer = match outcome {
                gramdrive_engine::transfer::RequestOutcome::Created { transfer, .. } => transfer,
                gramdrive_engine::transfer::RequestOutcome::Attached { .. } => {
                    panic!("fresh transfer")
                }
            };
            let mut claim = match machine.claim(&mut store, 2).expect("claim") {
                gramdrive_engine::transfer::ClaimOutcome::Claimed(claim) => *claim,
                other => panic!("expected claim, got {other:?}"),
            };
            assert_eq!(claim.record().id, transfer);
            machine
                .record_progress(
                    &mut store,
                    &mut claim,
                    &[ByteRange::new(0, 4).expect("prefix")],
                    staging.to_str().expect("path"),
                    3,
                )
                .expect("progress");
            transfer
        };

        let _hydrator = Hydrator::shared(root.text()).expect("relaunch composition");
        let mut store = StateStore::open(&database).expect("state");
        let row = store
            .read_txn()
            .expect("read")
            .transfer(transfer)
            .expect("query")
            .expect("transfer");
        assert_eq!(row.state, TransferState::Queued);
        assert_eq!(
            row.completed_ranges,
            vec![ByteRange::new(0, 4).expect("prefix")]
        );
        assert_eq!(row.temp_ref.as_deref(), staging.to_str());
    }

    #[test]
    fn promotion_host_refuses_to_move_files_outside_owned_staging() {
        let root = TempRoot::new();
        let staging = root.0.join("cache").join("transfers");
        let blobs = root.0.join("cache").join("blobs").join("sha256");
        fs::create_dir_all(&staging).expect("staging");
        let outside = root.0.join("outside.bin");
        fs::write(&outside, b"must remain").expect("outside file");
        let mut host = FilePromotionHost::new(staging, blobs).expect("promotion host");

        let result = host.promote(outside.to_str(), &ContentHash::Sha256([0; 32]));
        assert!(result.is_err());
        assert_eq!(fs::read(&outside).expect("outside remains"), b"must remain");
    }

    #[test]
    fn generated_document_retention_purge_replays_on_production_relaunch() {
        let root = TempRoot::new();
        let database = seed(&root, b"seed");
        let layout = shared_state_layout(root.text().to_owned()).expect("layout");
        let generated_parent = Path::new(&layout.cache_dir)
            .join("generated")
            .join("account-7")
            .join("chat-100")
            .join("2026");
        fs::create_dir_all(&generated_parent).expect("generated parent");
        let markdown = generated_parent.join("Messages.md");
        let ndjson = generated_parent.join("Messages.ndjson");
        fs::write(&markdown, b"generated markdown").expect("markdown");
        fs::write(&ndjson, b"generated ndjson").expect("ndjson");

        let store = StateStore::open(&database).expect("state");
        for (queued_at_ms, path) in [(10_i64, &markdown), (11_i64, &ndjson)] {
            store
                .connection()
                .execute(
                    "INSERT INTO retention_purge_queue (
                         account_id, materialization_ref, queued_at_ms)
                     VALUES (?1, ?2, ?3)",
                    (
                        scope().account.account_id.0,
                        path.to_str().expect("generated UTF-8"),
                        queued_at_ms,
                    ),
                )
                .expect("queue generated materialization");
        }
        drop(store);

        let _relaunched =
            Hydrator::shared(root.text()).expect("startup drains generated document purge");
        assert!(!markdown.exists());
        assert!(!ndjson.exists());
        let mut store = StateStore::open(&database).expect("state");
        assert!(
            store
                .read_txn()
                .expect("read queue")
                .retention_purge_queue(scope().account, 10)
                .expect("purge queue")
                .is_empty(),
            "successful physical removal is acknowledged durably"
        );
    }

    #[test]
    fn generated_document_purge_rejects_cache_escapes() {
        let root = TempRoot::new();
        let cache = root.0.join("cache");
        let storage = FileStorage::new(cache.clone()).expect("storage");
        let outside = root.0.join("Messages.md");
        let wrong_name = cache
            .join("generated")
            .join("account-7")
            .join("payload.bin");
        let traversal = cache
            .join("generated")
            .join("account-7")
            .join("..")
            .join("Messages.md");
        fs::write(&outside, b"outside").expect("outside");
        fs::create_dir_all(wrong_name.parent().expect("parent")).expect("generated parent");
        fs::write(&wrong_name, b"wrong name").expect("wrong-name file");

        assert!(
            storage
                .remove_cache_object(outside.to_str().expect("outside UTF-8"))
                .is_err()
        );
        assert!(
            storage
                .remove_cache_object(wrong_name.to_str().expect("wrong name UTF-8"))
                .is_err()
        );
        assert!(
            storage
                .remove_cache_object(traversal.to_str().expect("traversal UTF-8"))
                .is_err()
        );
        assert_eq!(fs::read(outside).expect("outside remains"), b"outside");
        assert_eq!(
            fs::read(wrong_name).expect("wrong-name file remains"),
            b"wrong name"
        );
    }

    #[test]
    fn thumbnail_publication_is_atomic_and_version_scoped() {
        let root = TempRoot::new();
        let cache = root.0.join("cache");
        let storage = FileStorage::new(cache.clone()).expect("storage");
        let first_version = ContentVersion::new("content-v1").expect("version");
        let second_version = ContentVersion::new("content-v2").expect("version");

        let first = storage
            .publish_thumbnail(&item_id(), &first_version, 256, 128, b"preview-one")
            .expect("first preview");
        let repeated = storage
            .publish_thumbnail(&item_id(), &first_version, 256, 128, b"preview-one")
            .expect("cached preview");
        let second = storage
            .publish_thumbnail(&item_id(), &second_version, 256, 128, b"preview-two")
            .expect("new-version preview");

        assert_eq!(first, repeated);
        assert_ne!(first, second);
        assert_eq!(fs::read(&first).expect("first bytes"), b"preview-one");
        assert_eq!(fs::read(&second).expect("second bytes"), b"preview-two");

        drop(storage);
        let relaunched = FileStorage::new(cache).expect("relaunched storage");
        let refreshed = relaunched
            .publish_thumbnail(&item_id(), &first_version, 256, 128, b"preview-new")
            .expect("equal-length refreshed preview");
        assert_eq!(refreshed, first, "the cache key remains stable");
        assert_eq!(
            fs::read(&refreshed).expect("refreshed bytes"),
            b"preview-new",
            "same-length locator refresh must replace stale bytes after relaunch"
        );
        let partials = fs::read_dir(root.0.join("cache").join("thumbnails"))
            .expect("thumbnail directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".partial"))
            .count();
        assert_eq!(partials, 0);
    }

    #[test]
    fn thumbnail_source_failures_keep_typed_retry_and_policy_categories() {
        assert!(matches!(
            source_error(SourceError::Restricted {
                detail: "protected".to_owned()
            }),
            DriveError::Restricted { .. }
        ));
        assert!(matches!(
            source_error(SourceError::Unavailable {
                detail: "offline".to_owned()
            }),
            DriveError::SourceUnavailable { .. }
        ));
        assert!(matches!(
            source_error(SourceError::NotFound {
                detail: "the TDLib source object is gone".to_owned()
            }),
            DriveError::SourceUnavailable { .. }
        ));
        assert!(matches!(
            source_error(SourceError::RateLimited {
                retry_after: Some(StdDuration::from_millis(750)),
                detail: "flood".to_owned()
            }),
            DriveError::RateLimited {
                retry_after_ms: Some(750),
                ..
            }
        ));
    }

    #[test]
    fn renderer_not_found_after_live_admission_is_source_unavailable() {
        // The renderer is reached only after durable admission accepted the
        // row. Its private source miss must therefore travel over the wire as
        // a retryable source condition, never as a durable-row deletion.
        assert!(matches!(
            failure_error(FailureCategory::NotFound),
            DriveError::SourceUnavailable { .. }
        ));
    }

    #[test]
    fn state_thumbnail_catalog_uses_preview_locator_and_live_policy() {
        let root = TempRoot::new();
        let database = seed(&root, b"full-media");
        let record = normalize_message(&serde_json::json!({
            "@type": "message",
            "id": 5,
            "chat_id": 100,
            "date": 1_700_000_000,
            "sender_id": {"@type": "messageSenderUser", "user_id": 42},
            "can_be_saved": true,
            "content": {
                "@type": "messageDocument",
                "caption": {"@type": "formattedText", "text": "", "entities": []},
                "document": {
                    "file_name": "payload.bin",
                    "mime_type": "application/octet-stream",
                    "document": {
                        "@type": "file",
                        "id": 700,
                        "size": 10,
                        "remote": {"id": "main", "unique_id": "stable-content-id"}
                    },
                    "thumbnail": {
                        "format": {"@type": "thumbnailFormatJpeg"},
                        "width": 64,
                        "height": 48,
                        "file": {
                            "@type": "file",
                            "id": 701,
                            "size": 128,
                            "remote": {"id": "preview", "unique_id": "preview-stable"}
                        }
                    }
                }
            }
        }))
        .expect("normalized preview message");
        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("transaction");
            tx.apply_message_changes(
                &chat_key(),
                &[MessageChange::Observed(MessageRevision {
                    message_id: MessageId(5),
                    sender_id: Some(42),
                    sent_at_ms: record.sent_at_ms,
                    edited_at_ms: None,
                    observed_at_ms: record.sent_at_ms,
                    payload_schema: NORMALIZED_MESSAGE_SCHEMA_FAMILY,
                    payload: serde_json::to_vec(&record).expect("payload"),
                })],
            )
            .expect("message payload");
            tx.commit().expect("payload commit");
        }
        let catalog = StateThumbnailCatalog::open(&database, AccountId(7)).expect("catalog");
        let target = catalog.resolve(&item_id()).expect("preview target");
        assert_eq!(target.availability, SourceAvailability::Fetchable);
        assert_eq!(
            target.downloadable.as_ref().map(|preview| preview.file_id),
            Some(701),
            "the dedicated preview locator is used, never full-media file 700"
        );

        {
            let mut store = StateStore::open(&database).expect("state");
            let tx = store.write_txn().expect("transaction");
            tx.upsert_chat(&ChatRecord {
                key: chat_key(),
                chat_type: ChatType::Private,
                title: "Chat".to_owned(),
                username: None,
                is_protected: true,
                archive_mode: false,
                metadata_version: gramdrive_model::version::MetadataVersion::new("chat-m2")
                    .expect("chat version"),
                left_at_ms: None,
                deleted_at_ms: None,
                last_update_at_ms: Some(2),
            })
            .expect("protected chat");
            tx.commit().expect("policy commit");
        }
        let protected = catalog.resolve(&item_id()).expect("protected target");
        assert_eq!(protected.availability, SourceAvailability::Restricted);
    }
}
