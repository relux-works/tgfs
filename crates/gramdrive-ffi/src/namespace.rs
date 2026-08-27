//! Long-lived Telegram namespace and normalized-content coordinator.
//!
//! One session owns one authorized account's TDLib database for the agent
//! lifecycle. It composes authorization readiness, the ordered folder catalog,
//! bounded list snapshots, chat metadata updates, resumable history, live
//! message updates, normalized durable repositories, and the provider-facing
//! virtual tree. History begins only after the root/chat snapshot is durable;
//! Archive media starts only after metadata completion and host policy gates.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use gramdrive_engine::backfill::{
    BackfillDemand, BackfillPriority, BackfillScheduler, BackfillStep, DiskState, HostConditions,
    IdleReason, NetworkState, PowerState,
};
use gramdrive_engine::render::markdown::{
    Attachment as RenderAttachment, AttachmentFidelity as RenderAttachmentFidelity,
    Availability as RenderAvailability, DisplayTimeZone, Entity as RenderEntity,
    EntityKind as RenderEntityKind, MediaKind as RenderMediaKind, MessageBody,
    Reaction as RenderReaction, ReactionKey as RenderReactionKey,
    ServiceAction as RenderServiceAction, TelegramRepresentation as RenderRepresentation,
};
use gramdrive_engine::render_pipeline::{
    DecodedRevision, MessagePayloadDecoder, RenderPipelineError, compose_chat_metadata,
    compose_month, publish_chat_metadata, publish_month, stage_chat_metadata, stage_month,
};
use gramdrive_engine::render_plan::{DocClass, dirty_affected, plan_worklist};
use gramdrive_model::identity::{
    AccountId, AppearanceKey, AttachmentKey, CanonicalKey, ChatId, ChatKey, ChatListKey,
    ChatListKind, DocPartition, ItemId, ItemKey, MessageId, MessageKey, StoryAppearanceLocation,
    StoryId, StoryKey,
};
use gramdrive_model::naming::{NameKind, SiblingName, attachment_leaf_name, resolve_siblings};
use gramdrive_model::tree::{
    AccountRecord as TreeAccountRecord, AttachmentRecord as TreeAttachmentRecord,
    ChatRecord as TreeChatRecord, DocSchemas, FolderRecord as TreeFolderRecord, MonthStamp,
    NodeKind, StoryRecord as TreeStoryRecord, TreeNode, TreeProjection,
};
use gramdrive_model::version::{ContentVersion, MetadataVersion};
use gramdrive_source_tdjson::message::{
    AttachmentAvailability as SourceAttachmentAvailability, ContentRestriction, FormattedText,
    MessageContent, MessageRecord, NORMALIZED_MESSAGE_SCHEMA_FAMILY,
    ReactionKind as SourceReactionKind, ReplyTarget, SenderRef,
    ServiceAction as SourceServiceAction, TextEntityKind, TopicRef, redact_restricted_content,
};
use gramdrive_source_tdjson::{
    AttachmentFidelity as MappedAttachmentFidelity,
    AttachmentLogicalKind as MappedAttachmentLogicalKind, AuthMachine, AuthState, ChatCrawl,
    ChatMetadata, CrawlMachine, CrawlPlan, CrawlPriority, CrawlStep, CrawlWindow,
    FolderCatalogMachine, HistoryCommit, ListCommit, LiveChange, LiveChat, LiveCommit, LiveMachine,
    LivePlan, LiveStep, MappedAttachment, MembershipChange, SnapshotChatKind, SnapshotError,
    SnapshotMachine, SnapshotPlan, SnapshotStep, StoryAccountKind,
    StoryArchiveCapability as SourceStoryArchiveCapability, StoryChatKind, StoryChatPlan,
    StoryCommit, StoryContentKind, StoryError, StoryFileType as SourceStoryFileType, StoryMachine,
    StoryObservation, StoryScanCursor, StoryStep, TdClient,
    TelegramRepresentation as MappedTelegramRepresentation, UpdateBatch, UpdateMachine,
    UpdateRecvError, UpdateStream, background_story_request_allowed, map_message_attachments,
    normalize_story_account,
};
use gramdrive_state::repo::{
    AttachmentAvailability as StateAttachmentAvailability, AttachmentFacts,
    AttachmentFidelity as StateAttachmentFidelity,
    AttachmentLogicalKind as StateAttachmentLogicalKind, ChatContentPhase,
    ChatContentProgressRecord, ChatListEntry, ChatRecord, ChatSyncRecord, ChatType, FileFacts,
    FolderRecord, ItemAvailability, ItemRecord, MessageChange, MessagePayload, MessageRevision,
    NamespaceBootstrapRecord, StoryAppearanceRecord, StoryArchiveEligibility,
    StoryContentLocatorRecord, StoryContentState, StoryFacts,
    StoryLocatorFileType as StateStoryLocatorFileType, StorySyncPhase, StorySyncProgressRecord,
    SyncWindow, TelegramRepresentation as StateTelegramRepresentation,
};
use gramdrive_state::{StateError, StateStore};
use serde_json::json;

use crate::api::DriveError;
use crate::auth::{
    AuthSessionConfig, ScopeGuard, SecretVault, VaultSecrets, recover_auth_finalization_locked,
    shared_runtime, td_to_drive_error,
};
use crate::hydration::Hydrator;
use crate::shared_state::{shared_state_layout, upsert_fixed_root_structure};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const UPDATE_POLL: Duration = Duration::from_millis(250);
/// A sustained TDLib update stream can keep `recv_timeout` continuously ready.
/// Yield after that busy path regardless of how long the bounded work itself
/// took; an elapsed-period floor can be defeated by an already-expensive tick.
/// Idle 250 ms polls receive no additional delay.
// Keep meaningful headroom below the installed acceptance ceiling even while
// a large durable render backlog is draining; 100 ms was measurably marginal
// (two independent windows reached 80.6% and 81.4% on the preserved profile).
const CONTENT_LOOP_BUSY_YIELD: Duration = Duration::from_millis(150);
/// Upper bound for folding one TDLib metadata burst into a single durable
/// checkpoint. Initial authorization can replay thousands of chat positions;
/// reconciling the account projection after every arrival monopolizes a core.
/// A fixed window keeps provider latency bounded while making the work
/// proportional to checkpoints rather than update count.
const UPDATE_CHECKPOINT_WINDOW: Duration = Duration::from_millis(100);
const MAX_RETRY_ATTEMPTS: u32 = 5;
const MAX_LIVE_STEPS_PER_TICK: usize = 32;
// Story/profile discovery can span thousands of chats. Keep its quantum
// deliberately small so one long story pass cannot delay every history page.
const MAX_STORY_STEPS_PER_TICK: usize = 4;
// Rendering is provider-visible publication work and may involve filesystem
// staging plus several state transactions. Four appearance rows keep each
// filesystem/SQLite burst comfortably below the installed CPU ceiling (eight
// remained marginal on the preserved profile); paired formats and duplicate
// list appearances still collapse to fewer month renders. The durable
// least-advanced-first index and mandatory busy-stream yield preserve fairness.
const MAX_RENDER_WORKLIST_ITEMS_PER_TICK: u32 = 4;
// History gets a bounded continuation slice after each scheduler pick. That is
// long enough to cross ordinary month boundaries without rotating a partially
// crawled chat behind an account-wide first-page pass. Projection/render work
// is published once at the slice boundary instead of rebuilding the entire
// provider tree after every 100-message page.
const MAX_BACKGROUND_HISTORY_PAGES_PER_SLICE: u64 = 8;
const MAX_FOREGROUND_HISTORY_PAGES_PER_SLICE: u64 = 16;
const MAX_PROJECTION_CHATS_PER_SLICE: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionDepth {
    Shallow,
    Deep,
}

#[derive(Clone, Copy)]
struct SessionIo<'a> {
    client: &'a TdClient,
    updates: &'a UpdateStream,
    cancelled: &'a AtomicBool,
}

/// Priority signal accepted by the one agent-owned content session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ChatHistoryPriority {
    /// Remove foreground demand; bounded background scheduling remains.
    Background,
    /// The user explicitly opened or requested the chat.
    Requested,
    /// The chat is currently visible and preempts requested/background work.
    Visible,
}

/// Connectivity class supplied by the native host for Archive backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ArchiveNetworkCondition {
    /// Unmetered connectivity.
    Online,
    /// Connected through an expensive or constrained path.
    Metered,
    /// No usable connectivity.
    Offline,
}

/// Power state supplied by the native host for Archive backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ArchivePowerCondition {
    /// Normal power operation.
    Unconstrained,
    /// Low-power mode or impending sleep.
    Saving,
}

/// Disk headroom supplied by the native host for Archive backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ArchiveDiskCondition {
    /// Enough headroom for eager content.
    Ample,
    /// Low headroom: eager hydration is suspended.
    Low,
    /// Critical headroom: eager hydration is suspended.
    Critical,
}

/// Native host conditions that gate Archive eager hydration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ArchiveHostConditions {
    /// Current connectivity.
    pub network: ArchiveNetworkCondition,
    /// Current power state.
    pub power: ArchivePowerCondition,
    /// Current disk headroom.
    pub disk: ArchiveDiskCondition,
}

impl From<ArchiveHostConditions> for HostConditions {
    fn from(value: ArchiveHostConditions) -> Self {
        Self {
            network: match value.network {
                ArchiveNetworkCondition::Online => NetworkState::Online,
                ArchiveNetworkCondition::Metered => NetworkState::Metered,
                ArchiveNetworkCondition::Offline => NetworkState::Offline,
            },
            power: match value.power {
                ArchivePowerCondition::Unconstrained => PowerState::Unconstrained,
                ArchivePowerCondition::Saving => PowerState::Saving,
            },
            disk: match value.disk {
                ArchiveDiskCondition::Ample => DiskState::Ample,
                ArchiveDiskCondition::Low => DiskState::Low,
                ArchiveDiskCondition::Critical => DiskState::Critical,
            },
        }
    }
}

/// The most unspent foreground admissions the demand ledger carries.
///
/// One admission is one accepted user gesture and it only has to outlive the
/// gap to the namespace worker's next scheduler boundary, so a small bound is
/// enough. It also keeps a looping or misbehaving provider client from growing
/// the ledger without limit; the oldest admission is dropped first.
const MAX_UNSPENT_ADMISSIONS: usize = 64;

/// A foreground hint that was accepted but has not yet been handed to the
/// scheduler as a real history turn.
#[derive(Debug, Clone, Copy)]
struct Admission {
    /// The highest priority this chat was ever admitted at while unspent.
    priority: ChatHistoryPriority,
    /// Admission order, used only to evict the oldest at the bound.
    seq: u64,
}

/// The agent-owned foreground demand queue.
///
/// It holds two things that are deliberately not the same. `priorities` is the
/// host's *live* view — which chats are on screen right now — and it is what
/// decides whether an in-flight crawl keeps its slice. `unspent` is a ledger of
/// admitted hints that have not yet bought a history turn, and it is what the
/// scheduler sees when it picks the next chat.
///
/// The split exists because the File Provider demand lifecycle is far faster
/// than one scheduler boundary: Finder enumerates a chat folder, the enumerator
/// signals `Visible`, and `invalidate()` signals `Background` again milliseconds
/// later — while the namespace worker is still inside another chat's crawl. A
/// ledger that only tracked the live view lost that edge entirely and the opened
/// chat never advanced (BUG-260728-2qfzbd).
#[derive(Debug, Default)]
struct ContentDemandState {
    priorities: BTreeMap<i64, ChatHistoryPriority>,
    unspent: BTreeMap<i64, Admission>,
    next_seq: u64,
}

impl ContentDemandState {
    fn set(&mut self, chat_id: i64, priority: ChatHistoryPriority) {
        match priority {
            // A release drops the live view only. Any admission this chat has
            // not yet spent survives, so the turn it was promised still comes.
            ChatHistoryPriority::Background => {
                self.priorities.remove(&chat_id);
            }
            ChatHistoryPriority::Requested | ChatHistoryPriority::Visible => {
                self.priorities.insert(chat_id, priority);
                self.admit(chat_id, priority);
            }
        }
    }

    /// Records an unspent admission, keeping the earliest arrival order and the
    /// strongest priority the chat was admitted at.
    fn admit(&mut self, chat_id: i64, priority: ChatHistoryPriority) {
        if let Some(existing) = self.unspent.get_mut(&chat_id) {
            if matches!(priority, ChatHistoryPriority::Visible) {
                existing.priority = priority;
            }
            return;
        }
        if self.unspent.len() >= MAX_UNSPENT_ADMISSIONS
            && let Some(oldest) = self
                .unspent
                .iter()
                .min_by_key(|(_, admission)| admission.seq)
                .map(|(&chat_id, _)| chat_id)
        {
            self.unspent.remove(&oldest);
        }
        let seq = self.next_seq;
        // Saturating rather than wrapping: the sequence is an ordering, and an
        // ordering that wraps would let the watermark below retire an admission
        // no plan ever saw. u64 does not reach this in any real lifetime.
        self.next_seq = self.next_seq.saturating_add(1);
        self.unspent.insert(chat_id, Admission { priority, seq });
    }

    /// The host's live foreground view. Used for slice preemption, where a
    /// released chat genuinely is no longer on screen.
    fn live_snapshot(&self) -> (Vec<ChatId>, Vec<ChatId>) {
        Self::split(
            self.priorities
                .iter()
                .map(|(&chat, &priority)| (chat, priority)),
        )
    }

    /// Every unspent admission plus live `Visible` demand, together with the
    /// watermark the snapshot was taken at. A live `Requested` edge still
    /// preempts the active background slice through [`Self::live_snapshot`],
    /// but its one admitted scheduler turn must not repeat until the read
    /// settles; one content read is one promised turn. A visible folder stays
    /// foreground while it remains on screen.
    fn scheduling_snapshot(&self) -> DemandPlan {
        let mut merged = self
            .priorities
            .iter()
            .filter_map(|(&chat_id, &priority)| {
                matches!(priority, ChatHistoryPriority::Visible).then_some((chat_id, priority))
            })
            .collect::<BTreeMap<_, _>>();
        for (&chat_id, admission) in &self.unspent {
            let slot = merged.entry(chat_id).or_insert(admission.priority);
            if matches!(admission.priority, ChatHistoryPriority::Visible) {
                *slot = ChatHistoryPriority::Visible;
            }
        }
        let (visible, requested) = Self::split(merged.into_iter());
        DemandPlan {
            visible,
            requested,
            watermark: self.next_seq,
        }
    }

    fn split(
        entries: impl Iterator<Item = (i64, ChatHistoryPriority)>,
    ) -> (Vec<ChatId>, Vec<ChatId>) {
        let mut visible = Vec::new();
        let mut requested = Vec::new();
        for (chat_id, priority) in entries {
            match priority {
                ChatHistoryPriority::Visible => visible.push(ChatId(chat_id)),
                ChatHistoryPriority::Requested => requested.push(ChatId(chat_id)),
                ChatHistoryPriority::Background => {}
            }
        }
        (visible, requested)
    }

    /// Retires one chat's admission — it has had the turn it was promised.
    fn spend(&mut self, chat_id: i64) {
        self.unspent.remove(&chat_id);
    }

    /// Retires every admission held at visible strength that the plan saw.
    fn spend_visible(&mut self, watermark: u64) {
        self.unspent.retain(|_, admission| {
            admission.seq >= watermark
                || !matches!(admission.priority, ChatHistoryPriority::Visible)
        });
    }

    /// Retires every admission the plan saw, keeping any that arrived while it
    /// was running — those were never offered and are still owed a turn.
    fn spend_all(&mut self, watermark: u64) {
        self.unspent
            .retain(|_, admission| admission.seq >= watermark);
    }

    fn remove(&mut self, chat_id: i64) {
        self.priorities.remove(&chat_id);
        self.unspent.remove(&chat_id);
    }
}

/// One scheduling view of the demand queue, and the admission watermark it was
/// taken at. Only admissions numbered below the watermark were offered to the
/// plan, so only those can be retired by it — a hint that lands while
/// `plan_next` is running has not been honored by anything yet.
#[derive(Debug)]
struct DemandPlan {
    visible: Vec<ChatId>,
    requested: Vec<ChatId>,
    watermark: u64,
}

/// Privacy-safe progress of one account namespace session.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum NamespaceProgress {
    /// Roots exist and the source metadata snapshot is in flight.
    Preparing,
    /// TDLib crossed its live authorization boundary. This does not claim
    /// namespace or projection readiness.
    Authorized,
    /// The complete Telegram folder catalog was observed and persisted.
    FolderCatalog,
    /// The bounded chat-list snapshot stage is in flight.
    SnapshotList,
    /// One bounded post-ready deep-projection slice committed.
    ProjectionSlice {
        /// Number of chats committed by this slice (aggregate only).
        processed_chat_count: u64,
    },
    /// Metadata is durable and live updates are being applied.
    Ready {
        /// Canonical chat rows, counted without names.
        canonical_chat_count: u64,
        /// List/folder appearances, counted without names.
        appearance_count: u64,
    },
    /// Existing durable content remains enumerable while one source operation
    /// is unavailable. The owned session stays alive and may publish `Ready`
    /// again after a later successful update.
    Degraded {
        /// Stable privacy-safe failure category.
        category: String,
        /// Whether the source operation can recover without reauthorization.
        retryable: bool,
    },
    /// The session stopped with an actionable, privacy-safe category.
    Failed {
        /// Stable privacy-safe failure category.
        category: String,
        /// Whether a relaunch/retry is meaningful.
        retryable: bool,
    },
    /// The owner intentionally stopped the session.
    Stopped,
}

/// Receives namespace progress synchronously on the session worker thread.
#[uniffi::export(with_foreign)]
pub trait NamespaceProgressListener: Send + Sync {
    /// One privacy-safe state transition or committed metadata batch.
    fn on_progress(&self, progress: NamespaceProgress);
}

/// One long-lived account namespace owner.
#[derive(uniffi::Object)]
pub struct NamespaceSession {
    cancelled: Arc<AtomicBool>,
    content_demand: Arc<Mutex<ContentDemandState>>,
    archive_conditions: Arc<RwLock<HostConditions>>,
    client: TdClient,
    hydrator: Arc<Hydrator>,
    hydration_account_id: i64,
    hydration_registration: u64,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for NamespaceSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NamespaceSession")
            .field("cancelled", &self.cancelled.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

#[uniffi::export]
impl NamespaceSession {
    /// Starts the namespace and content coordinator for one existing account.
    #[uniffi::constructor]
    pub fn start(
        config: AuthSessionConfig,
        account_id: i64,
        vault: Arc<dyn SecretVault>,
        listener: Arc<dyn NamespaceProgressListener>,
    ) -> Result<Arc<Self>, DriveError> {
        config.validate()?;
        if account_id <= 0 {
            return Err(DriveError::InvalidArgument {
                detail: "account_id must be a positive Telegram identity".to_owned(),
            });
        }
        let runtime = shared_runtime()?;
        let account = AccountId(account_id);
        let guard = ScopeGuard::acquire(&config.data_dir, account)?;
        recover_auth_finalization_locked(&config, &vault, account)?;
        let secrets = VaultSecrets::read_only(Arc::clone(&vault));
        let tdlib_config = config.tdlib_config(account, &secrets)?;
        let (client, updates) = runtime.create_client().map_err(td_to_drive_error)?;
        let hydrator = Hydrator::shared(&config.data_dir)?;
        let hydration_registration =
            hydrator.register_source(&config.data_dir, account_id, client.clone())?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let content_demand = Arc::new(Mutex::new(ContentDemandState::default()));
        // Fail closed until the native host supplies real path, power, and
        // volume conditions. Metadata synchronization may proceed, but no
        // eager byte request is admitted under this initial snapshot.
        let archive_conditions = Arc::new(RwLock::new(HostConditions {
            network: NetworkState::Offline,
            power: PowerState::Saving,
            disk: DiskState::Low,
        }));
        let worker_cancelled = Arc::clone(&cancelled);
        let worker_content_demand = Arc::clone(&content_demand);
        let worker_archive_conditions = Arc::clone(&archive_conditions);
        let worker_client = client.clone();
        let worker_hydrator = Arc::clone(&hydrator);
        let worker = std::thread::Builder::new()
            .name("gramdrive-namespace".to_owned())
            .spawn(move || {
                listener.on_progress(NamespaceProgress::Preparing);
                let outcome = run_session(
                    &config,
                    account,
                    tdlib_config,
                    SessionIo {
                        client: &worker_client,
                        updates: &updates,
                        cancelled: &worker_cancelled,
                    },
                    &worker_content_demand,
                    &worker_archive_conditions,
                    &worker_hydrator,
                    &listener,
                );
                drop(worker_client.close());
                drop(guard);
                match outcome {
                    Ok(()) => listener.on_progress(NamespaceProgress::Stopped),
                    Err(failure) if worker_cancelled.load(Ordering::Acquire) => {
                        let _ = failure;
                        listener.on_progress(NamespaceProgress::Stopped);
                    }
                    Err(failure) => listener.on_progress(NamespaceProgress::Failed {
                        category: failure.category.to_owned(),
                        retryable: failure.retryable,
                    }),
                }
            });
        let worker = match worker {
            Ok(worker) => worker,
            Err(error) => {
                hydrator.unregister_source(account_id, hydration_registration);
                drop(client.close());
                return Err(DriveError::Internal {
                    detail: format!("could not spawn namespace worker: {error}"),
                });
            }
        };
        Ok(Arc::new(Self {
            cancelled,
            content_demand,
            archive_conditions,
            client,
            hydrator,
            hydration_account_id: account_id,
            hydration_registration,
            worker: Mutex::new(Some(worker)),
        }))
    }

    /// Updates one chat's scheduling priority. This method only writes the
    /// agent-owned demand queue; it performs no TDLib or state I/O and is safe
    /// for a bounded IPC callback to invoke.
    pub fn set_chat_history_priority(
        &self,
        chat_id: i64,
        priority: ChatHistoryPriority,
    ) -> Result<(), DriveError> {
        if chat_id == 0 {
            return Err(DriveError::InvalidArgument {
                detail: "chat_id must be a non-zero Telegram identity".to_owned(),
            });
        }
        self.content_demand
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .set(chat_id, priority);
        Ok(())
    }

    /// Updates the real native host conditions used by the next bounded
    /// Archive-media scheduler tick.
    pub fn set_archive_host_conditions(&self, conditions: ArchiveHostConditions) {
        *self
            .archive_conditions
            .write()
            .unwrap_or_else(|error| error.into_inner()) = conditions.into();
    }

    /// Stops the client and waits for the worker to release account ownership.
    pub fn shutdown(&self) {
        self.hydrator
            .unregister_source(self.hydration_account_id, self.hydration_registration);
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            drop(self.client.close());
        }
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(worker) = worker {
            drop(worker.join());
        }
    }
}

impl Drop for NamespaceSession {
    fn drop(&mut self) {
        self.hydrator
            .unregister_source(self.hydration_account_id, self.hydration_registration);
        self.cancelled.store(true, Ordering::Release);
        drop(self.client.close());
        if let Some(worker) = self
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            drop(worker.join());
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SessionFailure {
    category: &'static str,
    retryable: bool,
}

impl SessionFailure {
    const AUTH: Self = Self {
        category: "auth-required",
        retryable: false,
    };
    const SOURCE: Self = Self {
        category: "source",
        retryable: true,
    };
    const FOLDER_CATALOG: Self = Self {
        category: "folder-catalog",
        retryable: true,
    };
    const SNAPSHOT_LOAD: Self = Self {
        category: "snapshot-load",
        retryable: true,
    };
    const SNAPSHOT_LIST: Self = Self {
        category: "snapshot-list",
        retryable: true,
    };
    const SNAPSHOT_CHAT: Self = Self {
        category: "snapshot-chat",
        retryable: true,
    };
    const STORAGE: Self = Self {
        category: "storage",
        retryable: true,
    };
    const RENDER: Self = Self {
        category: "render",
        retryable: true,
    };
    const RATE_LIMITED: Self = Self {
        category: "rate-limited",
        retryable: true,
    };

    fn storage_stage(self, category: &'static str) -> Self {
        if self.category == Self::STORAGE.category {
            Self {
                category,
                retryable: self.retryable,
            }
        } else {
            self
        }
    }
}

fn projection_node_upsert_failure(error: StateError) -> SessionFailure {
    let category = match &error {
        StateError::Sqlite(error) => {
            let diagnostic = error.to_string();
            if diagnostic.contains("items.parent_item_id, items.safe_name") {
                "projection-sibling-name-conflict"
            } else if diagnostic.contains(
                "items.canonical_item_id, items.view_kind, COALESCE(items.view_folder_id, 0)",
            ) || diagnostic
                .contains("items.canonical_item_id, items.view_kind, items.view_folder_id")
                || diagnostic.contains("items_appearance")
            {
                "projection-appearance-conflict"
            } else if diagnostic.contains("UNIQUE constraint failed") {
                "projection-unique-conflict"
            } else {
                "projection-node-sqlite-storage"
            }
        }
        StateError::InvalidArgument { .. } => "projection-node-invalid",
        StateError::RowNotFound { .. } => "projection-node-missing",
        _ => "projection-node-upsert-storage",
    };
    SessionFailure {
        category,
        retryable: true,
    }
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    config: &AuthSessionConfig,
    account: AccountId,
    tdlib_config: gramdrive_source_tdjson::TdlibConfig,
    io: SessionIo<'_>,
    content_demand: &Mutex<ContentDemandState>,
    archive_conditions: &RwLock<HostConditions>,
    hydrator: &Arc<Hydrator>,
    listener: &Arc<dyn NamespaceProgressListener>,
) -> Result<(), SessionFailure> {
    let mut auth = AuthMachine::new(tdlib_config);
    let mut folders = FolderCatalogMachine::new();
    let mut metadata = UpdateMachine::new();
    // Open the durable scope before waiting on TDLib readiness. Relaunch
    // updates for already-known chats can then be tracked and committed from
    // the first update instead of entering the unresolved readiness buffer.
    let layout =
        shared_state_layout(config.data_dir.clone()).map_err(|_| SessionFailure::STORAGE)?;
    let mut store = StateStore::open(&layout.database_file).map_err(|_| SessionFailure::STORAGE)?;
    let scope = {
        let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
        txn.account(gramdrive_model::identity::AccountKey {
            account_id: account,
        })
        .map_err(|_| SessionFailure::STORAGE)?
        .filter(|record| record.auth_state == "authorized")
        .map(|record| record.scope())
        .ok_or(SessionFailure::AUTH)?
    };
    let mut content = ContentCoordinator::new(&mut store, scope)?;
    drop(
        io.client
            .request(json!({"@type": "getOption", "name": "version"})),
    );

    wait_until_authorized(
        &mut auth,
        &mut folders,
        &mut metadata,
        &mut content.live,
        &mut content.stories,
        &mut store,
        scope,
        io.client,
        io.updates,
        io.cancelled,
    )?;
    listener.on_progress(NamespaceProgress::Authorized);
    let pending = io
        .client
        .request(json!({"@type": "getMe"}))
        .map_err(|_| SessionFailure::SOURCE)?;
    let me = wait_for_content_response(
        &mut store,
        scope,
        pending,
        &mut folders,
        &mut metadata,
        &mut content.live,
        &mut content.stories,
        io.updates,
        io.cancelled,
    )?
    .map_err(|_| SessionFailure::SOURCE)?;
    let (current_user_id, account_kind) =
        normalize_story_account(&me).map_err(|_| SessionFailure::SOURCE)?;
    content
        .stories
        .set_account_identity(current_user_id, account_kind)
        .map_err(|_| SessionFailure::SOURCE)?;
    {
        let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
        txn.start_story_list_pass(scope, now_ms())
            .map_err(|_| SessionFailure::STORAGE)?;
        txn.commit().map_err(|_| SessionFailure::STORAGE)?;
    }
    content
        .stories
        .start_active_list_discovery()
        .map_err(|_| SessionFailure::SOURCE)?;
    wait_for_folder_catalog(
        &mut folders,
        &mut metadata,
        &mut content.live,
        &mut content.stories,
        &mut store,
        scope,
        io.updates,
        io.cancelled,
    )
    .map_err(|failure| failure.storage_stage("folder-catalog-storage"))?;
    listener.on_progress(NamespaceProgress::FolderCatalog);

    // The source catalog is durable before its bounded membership snapshot.
    // Provider projection waits for the snapshot's atomic ready boundary.
    persist_folders(&mut store, scope, &folders)
        .map_err(|failure| failure.storage_stage("folder-catalog-storage"))?;
    // A crash may have committed story retention before its provider/cache
    // cleanup. Reapplying canonical policy after the relaunch catalog update
    // converges that window without fetching or opening any story.
    purge_disallowed_materializations(hydrator, scope.account)
        .map_err(|failure| failure.storage_stage("cache-policy-storage"))?;
    let _ = folders.take_batch();
    listener.on_progress(NamespaceProgress::SnapshotList);
    run_snapshot_cycle(
        &mut store,
        scope,
        &mut folders,
        &mut metadata,
        &mut content,
        io.client,
        io.updates,
        io.cancelled,
        listener,
        hydrator,
    )
    .map_err(|failure| failure.storage_stage("snapshot-storage"))?;
    purge_disallowed_materializations(hydrator, scope.account)
        .map_err(|failure| failure.storage_stage("cache-policy-storage"))?;
    render_pending_months(&mut store, Path::new(&layout.cache_dir), now_ms())?;
    initialize_content_progress(&mut store, scope)
        .map_err(|failure| failure.storage_stage("content-progress-storage"))?;
    {
        let txn = store.write_txn().map_err(|_| SessionFailure {
            category: "story-progress-storage",
            retryable: true,
        })?;
        txn.restart_ready_story_scans(scope, now_ms())
            .map_err(|_| SessionFailure {
                category: "story-progress-storage",
                retryable: true,
            })?;
        txn.commit().map_err(|_| SessionFailure {
            category: "story-progress-storage",
            retryable: true,
        })?;
    }
    listener.on_progress(
        namespace_counts(&mut store, scope)
            .map_err(|failure| failure.storage_stage("namespace-count-storage"))?,
    );

    let outcome = run_content_loop(
        &mut store,
        scope,
        &mut folders,
        &mut metadata,
        &mut content,
        Path::new(&layout.cache_dir),
        content_demand,
        archive_conditions,
        hydrator,
        io.client,
        io.updates,
        io.cancelled,
        listener,
    );
    if io.cancelled.load(Ordering::Acquire) {
        content.mark_cancelled(&mut store, scope)?;
        Ok(())
    } else {
        outcome
    }
}

struct ContentCoordinator {
    scheduler: BackfillScheduler,
    live: LiveMachine,
    stories: StoryMachine,
    crawl: Option<CrawlMachine>,
    active_chat: Option<i64>,
    history_projection_pending: bool,
    live_request_chat: Option<i64>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct HistoryDriveOutcome {
    /// Source/cursor/progress state committed this tick.
    durable_changed: bool,
    /// The chat projection was reconciled at a true slice boundary.
    provider_changed: bool,
}

impl ContentCoordinator {
    fn new(
        store: &mut StateStore,
        scope: gramdrive_model::identity::AccountScope,
    ) -> Result<Self, SessionFailure> {
        let chats = {
            let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
            let mut planned = Vec::new();
            for chat in txn.chats(scope).map_err(|_| SessionFailure::STORAGE)? {
                if chat.is_protected || chat.deleted_at_ms.is_some() {
                    continue;
                }
                let newest_message_id = txn
                    .chat_sync_state(&chat.key)
                    .map_err(|_| SessionFailure::STORAGE)?
                    .and_then(|sync| sync.window)
                    .map(|window| window.newest.0);
                planned.push(LiveChat {
                    chat_id: chat.key.chat_id.0,
                    newest_message_id,
                });
            }
            planned
        };
        Ok(Self {
            scheduler: BackfillScheduler::with_defaults(),
            live: LiveMachine::new(LivePlan::new(chats)).map_err(|_| SessionFailure::STORAGE)?,
            stories: StoryMachine::new(scope.account.account_id.0, StoryAccountKind::Unsupported)
                .map_err(|_| SessionFailure::STORAGE)?,
            crawl: None,
            active_chat: None,
            history_projection_pending: false,
            live_request_chat: None,
        })
    }

    fn mark_cancelled(
        &mut self,
        store: &mut StateStore,
        scope: gramdrive_model::identity::AccountScope,
    ) -> Result<(), SessionFailure> {
        if let Some(chat_id) = self.active_chat.take() {
            if self.history_projection_pending {
                rebuild_chat_projection(
                    store,
                    ChatKey {
                        scope,
                        chat_id: ChatId(chat_id),
                    },
                )?;
                self.history_projection_pending = false;
            }
            put_content_progress(
                store,
                ChatKey {
                    scope,
                    chat_id: ChatId(chat_id),
                },
                content_progress(ChatContentPhase::Cancelled, None, false, 0, None),
            )?;
        }
        self.crawl = None;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn run_content_loop(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    folders: &mut FolderCatalogMachine,
    metadata: &mut UpdateMachine,
    content: &mut ContentCoordinator,
    cache_root: &Path,
    demand: &Mutex<ContentDemandState>,
    archive_conditions: &RwLock<HostConditions>,
    hydrator: &Arc<Hydrator>,
    client: &TdClient,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
    listener: &Arc<dyn NamespaceProgressListener>,
) -> Result<(), SessionFailure> {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let projected = converge_projection_slice(store, scope)?;
        if projected > 0 {
            listener.on_progress(NamespaceProgress::ProjectionSlice {
                processed_chat_count: projected as u64,
            });
        }
        let live_changed = drive_live_steps(
            store, scope, folders, metadata, content, client, updates, cancelled, listener,
        )?;
        let history = drive_history_page(
            store, scope, folders, metadata, content, demand, client, updates, cancelled,
        )?;
        let story_changed = drive_story_steps(
            store, scope, folders, metadata, content, client, updates, cancelled,
        )?;
        if story_changed || live_changed || history.durable_changed {
            purge_disallowed_materializations(hydrator, scope.account)?;
        }
        let rendered_changed = render_pending_months(store, cache_root, now_ms())?;
        let conditions = *archive_conditions
            .read()
            .unwrap_or_else(|error| error.into_inner());
        hydrator
            .schedule_archive_backfill(content.scheduler, scope, conditions)
            .map_err(|_| SessionFailure::STORAGE)?;
        if story_changed || live_changed || history.provider_changed || rendered_changed {
            listener.on_progress(namespace_counts(store, scope)?);
        }

        let received_update = match updates.recv_timeout(UPDATE_POLL) {
            Ok(update) => {
                folders.on_update(&update);
                metadata.on_update(&update);
                route_content_live_update(store, scope, &mut content.live, &update)?;
                content.stories.on_update(&update);
                // TDLib commonly publishes a burst of chat/title/position
                // updates around authorization and history responses. Fold a
                // bounded arrival window before the projection transaction:
                // ordering is unchanged, provider latency remains bounded,
                // and a sustained replay cannot cause one account-wide
                // reconciliation and provider signal per individual update.
                let checkpoint_deadline = std::time::Instant::now() + UPDATE_CHECKPOINT_WINDOW;
                loop {
                    let Some(remaining) =
                        checkpoint_deadline.checked_duration_since(std::time::Instant::now())
                    else {
                        break;
                    };
                    match updates.recv_timeout(remaining) {
                        Ok(update) => {
                            folders.on_update(&update);
                            metadata.on_update(&update);
                            route_content_live_update(store, scope, &mut content.live, &update)?;
                            content.stories.on_update(&update);
                        }
                        Err(UpdateRecvError::Timeout | UpdateRecvError::Closed) => break,
                    }
                }
                if folders.has_pending() {
                    // Folder definitions are complete-state. Persist the new
                    // catalog, then snapshot only because a new folder list
                    // may introduce chats absent from main/archive.
                    persist_folders(store, scope, folders)?;
                    let _ = folders.take_batch();
                    run_snapshot_cycle(
                        store, scope, folders, metadata, content, client, updates, cancelled,
                        listener, hydrator,
                    )?;
                    purge_disallowed_materializations(hydrator, scope.account)?;
                }
                let metadata_changed = apply_live_batch(
                    store,
                    scope,
                    folders,
                    metadata,
                    Some(&mut content.live),
                    Some(&mut content.stories),
                    SessionIo {
                        client,
                        updates,
                        cancelled,
                    },
                )?;
                if metadata_changed {
                    purge_disallowed_materializations(hydrator, scope.account)?;
                }
                true
            }
            Err(UpdateRecvError::Timeout) => false,
            Err(UpdateRecvError::Closed) => {
                return if cancelled.load(Ordering::Acquire) {
                    Ok(())
                } else {
                    Err(SessionFailure::SOURCE)
                };
            }
        };
        let delay = content_loop_delay(received_update);
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
}

fn converge_projection_slice(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
) -> Result<usize, SessionFailure> {
    let (generation, after, chats) = {
        let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
        let Some(readiness) = txn
            .namespace_readiness(scope)
            .map_err(|_| SessionFailure::STORAGE)?
        else {
            return Ok(0);
        };
        if readiness.convergence_complete {
            return Ok(0);
        }
        let chats = txn
            .listed_chats(scope)
            .map_err(|_| SessionFailure::STORAGE)?
            .into_iter()
            .filter(|chat| {
                readiness
                    .projection_after_chat_id
                    .is_none_or(|after| chat.key.chat_id.0 > after.0)
            })
            .take(MAX_PROJECTION_CHATS_PER_SLICE)
            .map(|chat| chat.key)
            .collect::<Vec<_>>();
        (
            readiness.generation,
            readiness.projection_after_chat_id,
            chats,
        )
    };
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    if chats.is_empty() {
        txn.advance_namespace_projection(scope, generation, after, true, now_ms())
            .map_err(|_| SessionFailure::STORAGE)?;
        txn.commit().map_err(|_| SessionFailure::STORAGE)?;
        return Ok(0);
    }
    for chat in &chats {
        reconcile_chat_projection_txn(&txn, *chat)?;
    }
    let cursor = chats.last().map(|chat| chat.chat_id);
    txn.advance_namespace_projection(scope, generation, cursor, false, now_ms())
        .map_err(|_| SessionFailure::STORAGE)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)?;
    Ok(chats.len())
}

fn content_loop_delay(received_update: bool) -> Duration {
    if received_update {
        CONTENT_LOOP_BUSY_YIELD
    } else {
        Duration::ZERO
    }
}

fn initialize_content_progress(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
) -> Result<(), SessionFailure> {
    let pending = {
        let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
        let mut pending = Vec::new();
        for chat in txn
            .listed_chats(scope)
            .map_err(|_| SessionFailure::STORAGE)?
        {
            let existing = txn
                .chat_content_progress(&chat.key)
                .map_err(|_| SessionFailure::STORAGE)?;
            let phase = if chat.is_protected {
                ChatContentPhase::Protected
            } else if txn
                .chat_sync_state(&chat.key)
                .map_err(|_| SessionFailure::STORAGE)?
                .is_some_and(|sync| sync.history_complete)
            {
                ChatContentPhase::Ready
            } else {
                ChatContentPhase::Pending
            };
            let needs_write = match existing {
                None => true,
                Some(progress) if chat.is_protected => {
                    progress.phase != ChatContentPhase::Protected
                }
                Some(progress) if progress.phase == ChatContentPhase::Protected => true,
                Some(_) => false,
            };
            if !needs_write {
                continue;
            }
            pending.push((chat.key, phase));
        }
        pending
    };
    if pending.is_empty() {
        return Ok(());
    }
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    for (chat, phase) in pending {
        let category = (phase == ChatContentPhase::Protected).then_some("protected-content");
        txn.put_chat_content_progress(&chat, &content_progress(phase, category, false, 0, None))
            .map_err(|_| SessionFailure::STORAGE)?;
    }
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

#[allow(clippy::too_many_arguments)]
fn drive_history_page(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    folders: &mut FolderCatalogMachine,
    metadata: &mut UpdateMachine,
    content: &mut ContentCoordinator,
    demand: &Mutex<ContentDemandState>,
    client: &TdClient,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
) -> Result<HistoryDriveOutcome, SessionFailure> {
    if content.crawl.is_none() {
        let Some((chat_id, priority)) =
            open_next_history_turn(store, scope, &content.scheduler, demand, now_ms())?
        else {
            return Ok(HistoryDriveOutcome::default());
        };
        let crawl = crawl_for_chat(store, scope, chat_id, priority)?;
        content.active_chat = Some(chat_id.0);
        content.crawl = Some(crawl);
        content.history_projection_pending = false;
    }

    let mut outcome = HistoryDriveOutcome::default();
    loop {
        if cancelled.load(Ordering::Acquire) {
            outcome.provider_changed |= publish_pending_history_projection(store, scope, content)?;
            return Ok(outcome);
        }
        let step = match content
            .crawl
            .as_mut()
            .ok_or(SessionFailure::STORAGE)?
            .next_step()
        {
            Ok(step) => step,
            Err(_) => {
                outcome.provider_changed |=
                    publish_pending_history_projection(store, scope, content)?;
                let chat_id = content.active_chat.take().ok_or(SessionFailure::STORAGE)?;
                content.crawl = None;
                put_content_progress(
                    store,
                    ChatKey {
                        scope,
                        chat_id: ChatId(chat_id),
                    },
                    content_progress(
                        ChatContentPhase::Failed,
                        Some("history-machine"),
                        true,
                        1,
                        None,
                    ),
                )?;
                outcome.durable_changed = true;
                demand
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(chat_id);
                return Ok(outcome);
            }
        };
        match step {
            CrawlStep::Submit(request) => {
                if content
                    .scheduler
                    .observe(store, scope, now_ms())
                    .map_err(|_| SessionFailure::STORAGE)?
                    .pending_until_ms
                    .is_some()
                {
                    // Request spacing is not a scheduling/publication
                    // boundary. Keep the durable cursor and accumulated dirty
                    // months, service live/story work during the wait, and
                    // publish once the bounded history quantum actually ends.
                    return Ok(outcome);
                }
                let pending = client
                    .request(request)
                    .map_err(|_| SessionFailure::SOURCE)?;
                content
                    .scheduler
                    .note_dispatch(store, scope, now_ms())
                    .map_err(|_| SessionFailure::STORAGE)?;
                let outcome = wait_for_content_response(
                    store,
                    scope,
                    pending,
                    folders,
                    metadata,
                    &mut content.live,
                    &mut content.stories,
                    updates,
                    cancelled,
                )?;
                content
                    .crawl
                    .as_mut()
                    .ok_or(SessionFailure::STORAGE)?
                    .on_response(outcome)
                    .map_err(|_| SessionFailure::SOURCE)?;
            }
            CrawlStep::Backoff(backoff) => {
                let retry_after_ms = backoff
                    .retry_after_secs
                    .and_then(|seconds| i64::try_from(seconds.saturating_mul(1_000)).ok());
                let flood = content
                    .scheduler
                    .note_flood_wait(store, scope, retry_after_ms, backoff.attempt, now_ms())
                    .map_err(|_| SessionFailure::STORAGE)?;
                outcome.durable_changed = true;
                if flood.exhausted || backoff.attempt > MAX_RETRY_ATTEMPTS {
                    outcome.provider_changed |=
                        publish_pending_history_projection(store, scope, content)?;
                    let chat_id = content.active_chat.take().ok_or(SessionFailure::STORAGE)?;
                    content.crawl = None;
                    put_content_progress(
                        store,
                        ChatKey {
                            scope,
                            chat_id: ChatId(chat_id),
                        },
                        content_progress(
                            ChatContentPhase::Failed,
                            Some("rate-limited"),
                            true,
                            backoff.attempt,
                            Some(flood.until_ms),
                        ),
                    )?;
                    demand
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .remove(chat_id);
                    return Ok(outcome);
                }
                outcome.provider_changed |=
                    publish_pending_history_projection(store, scope, content)?;
                return Ok(outcome);
            }
            CrawlStep::Commit(commit) => {
                let chat_id = commit.chat_id;
                let complete = commit.history_complete;
                let keep_slice = !complete
                    && content
                        .crawl
                        .as_mut()
                        .is_some_and(|crawl| should_continue_history_slice(crawl, chat_id, demand));
                apply_history_commit_with_publication(
                    store,
                    scope,
                    &commit,
                    now_ms(),
                    !keep_slice,
                )?;
                outcome.durable_changed = true;
                content.history_projection_pending = keep_slice;
                if complete {
                    demand
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .remove(chat_id);
                }
                if !keep_slice {
                    outcome.provider_changed = true;
                    content.crawl = None;
                    content.active_chat = None;
                    return Ok(outcome);
                }
            }
            CrawlStep::Unavailable(unavailable) => {
                let chat_id = unavailable.chat_id;
                outcome.provider_changed |=
                    publish_pending_history_projection(store, scope, content)?;
                put_content_progress(
                    store,
                    ChatKey {
                        scope,
                        chat_id: ChatId(chat_id),
                    },
                    content_progress(
                        ChatContentPhase::Unavailable,
                        Some("history-unavailable"),
                        true,
                        1,
                        None,
                    ),
                )?;
                outcome.durable_changed = true;
                content.crawl = None;
                content.active_chat = None;
                demand
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove(chat_id);
                return Ok(outcome);
            }
            CrawlStep::Done => {
                outcome.provider_changed |=
                    publish_pending_history_projection(store, scope, content)?;
                content.crawl = None;
                content.active_chat = None;
                return Ok(outcome);
            }
        }
    }
}

/// Publishes history accumulated across request-spacing ticks at a real
/// scheduling boundary. The cursor and message rows were already committed;
/// this reconciles only the affected chat and leaves media dataless.
fn publish_pending_history_projection(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    content: &mut ContentCoordinator,
) -> Result<bool, SessionFailure> {
    if !content.history_projection_pending {
        return Ok(false);
    }
    let chat_id = content.active_chat.ok_or(SessionFailure::STORAGE)?;
    rebuild_chat_projection(
        store,
        ChatKey {
            scope,
            chat_id: ChatId(chat_id),
        },
    )?;
    content.history_projection_pending = false;
    Ok(true)
}

/// Whether the active one-chat crawl keeps the next page without returning to
/// the account-wide scheduler. Cursor authority remains entirely durable in
/// `chat_sync_state`; this is only a bounded in-process fairness quantum.
///
/// Preemption reads the host's *live* view, not the admission ledger: a chat
/// with an unspent admission is owed a turn, but it is not on screen, so it must
/// not cut every background slice down to one page while it waits.
///
/// The active chat's own weight, by contrast, never falls below the weight its
/// turn was granted at. The crawl carries that weight from `crawl_for_chat`, so
/// a chat admitted as visible and released again before the scheduler snapshot
/// still gets a foreground-sized turn rather than a silently background one.
fn should_continue_history_slice(
    crawl: &mut CrawlMachine,
    active_chat_id: i64,
    demand: &Mutex<ContentDemandState>,
) -> bool {
    let (visible, requested) = demand
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .live_snapshot();
    let active = ChatId(active_chat_id);
    let live_priority = if visible.contains(&active) {
        CrawlPriority::Visible
    } else if requested.contains(&active) {
        CrawlPriority::Requested
    } else {
        CrawlPriority::Background
    };
    let progress = crawl
        .progress()
        .into_iter()
        .find(|progress| progress.chat_id == active_chat_id);
    let granted_priority = progress
        .as_ref()
        .map_or(CrawlPriority::Background, |progress| progress.priority);
    let active_priority = live_priority.max(granted_priority);
    let _ = crawl.set_priority(active_chat_id, active_priority);
    let pages_served = progress.map_or(0, |progress| progress.pages_served);

    match active_priority {
        // Foreground crawls get a larger quantum, then return to publication
        // and live/story scheduling before the scheduler picks them again.
        CrawlPriority::Visible => pages_served < MAX_FOREGROUND_HISTORY_PAGES_PER_SLICE,
        // A newly visible different chat preempts requested work at this page
        // boundary. Requested work otherwise uses the same bounded foreground
        // quantum so one deep chat cannot monopolize projection publication.
        CrawlPriority::Requested => {
            visible.is_empty() && pages_served < MAX_FOREGROUND_HISTORY_PAGES_PER_SLICE
        }
        // Background work yields immediately to any foreground demand and
        // otherwise after a bounded number of committed pages.
        CrawlPriority::Background => {
            visible.is_empty()
                && requested.is_empty()
                && pages_served < MAX_BACKGROUND_HISTORY_PAGES_PER_SLICE
        }
    }
}

fn crawl_for_chat(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    chat_id: ChatId,
    priority: BackfillPriority,
) -> Result<CrawlMachine, SessionFailure> {
    let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
    let key = ChatKey { scope, chat_id };
    let sync = txn
        .chat_sync_state(&key)
        .map_err(|_| SessionFailure::STORAGE)?;
    let recovery_pending = full_live_recovery_pending(
        txn.chat_content_progress(&key)
            .map_err(|_| SessionFailure::STORAGE)?
            .as_ref(),
    );
    let chat = match (sync, recovery_pending) {
        (_, true) => ChatCrawl {
            chat_id: chat_id.0,
            window: None,
            history_complete: false,
            priority: crawl_priority(priority),
        },
        (None, false) => ChatCrawl {
            chat_id: chat_id.0,
            window: None,
            history_complete: false,
            priority: crawl_priority(priority),
        },
        (Some(sync), false) => ChatCrawl {
            chat_id: chat_id.0,
            window: sync.window.map(|window| CrawlWindow {
                oldest_message_id: window.oldest.0,
                newest_message_id: window.newest.0,
            }),
            history_complete: sync.history_complete,
            priority: crawl_priority(priority),
        },
    };
    CrawlMachine::new(CrawlPlan::new(vec![chat])).map_err(|_| SessionFailure::STORAGE)
}

fn crawl_priority(priority: BackfillPriority) -> CrawlPriority {
    match priority {
        BackfillPriority::Background => CrawlPriority::Background,
        BackfillPriority::Requested => CrawlPriority::Requested,
        BackfillPriority::Visible => CrawlPriority::Visible,
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_story_steps(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    folders: &mut FolderCatalogMachine,
    metadata: &mut UpdateMachine,
    content: &mut ContentCoordinator,
    client: &TdClient,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
) -> Result<bool, SessionFailure> {
    let mut changed = false;
    for _ in 0..MAX_STORY_STEPS_PER_TICK {
        if cancelled.load(Ordering::Acquire) {
            return Ok(changed);
        }
        if !content.stories.has_active_chat() && !content.stories.has_active_list_work() {
            let candidate = {
                let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
                txn.story_sync_worklist(scope, 1)
                    .map_err(|_| SessionFailure::STORAGE)?
                    .into_iter()
                    .next()
            };
            if let Some(chat_key) = candidate {
                let (chat, mut progress) = {
                    let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
                    let chat = txn
                        .chat(&chat_key)
                        .map_err(|_| SessionFailure::STORAGE)?
                        .ok_or(SessionFailure::STORAGE)?;
                    let progress = txn
                        .story_sync_progress(&chat_key)
                        .map_err(|_| SessionFailure::STORAGE)?
                        .ok_or(SessionFailure::STORAGE)?;
                    (chat, progress)
                };
                if chat.is_protected {
                    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
                    txn.protect_chat_stories(&chat_key, now_ms())
                        .map_err(|_| SessionFailure::STORAGE)?;
                    txn.commit().map_err(|_| SessionFailure::STORAGE)?;
                    rebuild_chat_projection(store, chat_key)?;
                    changed = true;
                    continue;
                }
                progress.phase = StorySyncPhase::Syncing;
                progress.failure_category = None;
                progress.retryable = false;
                progress.updated_at_ms = now_ms();
                put_story_progress(store, chat_key, &progress)?;
                content
                    .stories
                    .enqueue_chat(StoryChatPlan {
                        chat_id: chat_key.chat_id.0,
                        chat_kind: source_story_chat_kind(chat.chat_type),
                        cursor: source_story_cursor(&progress),
                    })
                    .map_err(|_| SessionFailure::SOURCE)?;
            }
        }

        match content
            .stories
            .next_step()
            .map_err(|_| SessionFailure::SOURCE)?
        {
            StoryStep::Submit(request) => {
                if !background_story_request_allowed(&request) {
                    return Err(SessionFailure::SOURCE);
                }
                if content
                    .scheduler
                    .observe(store, scope, now_ms())
                    .map_err(|_| SessionFailure::STORAGE)?
                    .pending_until_ms
                    .is_some()
                {
                    return Ok(changed);
                }
                let pending = client
                    .request(request)
                    .map_err(|_| SessionFailure::SOURCE)?;
                content
                    .scheduler
                    .note_dispatch(store, scope, now_ms())
                    .map_err(|_| SessionFailure::STORAGE)?;
                let response = wait_for_story_response(
                    store, scope, pending, folders, metadata, content, client, updates, cancelled,
                )?;
                if response.is_err() {
                    if let Some(chat_id) = content.stories.abandon_active_chat() {
                        mark_story_scan_failed(store, scope, chat_id, "story-source", true)?;
                        changed = true;
                        continue;
                    }
                    return Err(SessionFailure::SOURCE);
                }
            }
            StoryStep::Backoff(backoff) => {
                let retry_after_ms = backoff
                    .retry_after_secs
                    .and_then(|seconds| i64::try_from(seconds.saturating_mul(1_000)).ok());
                let flood = content
                    .scheduler
                    .note_flood_wait(store, scope, retry_after_ms, backoff.attempt, now_ms())
                    .map_err(|_| SessionFailure::STORAGE)?;
                if flood.exhausted || backoff.attempt > MAX_RETRY_ATTEMPTS {
                    if let Some(chat_id) = content.stories.abandon_active_chat() {
                        mark_story_scan_failed(store, scope, chat_id, "rate-limited", true)?;
                        return Ok(true);
                    }
                    return Err(SessionFailure::RATE_LIMITED);
                }
                return Ok(changed);
            }
            StoryStep::Commit(commit) => {
                if let Some(chat) = apply_story_commit(store, scope, &commit, now_ms())? {
                    // Story commits are chat-local. Publish only that chat's
                    // direct active/month branches; progress-only commits do
                    // not touch provider state at all.
                    rebuild_chat_projection(store, chat)?;
                    changed = true;
                }
            }
            StoryStep::ResyncRequired(chat_ids) => {
                let mut chat_ids: BTreeSet<_> = chat_ids.into_iter().collect();
                if let Some(active_chat_id) = content.stories.abandon_active_chat() {
                    chat_ids.insert(active_chat_id);
                }
                for chat_id in chat_ids {
                    require_story_resync(store, scope, chat_id)?;
                }
                changed = true;
            }
            StoryStep::Idle => return Ok(changed),
        }
    }
    Ok(changed)
}

#[allow(clippy::too_many_arguments)]
fn wait_for_story_response(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    pending: gramdrive_source_tdjson::PendingRequest,
    folders: &mut FolderCatalogMachine,
    metadata: &mut UpdateMachine,
    content: &mut ContentCoordinator,
    client: &TdClient,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
) -> Result<Result<(), StoryError>, SessionFailure> {
    let outcome = wait_for_content_response(
        store,
        scope,
        pending,
        folders,
        metadata,
        &mut content.live,
        &mut content.stories,
        updates,
        cancelled,
    )?;

    // Responses and updates use separate runtime queues. Once the response is
    // observable, all earlier TDLib events have been dispatched, but the
    // consumer may not yet have folded the final queued updates. Drain that
    // ordered prefix before checkpointing metadata or exposing story commits.
    while let Ok(update) = updates.try_recv() {
        folders.on_update(&update);
        metadata.on_update(&update);
        route_content_live_update(store, scope, &mut content.live, &update)?;
        content.stories.on_update(&update);
    }

    // TDLib orders updateNewChat before updates about that chat, but waiting
    // for a response buffers both reducers in memory. Persist the metadata
    // checkpoint before on_response makes any dependent story commit visible.
    // This is especially important for loadActiveStories, which intentionally
    // discovers chats outside the ordinary snapshot/worklist.
    apply_live_batch(
        store,
        scope,
        folders,
        metadata,
        Some(&mut content.live),
        Some(&mut content.stories),
        SessionIo {
            client,
            updates,
            cancelled,
        },
    )?;
    Ok(content.stories.on_response(outcome))
}

fn source_story_chat_kind(chat_type: ChatType) -> StoryChatKind {
    match chat_type {
        ChatType::Private => StoryChatKind::Private,
        ChatType::Group => StoryChatKind::Group,
        ChatType::Supergroup => StoryChatKind::Supergroup,
        ChatType::Channel => StoryChatKind::Channel,
    }
}

fn source_story_cursor(progress: &StorySyncProgressRecord) -> StoryScanCursor {
    StoryScanCursor {
        active_complete: progress.active_complete,
        profile_cursor: progress.profile_cursor,
        profile_scan_generation: progress.profile_scan_generation,
        profile_complete: progress.profile_complete,
        archive_capability: source_archive_capability(progress.archive_eligibility),
        archive_cursor: progress.archive_cursor,
        archive_complete: progress.archive_complete,
    }
}

fn source_archive_capability(capability: StoryArchiveEligibility) -> SourceStoryArchiveCapability {
    match capability {
        StoryArchiveEligibility::Unknown => SourceStoryArchiveCapability::Unknown,
        StoryArchiveEligibility::Owner => SourceStoryArchiveCapability::Owner,
        StoryArchiveEligibility::Manageable => SourceStoryArchiveCapability::Manageable,
        StoryArchiveEligibility::Ineligible => SourceStoryArchiveCapability::Ineligible,
        StoryArchiveEligibility::AccountUnsupported => {
            SourceStoryArchiveCapability::AccountUnsupported
        }
        StoryArchiveEligibility::RightsUnavailable => {
            SourceStoryArchiveCapability::RightsUnavailable
        }
    }
}

fn state_archive_eligibility(capability: SourceStoryArchiveCapability) -> StoryArchiveEligibility {
    match capability {
        SourceStoryArchiveCapability::Unknown => StoryArchiveEligibility::Unknown,
        SourceStoryArchiveCapability::Owner => StoryArchiveEligibility::Owner,
        SourceStoryArchiveCapability::Manageable => StoryArchiveEligibility::Manageable,
        SourceStoryArchiveCapability::Ineligible => StoryArchiveEligibility::Ineligible,
        SourceStoryArchiveCapability::AccountUnsupported => {
            StoryArchiveEligibility::AccountUnsupported
        }
        SourceStoryArchiveCapability::RightsUnavailable => {
            StoryArchiveEligibility::RightsUnavailable
        }
    }
}

fn put_story_progress(
    store: &mut StateStore,
    chat: ChatKey,
    progress: &StorySyncProgressRecord,
) -> Result<(), SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure {
        category: "snapshot-open-storage",
        retryable: true,
    })?;
    txn.put_story_sync_progress(&chat, progress)
        .map_err(|_| SessionFailure::STORAGE)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

fn mark_story_scan_failed(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    chat_id: i64,
    category: &str,
    retryable: bool,
) -> Result<(), SessionFailure> {
    let chat = ChatKey {
        scope,
        chat_id: ChatId(chat_id),
    };
    let mut progress = {
        let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
        txn.story_sync_progress(&chat)
            .map_err(|_| SessionFailure::STORAGE)?
            .ok_or(SessionFailure::STORAGE)?
    };
    progress.phase = StorySyncPhase::Failed;
    progress.failure_category = Some(category.to_owned());
    progress.retryable = retryable;
    progress.attempt_count = progress.attempt_count.saturating_add(1);
    progress.updated_at_ms = now_ms();
    put_story_progress(store, chat, &progress)
}

fn require_story_resync(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    chat_id: i64,
) -> Result<(), SessionFailure> {
    let chat = ChatKey {
        scope,
        chat_id: ChatId(chat_id),
    };
    let mut progress = {
        let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
        txn.story_sync_progress(&chat)
            .map_err(|_| SessionFailure::STORAGE)?
            .ok_or(SessionFailure::STORAGE)?
    };
    progress.phase = StorySyncPhase::Pending;
    progress.active_complete = false;
    progress.profile_cursor = None;
    progress.profile_scan_generation = progress.profile_scan_generation.saturating_add(1);
    progress.profile_complete = false;
    progress.archive_eligibility = StoryArchiveEligibility::Unknown;
    progress.archive_cursor = None;
    progress.archive_complete = false;
    progress.failure_category = None;
    progress.retryable = false;
    progress.updated_at_ms = now_ms();
    put_story_progress(store, chat, &progress)
}

fn apply_story_commit(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    commit: &StoryCommit,
    observed_at_ms: i64,
) -> Result<Option<ChatKey>, SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    let projected_chat = match commit {
        StoryCommit::ActiveListProgress { complete } => {
            txn.advance_story_list_progress(scope, *complete, observed_at_ms)
                .map_err(|_| SessionFailure::STORAGE)?;
            None
        }
        StoryCommit::ActiveSnapshot {
            chat_id,
            order: _,
            stories,
        } => {
            let chat = ChatKey {
                scope,
                chat_id: ChatId(*chat_id),
            };
            let chat_protected = txn
                .read()
                .chat(&chat)
                .map_err(|_| SessionFailure::STORAGE)?
                .ok_or(SessionFailure::STORAGE)?
                .is_protected;
            let records = stories
                .iter()
                .map(|story| {
                    let facts = state_story_facts(scope, story, chat_protected)?;
                    let locators = state_story_locators(facts.key, story, chat_protected)?;
                    // Active discovery remains byte-free, but full active
                    // story objects already contain the same allowed locator
                    // catalog used by profile stories. Persist it here so an
                    // explicit open can use the shared attachment transfer
                    // path without another discovery model or eager fetch.
                    txn.upsert_story_with_locators(&facts, &locators)
                        .map_err(|_| SessionFailure::STORAGE)?;
                    Ok((
                        facts,
                        story_appearance(
                            scope,
                            story,
                            StoryAppearanceLocation::Active,
                            None,
                            None,
                        )?,
                    ))
                })
                .collect::<Result<Vec<_>, SessionFailure>>()?;
            txn.replace_active_stories(&chat, &records)
                .map_err(|_| SessionFailure::STORAGE)?;
            let mut progress = txn
                .read()
                .story_sync_progress(&chat)
                .map_err(|_| SessionFailure::STORAGE)?
                .ok_or(SessionFailure::STORAGE)?;
            progress.active_complete = true;
            progress.pages_committed = progress.pages_committed.saturating_add(1);
            progress.stories_seen = progress
                .stories_seen
                .saturating_add(u64::try_from(stories.len()).unwrap_or(u64::MAX));
            progress.updated_at_ms = observed_at_ms;
            txn.put_story_sync_progress(&chat, &progress)
                .map_err(|_| SessionFailure::STORAGE)?;
            Some(chat)
        }
        StoryCommit::Upsert(story) => {
            apply_story_observation(&txn, scope, story, None, None, observed_at_ms)?;
            Some(ChatKey {
                scope,
                chat_id: ChatId(story.poster_chat_id),
            })
        }
        StoryCommit::ProfilePage {
            chat_id,
            generation,
            stories,
            pinned_story_ids,
            next_from_story_id,
            complete,
        } => {
            let chat = ChatKey {
                scope,
                chat_id: ChatId(*chat_id),
            };
            let mut progress = txn
                .read()
                .story_sync_progress(&chat)
                .map_err(|_| SessionFailure::STORAGE)?
                .ok_or(SessionFailure::STORAGE)?;
            let first_page = progress.profile_cursor.is_none();
            if first_page {
                txn.clear_profile_pin_order(&chat)
                    .map_err(|_| SessionFailure::STORAGE)?;
            }
            for story in stories {
                let pin_order = if first_page {
                    pinned_story_ids
                        .iter()
                        .position(|story_id| *story_id == story.story_id)
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|_| SessionFailure::STORAGE)?
                } else {
                    None
                };
                apply_story_observation(
                    &txn,
                    scope,
                    story,
                    Some(*generation),
                    pin_order,
                    observed_at_ms,
                )?;
            }
            progress.profile_cursor = *next_from_story_id;
            progress.profile_complete = *complete;
            progress.pages_committed = progress.pages_committed.saturating_add(1);
            progress.stories_seen = progress
                .stories_seen
                .saturating_add(u64::try_from(stories.len()).unwrap_or(u64::MAX));
            progress.updated_at_ms = observed_at_ms;
            if *complete {
                let retention = txn
                    .read()
                    .retention_mode(scope.account)
                    .map_err(|_| SessionFailure::STORAGE)?
                    .ok_or(SessionFailure::STORAGE)?;
                txn.finish_profile_scan(&chat, *generation, retention, observed_at_ms)
                    .map_err(|_| SessionFailure::STORAGE)?;
            }
            txn.put_story_sync_progress(&chat, &progress)
                .map_err(|_| SessionFailure::STORAGE)?;
            Some(chat)
        }
        StoryCommit::ArchiveCapability {
            chat_id,
            capability,
        } => {
            let chat = ChatKey {
                scope,
                chat_id: ChatId(*chat_id),
            };
            let mut progress = txn
                .read()
                .story_sync_progress(&chat)
                .map_err(|_| SessionFailure::STORAGE)?
                .ok_or(SessionFailure::STORAGE)?;
            progress.archive_eligibility = state_archive_eligibility(*capability);
            progress.archive_complete = matches!(
                capability,
                SourceStoryArchiveCapability::Ineligible
                    | SourceStoryArchiveCapability::AccountUnsupported
            );
            progress.updated_at_ms = observed_at_ms;
            txn.put_story_sync_progress(&chat, &progress)
                .map_err(|_| SessionFailure::STORAGE)?;
            None
        }
        StoryCommit::ArchivePage {
            chat_id,
            stories,
            next_from_story_id,
            complete,
        } => {
            let chat = ChatKey {
                scope,
                chat_id: ChatId(*chat_id),
            };
            for story in stories {
                apply_story_observation(&txn, scope, story, None, None, observed_at_ms)?;
            }
            let mut progress = txn
                .read()
                .story_sync_progress(&chat)
                .map_err(|_| SessionFailure::STORAGE)?
                .ok_or(SessionFailure::STORAGE)?;
            progress.archive_cursor = *next_from_story_id;
            progress.archive_complete = *complete;
            progress.pages_committed = progress.pages_committed.saturating_add(1);
            progress.stories_seen = progress
                .stories_seen
                .saturating_add(u64::try_from(stories.len()).unwrap_or(u64::MAX));
            progress.updated_at_ms = observed_at_ms;
            txn.put_story_sync_progress(&chat, &progress)
                .map_err(|_| SessionFailure::STORAGE)?;
            Some(chat)
        }
        StoryCommit::Inaccessible {
            poster_chat_id,
            story_id,
        } => {
            let retention = txn
                .read()
                .retention_mode(scope.account)
                .map_err(|_| SessionFailure::STORAGE)?
                .ok_or(SessionFailure::STORAGE)?;
            txn.mark_story_inaccessible(
                &StoryKey {
                    poster: ChatKey {
                        scope,
                        chat_id: ChatId(*poster_chat_id),
                    },
                    story_id: StoryId(*story_id),
                },
                retention,
                observed_at_ms,
            )
            .map_err(|_| SessionFailure::STORAGE)?;
            Some(ChatKey {
                scope,
                chat_id: ChatId(*poster_chat_id),
            })
        }
        StoryCommit::PostSucceeded {
            old_story_id,
            story,
        } => {
            let old_key = StoryKey {
                poster: ChatKey {
                    scope,
                    chat_id: ChatId(story.poster_chat_id),
                },
                story_id: StoryId(*old_story_id),
            };
            let old_was_active = txn
                .read()
                .story_appearances(&old_key)
                .map_err(|_| SessionFailure::STORAGE)?
                .iter()
                .any(|appearance| appearance.location == StoryAppearanceLocation::Active);
            let retention = txn
                .read()
                .retention_mode(scope.account)
                .map_err(|_| SessionFailure::STORAGE)?
                .ok_or(SessionFailure::STORAGE)?;
            txn.mark_story_inaccessible(&old_key, retention, observed_at_ms)
                .map_err(|_| SessionFailure::STORAGE)?;
            if old_was_active && !story.is_posted_to_chat_page {
                let chat_protected = txn
                    .read()
                    .chat(&ChatKey {
                        scope,
                        chat_id: ChatId(story.poster_chat_id),
                    })
                    .map_err(|_| SessionFailure::STORAGE)?
                    .ok_or(SessionFailure::STORAGE)?
                    .is_protected;
                let facts = state_story_facts(scope, story, chat_protected)?;
                let locators = state_story_locators(facts.key, story, chat_protected)?;
                txn.upsert_story_with_locators(&facts, &locators)
                    .map_err(|_| SessionFailure::STORAGE)?;
                txn.set_story_appearance(&story_appearance(
                    scope,
                    story,
                    StoryAppearanceLocation::Active,
                    None,
                    None,
                )?)
                .map_err(|_| SessionFailure::STORAGE)?;
            } else {
                apply_story_observation(&txn, scope, story, None, None, observed_at_ms)?;
            }
            Some(ChatKey {
                scope,
                chat_id: ChatId(story.poster_chat_id),
            })
        }
        StoryCommit::ScanComplete { chat_id } => {
            let chat = ChatKey {
                scope,
                chat_id: ChatId(*chat_id),
            };
            let mut progress = txn
                .read()
                .story_sync_progress(&chat)
                .map_err(|_| SessionFailure::STORAGE)?
                .ok_or(SessionFailure::STORAGE)?;
            progress.phase = StorySyncPhase::Ready;
            progress.failure_category = None;
            progress.retryable = false;
            progress.updated_at_ms = observed_at_ms;
            txn.put_story_sync_progress(&chat, &progress)
                .map_err(|_| SessionFailure::STORAGE)?;
            None
        }
    };
    if let Some(chat) = projected_chat {
        let active_order = match commit {
            StoryCommit::ActiveSnapshot { order, .. } => Some(*order),
            _ => None,
        };
        sync_stories_view(&txn, chat, active_order)?;
    }
    txn.commit().map_err(|_| SessionFailure::STORAGE)?;
    Ok(projected_chat)
}

/// Mirrors the approved Stories information architecture into the ordinary
/// appearance ledger without fabricating Main/Archive/folder membership.
/// Active snapshots carry Telegram's opaque story-list order. A profile pin
/// that survives active expiry keeps its previous position (or gets a stable
/// zero fallback when profile discovery is the first evidence), while a chat
/// with neither active nor profile-pinned stories disappears from Stories.
fn sync_stories_view(
    txn: &gramdrive_state::WriteTxn<'_>,
    chat: ChatKey,
    active_order: Option<i64>,
) -> Result<(), SessionFailure> {
    let list = ChatListKey {
        scope: chat.scope,
        kind: ChatListKind::Stories,
    };
    let has_member = txn
        .read()
        .has_stories_view_member(&chat)
        .map_err(|_| SessionFailure::STORAGE)?;
    if !has_member {
        txn.remove_chat_list_entry(&list, chat.chat_id)
            .map_err(|_| SessionFailure::STORAGE)?;
        return Ok(());
    }
    let prior_order = txn
        .read()
        .chat_list(&list)
        .map_err(|_| SessionFailure::STORAGE)?
        .into_iter()
        .find(|entry| entry.chat_id == chat.chat_id)
        .map(|entry| entry.sort_order);
    txn.upsert_chat_list_entry(
        &list,
        &ChatListEntry {
            chat_id: chat.chat_id,
            pinned: false,
            sort_order: active_order.or(prior_order).unwrap_or(0),
        },
    )
    .map_err(|_| SessionFailure::STORAGE)
}

fn apply_story_observation(
    txn: &gramdrive_state::WriteTxn<'_>,
    scope: gramdrive_model::identity::AccountScope,
    story: &StoryObservation,
    profile_generation: Option<u64>,
    profile_pin_order: Option<u32>,
    observed_at_ms: i64,
) -> Result<(), SessionFailure> {
    let chat_protected = txn
        .read()
        .chat(&ChatKey {
            scope,
            chat_id: ChatId(story.poster_chat_id),
        })
        .map_err(|_| SessionFailure::STORAGE)?
        .ok_or(SessionFailure::STORAGE)?
        .is_protected;
    let facts = state_story_facts(scope, story, chat_protected)?;
    let locators = state_story_locators(facts.key, story, chat_protected)?;
    txn.upsert_story_with_locators(&facts, &locators)
        .map_err(|_| SessionFailure::STORAGE)?;
    let key = facts.key;
    if story.is_posted_to_chat_page {
        let account = txn
            .read()
            .account(scope.account)
            .map_err(|_| SessionFailure::STORAGE)?
            .ok_or(SessionFailure::STORAGE)?;
        let timezone = DisplayTimeZone::named(&account.display_timezone)
            .map_err(|_| SessionFailure::STORAGE)?;
        let (year, month) = gramdrive_engine::render::civil::year_month_in_timezone(
            story.date_ms,
            timezone.timezone(),
        );
        txn.set_story_appearance(&story_appearance(
            scope,
            story,
            StoryAppearanceLocation::Month {
                year: u16::try_from(year).map_err(|_| SessionFailure::STORAGE)?,
                month: u8::try_from(month).map_err(|_| SessionFailure::STORAGE)?,
            },
            profile_generation,
            profile_pin_order,
        )?)
        .map_err(|_| SessionFailure::STORAGE)?;
    } else {
        let retention = txn
            .read()
            .retention_mode(scope.account)
            .map_err(|_| SessionFailure::STORAGE)?
            .ok_or(SessionFailure::STORAGE)?;
        txn.remove_profile_story(&key, retention, observed_at_ms)
            .map_err(|_| SessionFailure::STORAGE)?;
    }
    Ok(())
}

fn state_story_facts(
    scope: gramdrive_model::identity::AccountScope,
    story: &StoryObservation,
    chat_protected: bool,
) -> Result<StoryFacts, SessionFailure> {
    if chat_protected {
        return Ok(StoryFacts {
            key: StoryKey {
                poster: ChatKey {
                    scope,
                    chat_id: ChatId(story.poster_chat_id),
                },
                story_id: StoryId(story.story_id),
            },
            source_timestamp_ms: story.date_ms,
            mime_type: None,
            exact_size: None,
            content_version: ContentVersion::new(format!(
                "story-protected/{}/{}",
                story.poster_chat_id, story.story_id
            ))
            .map_err(|_| SessionFailure::STORAGE)?,
            availability: StateAttachmentAvailability::Restricted,
            can_be_forwarded: false,
            content_state: StoryContentState::Protected,
        });
    }
    if matches!(
        story.content_kind,
        StoryContentKind::Photo | StoryContentKind::Video
    ) && story
        .locators
        .iter()
        .filter(|locator| locator.is_primary)
        .count()
        != 1
    {
        return Err(SessionFailure::STORAGE);
    }
    let (content_state, availability, mime_type, exact_size, can_be_forwarded, content_version) =
        match story.content_kind {
            StoryContentKind::MetadataPending => (
                StoryContentState::MetadataPending,
                StateAttachmentAvailability::Unavailable,
                None,
                None,
                false,
                story.content_version.clone(),
            ),
            StoryContentKind::Photo | StoryContentKind::Video => (
                StoryContentState::Available,
                StateAttachmentAvailability::Fetchable,
                story.mime_type.clone(),
                story.exact_size,
                true,
                story.content_version.clone(),
            ),
            StoryContentKind::Protected => (
                StoryContentState::Protected,
                StateAttachmentAvailability::Restricted,
                None,
                None,
                false,
                format!(
                    "story-protected/{}/{}",
                    story.poster_chat_id, story.story_id
                ),
            ),
            StoryContentKind::Unsupported => (
                StoryContentState::Unsupported,
                StateAttachmentAvailability::Unavailable,
                None,
                None,
                false,
                story.content_version.clone(),
            ),
            StoryContentKind::LiveUnavailable => (
                StoryContentState::LiveUnavailable,
                StateAttachmentAvailability::Unavailable,
                None,
                None,
                false,
                story.content_version.clone(),
            ),
        };
    Ok(StoryFacts {
        key: StoryKey {
            poster: ChatKey {
                scope,
                chat_id: ChatId(story.poster_chat_id),
            },
            story_id: StoryId(story.story_id),
        },
        source_timestamp_ms: story.date_ms,
        mime_type,
        exact_size,
        content_version: ContentVersion::new(content_version)
            .map_err(|_| SessionFailure::STORAGE)?,
        availability,
        can_be_forwarded,
        content_state,
    })
}

fn state_story_locators(
    key: StoryKey,
    story: &StoryObservation,
    chat_protected: bool,
) -> Result<Vec<StoryContentLocatorRecord>, SessionFailure> {
    if chat_protected || !story.can_be_forwarded {
        return Ok(Vec::new());
    }
    story
        .locators
        .iter()
        .map(|locator| {
            Ok(StoryContentLocatorRecord {
                story: key,
                role: locator.role.clone(),
                file_type: match locator.file_type {
                    SourceStoryFileType::PhotoStory => StateStoryLocatorFileType::PhotoStory,
                    SourceStoryFileType::VideoStory => StateStoryLocatorFileType::VideoStory,
                    SourceStoryFileType::Thumbnail => StateStoryLocatorFileType::Thumbnail,
                },
                is_primary: locator.is_primary,
                local_file_id: locator.local_file_id,
                remote_file_id: locator.remote_file_id.clone(),
                remote_unique_id: locator.remote_unique_id.clone(),
                size: locator.size,
                expected_size: locator.expected_size,
                content_version: ContentVersion::new(locator.content_version.clone())
                    .map_err(|_| SessionFailure::STORAGE)?,
            })
        })
        .collect()
}

fn story_appearance(
    scope: gramdrive_model::identity::AccountScope,
    story: &StoryObservation,
    location: StoryAppearanceLocation,
    profile_scan_generation: Option<u64>,
    profile_pin_order: Option<u32>,
) -> Result<StoryAppearanceRecord, SessionFailure> {
    let extension = match story.content_kind {
        StoryContentKind::Photo => ".jpg",
        StoryContentKind::Video => ".mp4",
        _ => "",
    };
    Ok(StoryAppearanceRecord {
        story: StoryKey {
            poster: ChatKey {
                scope,
                chat_id: ChatId(story.poster_chat_id),
            },
            story_id: StoryId(story.story_id),
        },
        location,
        display_name: format!("Story {}{extension}", story.story_id),
        posted_at_ms: story.date_ms,
        expires_at_ms: None,
        removed_at_ms: None,
        profile_scan_generation,
        profile_pin_order,
    })
}

#[allow(clippy::too_many_arguments)]
fn drive_live_steps(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    folders: &mut FolderCatalogMachine,
    metadata: &mut UpdateMachine,
    content: &mut ContentCoordinator,
    client: &TdClient,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
    listener: &Arc<dyn NamespaceProgressListener>,
) -> Result<bool, SessionFailure> {
    let mut changed = false;
    for _ in 0..MAX_LIVE_STEPS_PER_TICK {
        if cancelled.load(Ordering::Acquire) {
            return Ok(changed);
        }
        match content
            .live
            .next_step()
            .map_err(|_| SessionFailure::SOURCE)?
        {
            LiveStep::Submit(request) => {
                if content
                    .scheduler
                    .observe(store, scope, now_ms())
                    .map_err(|_| SessionFailure::STORAGE)?
                    .pending_until_ms
                    .is_some()
                {
                    return Ok(changed);
                }
                content.live_request_chat = request.get("chat_id").and_then(|id| id.as_i64());
                let pending = client
                    .request(request)
                    .map_err(|_| SessionFailure::SOURCE)?;
                content
                    .scheduler
                    .note_dispatch(store, scope, now_ms())
                    .map_err(|_| SessionFailure::STORAGE)?;
                let outcome = wait_for_content_response(
                    store,
                    scope,
                    pending,
                    folders,
                    metadata,
                    &mut content.live,
                    &mut content.stories,
                    updates,
                    cancelled,
                )?;
                content
                    .live
                    .on_response(outcome)
                    .map_err(|_| SessionFailure::SOURCE)?;
            }
            LiveStep::Backoff(backoff) => {
                let retry_after_ms = backoff
                    .retry_after_secs
                    .and_then(|seconds| i64::try_from(seconds.saturating_mul(1_000)).ok());
                let flood = content
                    .scheduler
                    .note_flood_wait(store, scope, retry_after_ms, backoff.attempt, now_ms())
                    .map_err(|_| SessionFailure::STORAGE)?;
                if flood.exhausted || backoff.attempt > MAX_RETRY_ATTEMPTS {
                    if let Some(chat_id) = content.live_request_chat {
                        put_content_progress(
                            store,
                            ChatKey {
                                scope,
                                chat_id: ChatId(chat_id),
                            },
                            content_progress(
                                ChatContentPhase::Failed,
                                Some("rate-limited"),
                                true,
                                backoff.attempt,
                                Some(flood.until_ms),
                            ),
                        )?;
                    }
                    return Err(SessionFailure::RATE_LIMITED);
                }
                return Ok(changed);
            }
            LiveStep::Commit(commit) => {
                apply_live_commit(store, scope, &commit, now_ms())?;
                content.live_request_chat = None;
                changed = true;
            }
            LiveStep::Unresolved {
                chat_id,
                recovery_required,
            } => {
                let tracked = resolve_content_chat(
                    store, scope, chat_id, folders, metadata, content, client, updates, cancelled,
                    listener,
                )?;
                changed |= tracked;
                if recovery_required && tracked {
                    put_content_progress(
                        store,
                        ChatKey {
                            scope,
                            chat_id: ChatId(chat_id),
                        },
                        content_progress(
                            ChatContentPhase::Degraded,
                            Some("live-buffer-overflow"),
                            true,
                            1,
                            None,
                        ),
                    )?;
                }
            }
            LiveStep::ResyncRequired => {
                // The complete metadata snapshot plus resumable background
                // crawl is the recovery path for identities that could not
                // fit in the bounded pre-readiness queue.
                changed = true;
            }
            LiveStep::RecoveryRequired { chat_id } => {
                require_full_live_recovery(store, scope, chat_id)?;
                changed = true;
            }
            LiveStep::Degraded(degraded) => {
                put_content_progress(
                    store,
                    ChatKey {
                        scope,
                        chat_id: ChatId(degraded.chat_id),
                    },
                    content_progress(ChatContentPhase::Degraded, Some("live-gap"), true, 1, None),
                )?;
                changed = true;
            }
            LiveStep::Idle => return Ok(changed),
        }
    }
    Ok(changed)
}

#[allow(clippy::too_many_arguments)]
fn resolve_content_chat(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    chat_id: i64,
    folders: &mut FolderCatalogMachine,
    metadata: &mut UpdateMachine,
    content: &mut ContentCoordinator,
    client: &TdClient,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
    listener: &Arc<dyn NamespaceProgressListener>,
) -> Result<bool, SessionFailure> {
    let mut chat = {
        let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
        txn.chat(&ChatKey {
            scope,
            chat_id: ChatId(chat_id),
        })
        .map_err(|_| SessionFailure::STORAGE)?
    };
    if chat.is_none() {
        let pending = match client.request(json!({"@type": "getChat", "chat_id": chat_id})) {
            Ok(pending) => pending,
            Err(_) => {
                degrade_unresolved_chat(&mut content.live, chat_id, listener);
                return Ok(false);
            }
        };
        content
            .scheduler
            .note_dispatch(store, scope, now_ms())
            .map_err(|_| SessionFailure::STORAGE)?;
        let value = match wait_for_content_response(
            store,
            scope,
            pending,
            folders,
            metadata,
            &mut content.live,
            &mut content.stories,
            updates,
            cancelled,
        ) {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => {
                degrade_unresolved_chat(&mut content.live, chat_id, listener);
                return Ok(false);
            }
            Err(failure) if failure.category == SessionFailure::SOURCE.category => {
                degrade_unresolved_chat(&mut content.live, chat_id, listener);
                return Ok(false);
            }
            Err(failure) => return Err(failure),
        };
        metadata.on_update(&json!({"@type": "updateNewChat", "chat": value}));
        apply_live_batch(
            store,
            scope,
            folders,
            metadata,
            Some(&mut content.live),
            Some(&mut content.stories),
            SessionIo {
                client,
                updates,
                cancelled,
            },
        )?;
        rebuild_projection(store, scope)?;
        initialize_content_progress(store, scope)?;
        let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
        chat = txn
            .chat(&ChatKey {
                scope,
                chat_id: ChatId(chat_id),
            })
            .map_err(|_| SessionFailure::STORAGE)?;
    }
    let Some(chat) = chat else {
        content.live.ignore_untracked_chat(chat_id);
        return Ok(false);
    };
    if chat.is_protected || chat.deleted_at_ms.is_some() {
        content.live.ignore_untracked_chat(chat_id);
        put_content_progress(
            store,
            chat.key,
            content_progress(
                ChatContentPhase::Protected,
                Some("protected-content"),
                false,
                0,
                None,
            ),
        )?;
        return Ok(false);
    }
    let newest = {
        let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
        txn.chat_sync_state(&chat.key)
            .map_err(|_| SessionFailure::STORAGE)?
            .and_then(|sync| sync.window)
            .map(|window| window.newest.0)
    };
    Ok(content.live.track_chat(chat_id, newest))
}

fn degrade_unresolved_chat(
    live: &mut LiveMachine,
    chat_id: i64,
    listener: &Arc<dyn NamespaceProgressListener>,
) {
    // The update that discovered this chat is item-local. Drop its bounded
    // buffer after an unavailable metadata lookup so it cannot poison the
    // account namespace; a later Telegram update recreates the unresolved
    // entry and retries the same normal resolution path.
    live.ignore_untracked_chat(chat_id);
    listener.on_progress(NamespaceProgress::Degraded {
        category: "chat-metadata".to_owned(),
        retryable: true,
    });
}

#[allow(clippy::too_many_arguments)]
fn wait_for_content_response(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    mut pending: gramdrive_source_tdjson::PendingRequest,
    folders: &mut FolderCatalogMachine,
    metadata: &mut UpdateMachine,
    live: &mut LiveMachine,
    stories: &mut StoryMachine,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
) -> Result<Result<serde_json::Value, gramdrive_source_tdjson::TdError>, SessionFailure> {
    let deadline = std::time::Instant::now() + REQUEST_TIMEOUT;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(SessionFailure::SOURCE);
        }
        while let Ok(update) = updates.try_recv() {
            folders.on_update(&update);
            metadata.on_update(&update);
            route_content_live_update(store, scope, live, &update)?;
            stories.on_update(&update);
        }
        match pending.wait_timeout(Duration::from_millis(10)) {
            Ok(outcome) => return Ok(outcome),
            Err(still_pending) => pending = still_pending,
        }
        if std::time::Instant::now() >= deadline {
            return Err(SessionFailure::SOURCE);
        }
    }
}

#[cfg(test)]
fn apply_history_commit(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    commit: &HistoryCommit,
    observed_at_ms: i64,
) -> Result<(), SessionFailure> {
    apply_history_commit_with_publication(store, scope, commit, observed_at_ms, true)
}

fn apply_history_commit_with_publication(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    commit: &HistoryCommit,
    observed_at_ms: i64,
    publish_projection: bool,
) -> Result<(), SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    let chat = ChatKey {
        scope,
        chat_id: ChatId(commit.chat_id),
    };
    let before_watermark = txn
        .read()
        .latest_event_seq(&chat)
        .map_err(|_| SessionFailure::STORAGE)?;
    let chat_restricted = txn
        .read()
        .chat(&chat)
        .map_err(|_| SessionFailure::STORAGE)?
        .is_none_or(|record| record.is_protected || record.deleted_at_ms.is_some());
    let changes = observed_changes(&commit.records, observed_at_ms, chat_restricted)?;
    txn.apply_message_changes(&chat, &changes)
        .map_err(|_| SessionFailure::STORAGE)?;
    enforce_observed_message_restrictions(&txn, chat, &commit.records, chat_restricted)?;
    project_current_attachments(
        &txn,
        chat,
        commit
            .records
            .iter()
            .map(|record| MessageId(record.message_id)),
        observed_at_ms,
    )?;
    let stored = txn
        .read()
        .chat_sync_state(&chat)
        .map_err(|_| SessionFailure::STORAGE)?;
    let prior_progress = txn
        .read()
        .chat_content_progress(&chat)
        .map_err(|_| SessionFailure::STORAGE)?;
    if full_live_recovery_pending(prior_progress.as_ref()) {
        // This page belongs to a crawl generation planned before an edit
        // invalidated the window. Its normalized rows are still useful and
        // idempotent, but its cursor cannot clear the durable recovery fence
        // or erase the last known monotonic checkpoint.
    } else {
        let window = merge_history_window(commit, stored.and_then(|sync| sync.window));
        txn.record_chat_sync(
            &chat,
            &ChatSyncRecord {
                window,
                history_complete: commit.history_complete,
                last_sync_at_ms: Some(observed_at_ms),
            },
        )
        .map_err(|_| SessionFailure::STORAGE)?;
        txn.put_chat_content_progress(
            &chat,
            &content_progress(
                if commit.history_complete {
                    ChatContentPhase::Ready
                } else {
                    ChatContentPhase::Syncing
                },
                None,
                false,
                0,
                None,
            ),
        )
        .map_err(|_| SessionFailure::STORAGE)?;
    }
    if publish_projection {
        reconcile_chat_projection_txn(&txn, chat)?;
    }
    mark_affected_months(&txn, chat, before_watermark)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

fn merge_history_window(commit: &HistoryCommit, stored: Option<SyncWindow>) -> Option<SyncWindow> {
    match (commit.window, stored) {
        (None, stored) => stored,
        (Some(window), None) => Some(SyncWindow {
            oldest: MessageId(window.oldest_message_id),
            newest: MessageId(window.newest_message_id),
        }),
        (Some(window), Some(stored)) => Some(SyncWindow {
            oldest: MessageId(window.oldest_message_id.min(stored.oldest.0)),
            newest: MessageId(window.newest_message_id.max(stored.newest.0)),
        }),
    }
}

fn apply_live_commit(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    commit: &LiveCommit,
    observed_at_ms: i64,
) -> Result<(), SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    let chat = ChatKey {
        scope,
        chat_id: ChatId(commit.chat_id),
    };
    let before_watermark = txn
        .read()
        .latest_event_seq(&chat)
        .map_err(|_| SessionFailure::STORAGE)?;
    let chat_restricted = txn
        .read()
        .chat(&chat)
        .map_err(|_| SessionFailure::STORAGE)?
        .is_none_or(|record| record.is_protected || record.deleted_at_ms.is_some());
    let changes: Vec<MessageChange> = commit
        .changes
        .iter()
        .map(|change| match change {
            LiveChange::Observed(record) => {
                revision_of(record, observed_at_ms, chat_restricted).map(MessageChange::Observed)
            }
            LiveChange::Deleted { message_id } => Ok(MessageChange::Deleted {
                message_id: MessageId(*message_id),
                observed_at_ms,
            }),
        })
        .collect::<Result<_, SessionFailure>>()?;
    txn.apply_message_changes(&chat, &changes)
        .map_err(|_| SessionFailure::STORAGE)?;
    let observed_records: Vec<&MessageRecord> = commit
        .changes
        .iter()
        .filter_map(|change| match change {
            LiveChange::Observed(record) => Some(record.as_ref()),
            LiveChange::Deleted { .. } => None,
        })
        .collect();
    enforce_observed_message_restrictions(&txn, chat, &observed_records, chat_restricted)?;
    project_current_attachments(
        &txn,
        chat,
        commit.changes.iter().filter_map(|change| match change {
            LiveChange::Observed(record) => Some(MessageId(record.message_id)),
            LiveChange::Deleted { .. } => None,
        }),
        observed_at_ms,
    )?;
    let existing_sync = txn
        .read()
        .chat_sync_state(&chat)
        .map_err(|_| SessionFailure::STORAGE)?;
    if let (Some(advance), Some(sync), Some(window)) = (
        commit.advance_newest,
        existing_sync,
        existing_sync.and_then(|record| record.window),
    ) {
        txn.record_chat_sync(
            &chat,
            &ChatSyncRecord {
                window: Some(SyncWindow {
                    oldest: window.oldest,
                    newest: MessageId(window.newest.0.max(advance)),
                }),
                history_complete: sync.history_complete,
                last_sync_at_ms: Some(observed_at_ms),
            },
        )
        .map_err(|_| SessionFailure::STORAGE)?;
    }
    let prior_progress = txn
        .read()
        .chat_content_progress(&chat)
        .map_err(|_| SessionFailure::STORAGE)?;
    if full_live_recovery_pending(prior_progress.as_ref()) {
        // A new-message/delete commit is not proof that the pending edit was
        // re-fetched. Keep the recovery fence and absent window crash-durable.
        reconcile_chat_projection_txn(&txn, chat)?;
        mark_affected_months(&txn, chat, before_watermark)?;
        return txn.commit().map_err(|_| SessionFailure::STORAGE);
    }
    let prior_phase = prior_progress.map(|progress| progress.phase);
    let phase = if existing_sync.is_some_and(|sync| sync.history_complete)
        || prior_phase == Some(ChatContentPhase::Ready)
    {
        ChatContentPhase::Ready
    } else {
        ChatContentPhase::Syncing
    };
    txn.put_chat_content_progress(&chat, &content_progress(phase, None, false, 0, None))
        .map_err(|_| SessionFailure::STORAGE)?;
    reconcile_chat_projection_txn(&txn, chat)?;
    mark_affected_months(&txn, chat, before_watermark)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

fn mark_affected_months(
    txn: &gramdrive_state::WriteTxn<'_>,
    chat: ChatKey,
    before_watermark: i64,
) -> Result<(), SessionFailure> {
    let after_watermark = txn
        .read()
        .latest_event_seq(&chat)
        .map_err(|_| SessionFailure::STORAGE)?;
    if after_watermark == before_watermark {
        return Ok(());
    }
    let touched = txn
        .read()
        .affected_message_instants(&chat, before_watermark, after_watermark)
        .map_err(|_| SessionFailure::STORAGE)?;
    let account = txn
        .read()
        .account(chat.scope.account)
        .map_err(|_| SessionFailure::STORAGE)?
        .ok_or(SessionFailure::STORAGE)?;
    let timezone =
        DisplayTimeZone::named(&account.display_timezone).map_err(|_| SessionFailure::STORAGE)?;
    dirty_affected(txn, chat, &touched, &timezone).map_err(|_| SessionFailure::STORAGE)?;
    Ok(())
}

fn observed_changes(
    records: &[MessageRecord],
    observed_at_ms: i64,
    chat_restricted: bool,
) -> Result<Vec<MessageChange>, SessionFailure> {
    records
        .iter()
        .map(|record| {
            revision_of(record, observed_at_ms, chat_restricted).map(MessageChange::Observed)
        })
        .collect()
}

fn revision_of(
    record: &MessageRecord,
    observed_at_ms: i64,
    chat_restricted: bool,
) -> Result<MessageRevision, SessionFailure> {
    let record = retained_message_record(record, chat_restricted);
    Ok(MessageRevision {
        message_id: MessageId(record.message_id),
        sender_id: match record.sender {
            SenderRef::User { user_id } => Some(user_id),
            SenderRef::Chat { chat_id } => Some(chat_id),
            SenderRef::Unknown { .. } => None,
        },
        sent_at_ms: record.sent_at_ms,
        edited_at_ms: record.edited_at_ms,
        observed_at_ms,
        payload_schema: NORMALIZED_MESSAGE_SCHEMA_FAMILY,
        payload: serde_json::to_vec(&record).map_err(|_| SessionFailure::STORAGE)?,
    })
}

fn enforce_observed_message_restrictions(
    txn: &gramdrive_state::WriteTxn<'_>,
    chat: ChatKey,
    records: &[impl std::borrow::Borrow<MessageRecord>],
    chat_restricted: bool,
) -> Result<(), SessionFailure> {
    if chat_restricted {
        txn.purge_restricted_chat_message_content(&chat)
            .map_err(|_| SessionFailure::STORAGE)?;
        return Ok(());
    }
    let mut message_ids = HashSet::new();
    for record in records {
        let record = record.borrow();
        if message_restriction(record, false).is_some() {
            message_ids.insert(MessageId(record.message_id));
        }
    }
    for message_id in message_ids {
        txn.purge_restricted_message_history(&MessageKey { chat, message_id })
            .map_err(|_| SessionFailure::STORAGE)?;
    }
    Ok(())
}

fn message_restriction(
    record: &MessageRecord,
    chat_restricted: bool,
) -> Option<ContentRestriction> {
    match &record.content {
        MessageContent::Restricted { reason } => Some(*reason),
        _ if record.self_destruct.is_some() => Some(ContentRestriction::Ephemeral),
        _ if chat_restricted || !record.can_be_saved => Some(ContentRestriction::SaveForbidden),
        _ => None,
    }
}

/// Returns the only normalized shape permitted to cross the persistence
/// boundary. A per-message restriction and a chat-level restriction are
/// equivalent here: both discard every copyable or relationship-bearing body
/// fact while preserving only source identity and timestamps needed for sync.
fn retained_message_record(record: &MessageRecord, chat_restricted: bool) -> MessageRecord {
    let Some(reason) = message_restriction(record, chat_restricted) else {
        return record.clone();
    };
    MessageRecord {
        chat_id: record.chat_id,
        message_id: record.message_id,
        sender: record.sender.clone(),
        sent_at_ms: record.sent_at_ms,
        edited_at_ms: record.edited_at_ms,
        reply: None,
        topic: None,
        album_id: None,
        reactions: Vec::new(),
        can_be_saved: false,
        self_destruct: None,
        content: redact_restricted_content(record.content.clone(), reason),
    }
}

fn project_current_attachments(
    txn: &gramdrive_state::WriteTxn<'_>,
    chat: ChatKey,
    message_ids: impl IntoIterator<Item = MessageId>,
    observed_at_ms: i64,
) -> Result<(), SessionFailure> {
    let message_ids: HashSet<_> = message_ids.into_iter().collect();
    for message_id in message_ids {
        let message = MessageKey { chat, message_id };
        let Some(payload) = txn
            .read()
            .current_message_payload(&message)
            .map_err(|_| SessionFailure::STORAGE)?
        else {
            continue;
        };
        if payload.schema != NORMALIZED_MESSAGE_SCHEMA_FAMILY {
            return Err(SessionFailure::STORAGE);
        }
        let record: MessageRecord =
            serde_json::from_slice(&payload.bytes).map_err(|_| SessionFailure::STORAGE)?;
        let mapped = map_message_attachments(&record, chat.scope);
        let facts = mapped
            .iter()
            .map(attachment_facts)
            .collect::<Result<Vec<_>, SessionFailure>>()?;
        txn.replace_message_attachments(&message, &facts, observed_at_ms)
            .map_err(|_| SessionFailure::STORAGE)?;
    }
    Ok(())
}

fn attachment_facts(mapped: &MappedAttachment) -> Result<AttachmentFacts, SessionFailure> {
    let content_version = attachment_content_version(mapped)?;
    Ok(AttachmentFacts {
        key: mapped.key,
        logical_kind: match mapped.logical_kind {
            MappedAttachmentLogicalKind::Photo => StateAttachmentLogicalKind::Photo,
            MappedAttachmentLogicalKind::Video => StateAttachmentLogicalKind::Video,
            MappedAttachmentLogicalKind::Animation => StateAttachmentLogicalKind::Animation,
            MappedAttachmentLogicalKind::Audio => StateAttachmentLogicalKind::Audio,
            MappedAttachmentLogicalKind::Voice => StateAttachmentLogicalKind::Voice,
            MappedAttachmentLogicalKind::VideoNote => StateAttachmentLogicalKind::VideoNote,
            MappedAttachmentLogicalKind::Sticker => StateAttachmentLogicalKind::Sticker,
            MappedAttachmentLogicalKind::Document => StateAttachmentLogicalKind::Document,
        },
        telegram_representation: match mapped.telegram_representation {
            MappedTelegramRepresentation::OriginalDocument => {
                StateTelegramRepresentation::OriginalDocument
            }
            MappedTelegramRepresentation::Photo => StateTelegramRepresentation::Photo,
            MappedTelegramRepresentation::Video => StateTelegramRepresentation::Video,
            MappedTelegramRepresentation::Animation => StateTelegramRepresentation::Animation,
            MappedTelegramRepresentation::Audio => StateTelegramRepresentation::Audio,
            MappedTelegramRepresentation::Voice => StateTelegramRepresentation::Voice,
            MappedTelegramRepresentation::VideoNote => StateTelegramRepresentation::VideoNote,
            MappedTelegramRepresentation::Sticker => StateTelegramRepresentation::Sticker,
        },
        fidelity: match mapped.fidelity {
            MappedAttachmentFidelity::Original => StateAttachmentFidelity::Original,
            MappedAttachmentFidelity::TelegramVariant => StateAttachmentFidelity::TelegramVariant,
            MappedAttachmentFidelity::MetadataOnly => StateAttachmentFidelity::MetadataOnly,
        },
        source_name: mapped.source_name.clone(),
        mime_type: mapped.descriptor.mime_type.clone(),
        exact_size: mapped.descriptor.size,
        content_version,
        telegram_unique_id: mapped.descriptor.remote_unique_id.clone(),
        telegram_local_file_id: mapped.descriptor.file_id,
        telegram_file_id: mapped.descriptor.remote_id.clone(),
        file_reference: None,
        availability: match mapped.descriptor.availability {
            SourceAttachmentAvailability::Fetchable => StateAttachmentAvailability::Fetchable,
            SourceAttachmentAvailability::Restricted => StateAttachmentAvailability::Restricted,
            SourceAttachmentAvailability::Unavailable => StateAttachmentAvailability::Unavailable,
            SourceAttachmentAvailability::ViewOnce => StateAttachmentAvailability::ViewOnce,
        },
        can_be_saved: mapped.can_be_saved,
    })
}

fn attachment_content_version(mapped: &MappedAttachment) -> Result<ContentVersion, SessionFailure> {
    let stable_identity = mapped.descriptor.remote_unique_id.as_ref().map_or_else(
        || {
            json!({
                "fallback_attachment": {
                    "account_id": mapped.key.message.chat.scope.account.account_id.0,
                    "namespace_version": mapped.key.message.chat.scope.namespace_version.0,
                    "chat_id": mapped.key.message.chat.chat_id.0,
                    "message_id": mapped.key.message.message_id.0,
                    "index": mapped.key.index.0,
                },
                "main_content_generation": {
                    "telegram_local_file_id": mapped.descriptor.file_id,
                    "telegram_remote_id": mapped.descriptor.remote_id,
                },
            })
        },
        |unique_id| json!({"telegram_unique_id": unique_id}),
    );
    // A content version pins the represented attachment bytes, not the
    // current route to those bytes. When TDLib exposes a stable remote unique
    // id, its local file id, remote id, and all preview objects are deliberately
    // absent: those routes can refresh independently of the main content. If
    // that stable identity is unavailable, canonical identity plus the current
    // main locators forms a conservative generation signal: a possible main
    // replacement invalidates verified bytes, while preview-only refreshes do
    // not. This deliberately sacrifices locator-refresh stability only in the
    // branch where Telegram supplied no stable content identity.
    let stable_facts = json!({
        "schema": 2,
        "identity": stable_identity,
        "logical_kind": mapped.logical_kind.tag(),
        "telegram_representation": mapped.telegram_representation.tag(),
        "exact_size": mapped.descriptor.size,
        "width": mapped.descriptor.width,
        "height": mapped.descriptor.height,
        "duration_secs": mapped.descriptor.duration_secs,
    });
    let stable_bytes = serde_json::to_vec(&stable_facts).map_err(|_| SessionFailure::STORAGE)?;
    ContentVersion::new(format!(
        "telegram-attachment-v2-{:016x}",
        stable_hash(&stable_bytes)
    ))
    .map_err(|_| SessionFailure::STORAGE)
}

#[derive(Debug, Clone)]
struct NormalizedMessageDecoder {
    scope: gramdrive_model::identity::AccountScope,
    media_names: HashMap<AttachmentKey, String>,
}

impl MessagePayloadDecoder for NormalizedMessageDecoder {
    type Error = String;

    fn decode(&self, payload: &MessagePayload) -> Result<DecodedRevision, Self::Error> {
        if payload.schema != NORMALIZED_MESSAGE_SCHEMA_FAMILY {
            return Err(format!(
                "unsupported normalized schema {}",
                payload.schema.0
            ));
        }
        let record: MessageRecord =
            serde_json::from_slice(&payload.bytes).map_err(|error| error.to_string())?;
        Ok(DecodedRevision {
            edited_at_ms: record.edited_at_ms,
            body: render_body(&record, self.scope, &self.media_names),
        })
    }
}

fn render_body(
    record: &MessageRecord,
    scope: gramdrive_model::identity::AccountScope,
    media_names: &HashMap<AttachmentKey, String>,
) -> MessageBody {
    if !record.can_be_saved
        || record.self_destruct.is_some()
        || matches!(record.content, MessageContent::Restricted { .. })
    {
        return MessageBody {
            text: None,
            entities: Vec::new(),
            reply_to: None,
            thread_top: None,
            topic_id: None,
            album_id: None,
            reactions: Vec::new(),
            attachments: Vec::new(),
            service: None,
            protected: true,
        };
    }
    let (text, entities) = render_text(&record.content);
    MessageBody {
        text,
        entities,
        reply_to: match &record.reply {
            Some(ReplyTarget::Message { message_id, .. }) => Some(MessageId(*message_id)),
            _ => None,
        },
        thread_top: None,
        topic_id: match &record.topic {
            Some(TopicRef::Forum { forum_topic_id }) => Some(*forum_topic_id),
            Some(TopicRef::DirectMessages { topic_id })
            | Some(TopicRef::SavedMessages { topic_id }) => Some(*topic_id),
            _ => None,
        },
        album_id: record.album_id,
        reactions: record
            .reactions
            .iter()
            .filter_map(|reaction| {
                let key = match &reaction.kind {
                    SourceReactionKind::Emoji { emoji } => RenderReactionKey::Emoji(emoji.clone()),
                    SourceReactionKind::CustomEmoji { custom_emoji_id } => {
                        RenderReactionKey::Custom(*custom_emoji_id)
                    }
                    SourceReactionKind::Paid | SourceReactionKind::Unknown { .. } => return None,
                };
                Some(RenderReaction {
                    key,
                    count: reaction.count,
                    chosen: reaction.chosen,
                })
            })
            .collect(),
        attachments: render_attachment(record, scope, media_names),
        service: render_service(&record.content),
        protected: false,
    }
}

fn render_text(content: &MessageContent) -> (Option<String>, Vec<RenderEntity>) {
    let formatted = match content {
        MessageContent::Text { text }
        | MessageContent::Photo { caption: text, .. }
        | MessageContent::Video { caption: text, .. }
        | MessageContent::Animation { caption: text, .. }
        | MessageContent::Audio { caption: text, .. }
        | MessageContent::Document { caption: text, .. }
        | MessageContent::VoiceNote { caption: text, .. } => Some(text),
        MessageContent::Sticker { emoji, .. } if !emoji.is_empty() => {
            return (Some(emoji.clone()), Vec::new());
        }
        _ => None,
    };
    let Some(FormattedText { text, entities }) = formatted else {
        return (None, Vec::new());
    };
    let text = (!text.is_empty()).then(|| text.clone());
    let entities = entities
        .iter()
        .map(|entity| RenderEntity {
            kind: match &entity.kind {
                TextEntityKind::Bold => RenderEntityKind::Bold,
                TextEntityKind::Italic => RenderEntityKind::Italic,
                TextEntityKind::Underline => RenderEntityKind::Underline,
                TextEntityKind::Strikethrough => RenderEntityKind::Strikethrough,
                TextEntityKind::Spoiler => RenderEntityKind::Spoiler,
                TextEntityKind::Code => RenderEntityKind::Code,
                TextEntityKind::Pre => RenderEntityKind::Pre { language: None },
                TextEntityKind::PreCode { language } => RenderEntityKind::Pre {
                    language: Some(language.clone()),
                },
                TextEntityKind::BlockQuote | TextEntityKind::ExpandableBlockQuote => {
                    RenderEntityKind::Blockquote
                }
                TextEntityKind::Url => RenderEntityKind::Url,
                TextEntityKind::TextUrl { url } => RenderEntityKind::TextLink { url: url.clone() },
                TextEntityKind::Mention => RenderEntityKind::Mention,
                TextEntityKind::MentionName { user_id } => {
                    RenderEntityKind::TextMention { user_id: *user_id }
                }
                TextEntityKind::Hashtag => RenderEntityKind::Hashtag,
                TextEntityKind::Cashtag => RenderEntityKind::Cashtag,
                TextEntityKind::BotCommand => RenderEntityKind::BotCommand,
                TextEntityKind::EmailAddress => RenderEntityKind::Email,
                TextEntityKind::PhoneNumber => RenderEntityKind::PhoneNumber,
                TextEntityKind::BankCardNumber => RenderEntityKind::BankCard,
                TextEntityKind::CustomEmoji { custom_emoji_id } => RenderEntityKind::CustomEmoji {
                    document_id: *custom_emoji_id,
                },
                TextEntityKind::MediaTimestamp { .. } => RenderEntityKind::Other {
                    kind: "media_timestamp".to_owned(),
                },
                TextEntityKind::Unknown { raw_type } => RenderEntityKind::Other {
                    kind: raw_type.clone(),
                },
            },
            offset: entity.offset,
            length: entity.length,
        })
        .collect();
    (text, entities)
}

fn render_attachment(
    record: &MessageRecord,
    scope: gramdrive_model::identity::AccountScope,
    media_names: &HashMap<AttachmentKey, String>,
) -> Vec<RenderAttachment> {
    map_message_attachments(record, scope)
        .into_iter()
        .map(|mapped| RenderAttachment {
            index: mapped.key.index,
            media_kind: match mapped.logical_kind {
                MappedAttachmentLogicalKind::Photo => RenderMediaKind::Photo,
                MappedAttachmentLogicalKind::Video => RenderMediaKind::Video,
                MappedAttachmentLogicalKind::Animation => RenderMediaKind::Animation,
                MappedAttachmentLogicalKind::Audio => RenderMediaKind::Audio,
                MappedAttachmentLogicalKind::Voice => RenderMediaKind::Voice,
                MappedAttachmentLogicalKind::VideoNote => RenderMediaKind::VideoNote,
                MappedAttachmentLogicalKind::Sticker => RenderMediaKind::Sticker,
                MappedAttachmentLogicalKind::Document => RenderMediaKind::Document,
            },
            telegram_representation: match mapped.telegram_representation {
                MappedTelegramRepresentation::OriginalDocument => {
                    RenderRepresentation::OriginalDocument
                }
                MappedTelegramRepresentation::Photo => RenderRepresentation::Photo,
                MappedTelegramRepresentation::Video => RenderRepresentation::Video,
                MappedTelegramRepresentation::Animation => RenderRepresentation::Animation,
                MappedTelegramRepresentation::Audio => RenderRepresentation::Audio,
                MappedTelegramRepresentation::Voice => RenderRepresentation::Voice,
                MappedTelegramRepresentation::VideoNote => RenderRepresentation::VideoNote,
                MappedTelegramRepresentation::Sticker => RenderRepresentation::Sticker,
            },
            fidelity: match mapped.fidelity {
                MappedAttachmentFidelity::Original => RenderAttachmentFidelity::Original,
                MappedAttachmentFidelity::TelegramVariant => {
                    RenderAttachmentFidelity::TelegramVariant
                }
                MappedAttachmentFidelity::MetadataOnly => RenderAttachmentFidelity::MetadataOnly,
            },
            source_name: mapped.source_name,
            mime_type: mapped.descriptor.mime_type,
            exact_size: mapped.descriptor.size,
            availability: match mapped.descriptor.availability {
                SourceAttachmentAvailability::Fetchable => RenderAvailability::Fetchable,
                SourceAttachmentAvailability::Restricted => RenderAvailability::Restricted,
                SourceAttachmentAvailability::Unavailable => RenderAvailability::Unavailable,
                SourceAttachmentAvailability::ViewOnce => RenderAvailability::ViewOnce,
            },
            content_hash: None,
            media_name: media_names.get(&mapped.key).cloned(),
        })
        .collect()
}

fn render_service(content: &MessageContent) -> Option<RenderServiceAction> {
    let MessageContent::Service { action } = content else {
        return match content {
            MessageContent::Expired { kind, .. } => Some(RenderServiceAction::Other {
                kind: format!("expired_{kind:?}").to_ascii_lowercase(),
            }),
            MessageContent::Unsupported { content } => Some(RenderServiceAction::Other {
                kind: format!("{}:{}", content.raw_type, content.raw_json),
            }),
            _ => None,
        };
    };
    Some(match action {
        SourceServiceAction::ChatCreated { title, .. } => RenderServiceAction::ChatCreated {
            title: title.clone(),
        },
        SourceServiceAction::TitleChanged { title } => RenderServiceAction::ChatTitleChanged {
            title: title.clone(),
        },
        SourceServiceAction::MembersAdded { user_ids } => RenderServiceAction::MembersAdded {
            user_ids: user_ids.clone(),
        },
        SourceServiceAction::MemberRemoved { user_id } => {
            RenderServiceAction::MemberRemoved { user_id: *user_id }
        }
        SourceServiceAction::MessagePinned { message_id } => RenderServiceAction::MessagePinned {
            message_id: MessageId(*message_id),
        },
        SourceServiceAction::AutoDeleteTimeChanged { seconds } => {
            RenderServiceAction::AutoDeleteTimerChanged {
                seconds: i64::from(*seconds),
            }
        }
        other => RenderServiceAction::Other {
            kind: format!("{other:?}"),
        },
    })
}

fn render_pending_months(
    store: &mut StateStore,
    cache_root: &Path,
    published_at_ms: i64,
) -> Result<bool, SessionFailure> {
    let plan = {
        let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
        plan_worklist(&txn, MAX_RENDER_WORKLIST_ITEMS_PER_TICK)
            .map_err(|_| SessionFailure::RENDER)?
    };
    let mut published = false;
    let mut chats = Vec::new();
    let mut months = Vec::new();
    for job in plan.jobs {
        match job.partition {
            DocPartition::Chat if job.class == DocClass::ChatJson => {
                if !chats.contains(&job.chat) {
                    chats.push(job.chat);
                }
            }
            DocPartition::Month { year, month } => {
                if !months.contains(&(job.chat, year, month)) {
                    months.push((job.chat, year, month));
                }
            }
            DocPartition::Chat | DocPartition::Year { .. } => {}
        }
    }
    for chat in chats {
        let source = store
            .read_txn()
            .map_err(|_| SessionFailure::STORAGE)?
            .chat(&chat)
            .map_err(|_| SessionFailure::STORAGE)?;
        if source.as_ref().is_some_and(|record| record.is_protected) {
            skip_policy_excluded_documents(store, chat, published_at_ms)?;
            continue;
        }
        let source =
            source.filter(|record| record.left_at_ms.is_none() && record.deleted_at_ms.is_none());
        let Some(source) = source else {
            continue;
        };
        let rendered = compose_chat_metadata(&source).map_err(|_| SessionFailure::RENDER)?;
        let staged =
            stage_chat_metadata(cache_root, &rendered).map_err(|_| SessionFailure::RENDER)?;
        match publish_chat_metadata(store, &rendered, &staged, published_at_ms) {
            Ok(_) => published = true,
            Err(RenderPipelineError::PublicationLeased) => continue,
            Err(_) => return Err(SessionFailure::RENDER),
        }
    }
    for (chat, year, month) in months {
        let source = store
            .read_txn()
            .map_err(|_| SessionFailure::STORAGE)?
            .chat(&chat)
            .map_err(|_| SessionFailure::STORAGE)?;
        if source.as_ref().is_some_and(|record| record.is_protected) {
            skip_policy_excluded_documents(store, chat, published_at_ms)?;
            continue;
        }
        if source.is_none_or(|record| record.deleted_at_ms.is_some()) {
            continue;
        }
        let (snapshot, media_names) = {
            let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
            let account = txn
                .account(chat.scope.account)
                .map_err(|_| SessionFailure::STORAGE)?
                .ok_or(SessionFailure::STORAGE)?;
            let timezone = DisplayTimeZone::named(&account.display_timezone)
                .map_err(|_| SessionFailure::RENDER)?;
            let (start_ms, end_ms) = timezone
                .month_bounds_ms(year, month)
                .map_err(|_| SessionFailure::RENDER)?;
            let snapshot = txn
                .month_render_snapshot(chat, start_ms, end_ms)
                .map_err(|_| SessionFailure::STORAGE)?;
            let mut media_names = HashMap::new();
            for message in &snapshot.messages {
                let key = MessageKey {
                    chat,
                    message_id: message.message_id,
                };
                for attachment in txn
                    .attachments_of_message(&key)
                    .map_err(|_| SessionFailure::STORAGE)?
                {
                    let canonical =
                        ItemKey::Canonical(CanonicalKey::Attachment(attachment.facts.key)).id();
                    if let Some(appearance) = txn
                        .appearances_of(&canonical)
                        .map_err(|_| SessionFailure::STORAGE)?
                        .into_iter()
                        .find(|item| item.deleted_at_ms.is_none())
                    {
                        media_names.insert(attachment.facts.key, appearance.safe_name);
                    }
                }
            }
            (snapshot, media_names)
        };
        let decoder = NormalizedMessageDecoder {
            scope: chat.scope,
            media_names,
        };
        let rendered =
            compose_month(&snapshot, year, month, &decoder).map_err(|_| SessionFailure::RENDER)?;
        let staged =
            stage_month(cache_root, &snapshot, &rendered).map_err(|_| SessionFailure::RENDER)?;
        match publish_month(store, &snapshot, &rendered, &staged, published_at_ms) {
            Ok(_) => published = true,
            Err(RenderPipelineError::PublicationLeased) => continue,
            Err(_) => return Err(SessionFailure::RENDER),
        }
    }
    Ok(published)
}

fn content_progress(
    phase: ChatContentPhase,
    category: Option<&str>,
    retryable: bool,
    attempt_count: u32,
    retry_at_ms: Option<i64>,
) -> ChatContentProgressRecord {
    ChatContentProgressRecord {
        phase,
        failure_category: category.map(str::to_owned),
        retryable,
        retry_at_ms,
        attempt_count,
        updated_at_ms: now_ms(),
    }
}

fn full_live_recovery_pending(progress: Option<&ChatContentProgressRecord>) -> bool {
    progress.is_some_and(|progress| {
        progress.phase == ChatContentPhase::Degraded
            && matches!(
                progress.failure_category.as_deref(),
                Some("live-edit-pending" | "live-refresh-overflow")
            )
    })
}

fn put_content_progress(
    store: &mut StateStore,
    chat: ChatKey,
    progress: ChatContentProgressRecord,
) -> Result<(), SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    txn.put_chat_content_progress(&chat, &progress)
        .map_err(|_| SessionFailure::STORAGE)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

/// Picks the next chat to crawl and opens its turn, or reports that there is
/// no history work right now (BUG-260728-2qfzbd).
///
/// Selection and the turn belong together: a chat that is offered but whose
/// turn is not recorded is a chat the very next plan offers again, so one
/// chat that cannot make progress would hold the head of the queue and
/// nothing behind it would ever run. That is why the turn is stamped here,
/// where the work is handed out, and not where a page commits — a turn spent
/// on a spacing wait, a source error, or an empty page is still a turn.
fn open_next_history_turn(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    scheduler: &BackfillScheduler,
    demand: &Mutex<ContentDemandState>,
    at_ms: i64,
) -> Result<Option<(ChatId, BackfillPriority)>, SessionFailure> {
    let plan = demand
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .scheduling_snapshot();
    let step = scheduler
        .plan_next(
            store,
            scope,
            BackfillDemand {
                visible: &plan.visible,
                requested: &plan.requested,
            },
            HostConditions::UNCONSTRAINED,
            at_ms,
        )
        .map_err(|_| SessionFailure::STORAGE)?;
    settle_admissions(
        &mut demand.lock().unwrap_or_else(|error| error.into_inner()),
        step,
        plan.watermark,
    );
    let BackfillStep::AdvanceHistory { chat_id, priority } = step else {
        return Ok(None);
    };
    begin_history_turn(store, ChatKey { scope, chat_id }, at_ms)?;
    Ok(Some((chat_id, priority)))
}

/// Exercises the production history scheduler with two ordinary background
/// chats. This is test-only composition of the scheduler seam, not a second
/// orchestration API: the returned ids are the durable turns it actually
/// opened. Hydration-runtime saturation coverage runs this concurrently to
/// prove that the namespace's independent crawl owner remains live and
/// round-robins rather than being held behind demand I/O.
#[cfg(test)]
pub(crate) fn test_background_history_fairness_turns(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    at_ms: i64,
) -> (i64, i64) {
    let second_chat_id = 200;
    let txn = store
        .write_txn()
        .expect("history-fairness setup transaction");
    txn.upsert_chat(
        &snapshot_chat_record(
            scope,
            &gramdrive_source_tdjson::ChatSnapshot {
                chat_id: second_chat_id,
                kind: SnapshotChatKind::Private,
                title: "Background fairness peer".to_owned(),
                username: None,
                is_protected: false,
            },
        )
        .expect("history-fairness peer record"),
    )
    .expect("history-fairness peer chat");
    let main = ChatListKey {
        scope,
        kind: ChatListKind::Main,
    };
    txn.upsert_chat_list_entry(
        &main,
        &ChatListEntry {
            chat_id: ChatId(100),
            sort_order: 2,
            pinned: false,
        },
    )
    .expect("history-fairness primary membership");
    txn.upsert_chat_list_entry(
        &main,
        &ChatListEntry {
            chat_id: ChatId(second_chat_id),
            sort_order: 1,
            pinned: false,
        },
    )
    .expect("history-fairness peer membership");
    txn.commit().expect("history-fairness setup commit");

    initialize_content_progress(store, scope).expect("history-fairness progress setup");
    let scheduler = BackfillScheduler::with_defaults();
    let demand = Mutex::new(ContentDemandState::default());
    let first = open_next_history_turn(store, scope, &scheduler, &demand, at_ms)
        .expect("first background history turn")
        .expect("first runnable background chat");
    let second = open_next_history_turn(store, scope, &scheduler, &demand, at_ms + 1)
        .expect("second background history turn")
        .expect("second runnable background chat");
    assert_eq!(first.1, BackfillPriority::Background);
    assert_eq!(second.1, BackfillPriority::Background);
    assert_ne!(
        first.0, second.0,
        "the scheduler must give both pending background chats a bounded turn"
    );
    (first.0.0, second.0.0)
}

/// Retires the foreground admissions this one plan actually consumed.
///
/// An admission is spent only where the scheduler demonstrably looked at it —
/// either it gave that chat the turn, or it reached past that entire demand
/// tier having found nothing there that still needs history. A plan that never
/// reached the demand lists at all (paused, paced, offline) spends nothing,
/// which is what keeps a hint that arrived during a flood wait alive until the
/// wait clears. `watermark` is the ledger position the plan was handed, so a
/// hint that arrived while it was running survives it too.
fn settle_admissions(demand: &mut ContentDemandState, step: BackfillStep, watermark: u64) {
    match step {
        // The plan returned before it read either demand list.
        BackfillStep::Paused
        | BackfillStep::Wait { .. }
        | BackfillStep::Idle {
            reason: IdleReason::Offline,
        } => {}
        // Visible work was found: only that chat is known to have been reached,
        // because the visible walk stops at the first chat that needs history.
        BackfillStep::AdvanceHistory {
            chat_id,
            priority: BackfillPriority::Visible,
        } => demand.spend(chat_id.0),
        // Reaching requested work proves the whole visible list was walked and
        // none of it still needed history.
        BackfillStep::AdvanceHistory {
            chat_id,
            priority: BackfillPriority::Requested,
        } => {
            demand.spend_visible(watermark);
            demand.spend(chat_id.0);
        }
        // Reaching the background backlog, or any post-demand idle, proves both
        // foreground tiers were walked to the end.
        BackfillStep::AdvanceHistory {
            priority: BackfillPriority::Background,
            ..
        }
        | BackfillStep::Idle { .. } => demand.spend_all(watermark),
    }
}

/// Records that a chat has been handed a history turn and reports it syncing,
/// in one transaction (BUG-260728-2qfzbd).
fn begin_history_turn(
    store: &mut StateStore,
    chat: ChatKey,
    at_ms: i64,
) -> Result<(), SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    txn.record_backfill_turn(&chat, at_ms)
        .map_err(|_| SessionFailure::STORAGE)?;
    txn.put_chat_content_progress(
        &chat,
        &content_progress(ChatContentPhase::Syncing, None, false, 0, None),
    )
    .map_err(|_| SessionFailure::STORAGE)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

/// Atomically invalidates a chat's contiguous window after bounded targeted
/// edit recovery overflows. Normalized rows already committed remain
/// idempotent; a relaunch sees the absent window and anchors a full crawl,
/// which is the only truthful way to re-observe edits whose individual ids
/// could not be retained.
fn require_full_live_recovery(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    chat_id: i64,
) -> Result<(), SessionFailure> {
    require_full_live_recovery_for(store, scope, chat_id, "live-refresh-overflow")
}

/// Persists the conservative crash-recovery obligation for one edit before
/// the corresponding targeted refresh exists only in `LiveMachine` memory.
/// Unknown chats already relaunch without a window, and protected/deleted
/// chats must not be scheduled for content access.
fn persist_edit_recovery_at_intake(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    update: &serde_json::Value,
) -> Result<(), SessionFailure> {
    let update_type = update.get("@type").and_then(serde_json::Value::as_str);
    if !matches!(
        update_type,
        Some("updateMessageContent" | "updateMessageEdited")
    ) {
        return Ok(());
    }
    let Some(message_id) = update.get("message_id").and_then(serde_json::Value::as_i64) else {
        return Ok(());
    };
    let Some(chat_id) = update.get("chat_id").and_then(serde_json::Value::as_i64) else {
        return Ok(());
    };
    let chat = ChatKey {
        scope,
        chat_id: ChatId(chat_id),
    };
    let needs_recovery = {
        let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
        let Some(record) = txn.chat(&chat).map_err(|_| SessionFailure::STORAGE)? else {
            return Ok(());
        };
        if record.is_protected || record.deleted_at_ms.is_some() {
            return Ok(());
        }
        if update_type == Some("updateMessageEdited")
            && let Some(edit_date_ms) = update
                .get("edit_date")
                .and_then(serde_json::Value::as_i64)
                .and_then(|seconds| seconds.checked_mul(1_000))
            && txn
                .message(&MessageKey {
                    chat,
                    message_id: MessageId(message_id),
                })
                .map_err(|_| SessionFailure::STORAGE)?
                .and_then(|message| message.edited_at_ms)
                .is_some_and(|stored| stored >= edit_date_ms)
        {
            // TDLib replays updateMessageEdited during authorization. When
            // the same or a newer revision is already durable, resetting the
            // account-history cursor would move relaunch progress backwards.
            return Ok(());
        }
        txn.chat_sync_state(&chat)
            .map_err(|_| SessionFailure::STORAGE)?
            .is_some_and(|sync| sync.window.is_some() || sync.history_complete)
    };
    if needs_recovery {
        require_full_live_recovery_for(store, scope, chat_id, "live-edit-pending")?;
    }
    Ok(())
}

fn require_full_live_recovery_for(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    chat_id: i64,
    category: &'static str,
) -> Result<(), SessionFailure> {
    let chat = ChatKey {
        scope,
        chat_id: ChatId(chat_id),
    };
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    txn.put_chat_content_progress(
        &chat,
        &content_progress(ChatContentPhase::Degraded, Some(category), true, 1, None),
    )
    .map_err(|_| SessionFailure::STORAGE)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

/// Lands normalized updates for already-durable chats while the namespace is
/// still becoming ready. It deliberately does not consume unresolved reports
/// or start TDLib requests; those remain queued for the post-snapshot content
/// loop, while known-chat observations and deletions stay crash-safe and
/// bounded in memory.
fn drain_pre_ready_live(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    live: &mut LiveMachine,
) -> Result<(), SessionFailure> {
    while let Some(commit) = live.take_ready_commit() {
        apply_live_commit(store, scope, &commit, now_ms())?;
    }
    Ok(())
}

/// Routes one live update through the bounded intake path, landing spill
/// commits immediately and then draining the remainder. This is used anywhere
/// the owned session must keep consuming TDLib updates while another response
/// is pending; the provider is never involved.
fn route_content_live_update(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    live: &mut LiveMachine,
    update: &serde_json::Value,
) -> Result<(), SessionFailure> {
    // Edit signals carry no complete message. Commit a conservative crawl
    // obligation before retaining their targeted refresh only in memory, so
    // a crash in any response-wait path cannot leave a stale complete window.
    persist_edit_recovery_at_intake(store, scope, update)?;
    live.on_update_bounded(update, |commit| {
        apply_live_commit(store, scope, &commit, now_ms())
    })?;
    drain_pre_ready_live(store, scope, live)
}

#[allow(clippy::too_many_arguments)]
fn wait_until_authorized(
    auth: &mut AuthMachine,
    folders: &mut FolderCatalogMachine,
    live: &mut UpdateMachine,
    content_live: &mut LiveMachine,
    stories: &mut StoryMachine,
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    client: &TdClient,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
) -> Result<(), SessionFailure> {
    let deadline = std::time::Instant::now() + REQUEST_TIMEOUT;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(SessionFailure::SOURCE);
        }
        let update = match updates.recv_timeout(UPDATE_POLL) {
            Ok(update) => update,
            Err(UpdateRecvError::Timeout) => continue,
            Err(UpdateRecvError::Closed) => return Err(SessionFailure::SOURCE),
        };
        folders.on_update(&update);
        live.on_update(&update);
        route_content_live_update(store, scope, content_live, &update)?;
        stories.on_update(&update);
        // Authorization state is the authoritative readiness signal. TDLib
        // can replay a malformed/obsolete auth update or answer a plumbing
        // command with an error while continuing to a later definitive state;
        // the established authorization probe deliberately tolerates both.
        // Keep waiting inside the deadline instead of turning that transient
        // response into a false terminal namespace failure.
        let step = match auth.on_update(&update) {
            Ok(step) => step,
            Err(_) => continue,
        };
        for request in step.requests {
            if let Ok(pending) = client.request(request) {
                drop(pending.wait_timeout(REQUEST_TIMEOUT));
            }
        }
        match step.entered {
            Some(AuthState::Ready) => return Ok(()),
            Some(
                AuthState::WaitPhoneNumber
                | AuthState::WaitCode(_)
                | AuthState::WaitQrConfirmation { .. }
                | AuthState::WaitPassword(_)
                | AuthState::Unsupported { .. },
            ) => return Err(SessionFailure::AUTH),
            Some(AuthState::Closed) => return Err(SessionFailure::SOURCE),
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn wait_for_folder_catalog(
    folders: &mut FolderCatalogMachine,
    live: &mut UpdateMachine,
    content_live: &mut LiveMachine,
    stories: &mut StoryMachine,
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
) -> Result<(), SessionFailure> {
    let deadline = std::time::Instant::now() + REQUEST_TIMEOUT;
    while !folders.has_observed_catalog() {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(SessionFailure::FOLDER_CATALOG);
        }
        match updates.recv_timeout(UPDATE_POLL) {
            Ok(update) => {
                folders.on_update(&update);
                live.on_update(&update);
                route_content_live_update(store, scope, content_live, &update)?;
                stories.on_update(&update);
            }
            Err(UpdateRecvError::Timeout) => {}
            Err(UpdateRecvError::Closed) => return Err(SessionFailure::FOLDER_CATALOG),
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_snapshot_cycle(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    folders: &mut FolderCatalogMachine,
    live: &mut UpdateMachine,
    content: &mut ContentCoordinator,
    client: &TdClient,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
    listener: &Arc<dyn NamespaceProgressListener>,
    hydrator: &Hydrator,
) -> Result<(), SessionFailure> {
    let lists: Vec<ChatListKind> = [ChatListKind::Main, ChatListKind::Archive]
        .into_iter()
        .chain(folders.folders().into_iter().map(ChatListKind::Folder))
        .collect();
    let plan = SnapshotPlan::new(lists);
    let checkpoint = {
        let txn = store.read_txn().map_err(|_| SessionFailure {
            category: "snapshot-checkpoint-storage",
            retryable: true,
        })?;
        txn.namespace_bootstrap(scope).map_err(|_| SessionFailure {
            category: "snapshot-checkpoint-storage",
            retryable: true,
        })?
    };
    let mut machine = match checkpoint {
        Some(checkpoint) => SnapshotMachine::resume(plan.clone(), &checkpoint.resume_token)
            .or_else(|_| SnapshotMachine::new(plan))
            .map_err(|_| SessionFailure {
                category: "snapshot-machine-storage",
                retryable: true,
            })?,
        None => SnapshotMachine::new(plan).map_err(|_| SessionFailure {
            category: "snapshot-machine-storage",
            retryable: true,
        })?,
    };

    loop {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        match machine.next_step().map_err(snapshot_failure)? {
            SnapshotStep::Submit(request) => {
                let pending = client
                    .request(request)
                    .map_err(|_| SessionFailure::SOURCE)?;
                let outcome = wait_for_snapshot_response(
                    store,
                    scope,
                    pending,
                    &mut machine,
                    folders,
                    live,
                    Some(&mut content.live),
                    Some(&mut content.stories),
                    updates,
                    cancelled,
                )
                .map_err(|failure| failure.storage_stage("snapshot-response-storage"))?;
                machine.on_response(outcome).map_err(snapshot_failure)?;
            }
            SnapshotStep::Backoff(backoff) => {
                if backoff.attempt > MAX_RETRY_ATTEMPTS {
                    return Err(SessionFailure::RATE_LIMITED);
                }
                let duration = Duration::from_secs(backoff.retry_after_secs.unwrap_or(1).min(30));
                let deadline = std::time::Instant::now() + duration;
                while std::time::Instant::now() < deadline {
                    if cancelled.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            SnapshotStep::Commit(commit) => {
                apply_snapshot_commit(store, scope, &commit)
                    .map_err(|failure| failure.storage_stage("snapshot-commit-storage"))?;
                apply_live_batch(
                    store,
                    scope,
                    folders,
                    live,
                    Some(&mut content.live),
                    Some(&mut content.stories),
                    SessionIo {
                        client,
                        updates,
                        cancelled,
                    },
                )
                .map_err(|failure| failure.storage_stage("snapshot-live-batch-storage"))?;
                drive_live_steps(
                    store, scope, folders, live, content, client, updates, cancelled, listener,
                )
                .map_err(|failure| failure.storage_stage("snapshot-live-content-storage"))?;
                purge_disallowed_materializations(hydrator, scope.account)
                    .map_err(|failure| failure.storage_stage("snapshot-cache-policy-storage"))?;
            }
            SnapshotStep::Done => {
                let txn = store.write_txn().map_err(|_| SessionFailure {
                    category: "snapshot-checkpoint-storage",
                    retryable: true,
                })?;
                reconcile_projection_scope_txn(&txn, scope, None, ProjectionDepth::Shallow)
                    .map_err(|failure| failure.storage_stage("snapshot-projection-storage"))?;
                txn.clear_namespace_bootstrap(scope)
                    .map_err(|_| SessionFailure {
                        category: "snapshot-checkpoint-storage",
                        retryable: true,
                    })?;
                txn.publish_namespace_readiness(scope, now_ms())
                    .map_err(|_| SessionFailure {
                        category: "namespace-readiness-storage",
                        retryable: true,
                    })?;
                txn.commit().map_err(|_| SessionFailure {
                    category: "snapshot-checkpoint-storage",
                    retryable: true,
                })?;
                listener.on_progress(
                    namespace_counts(store, scope)
                        .map_err(|failure| failure.storage_stage("snapshot-count-storage"))?,
                );
                return Ok(());
            }
        }
    }
}

/// Waits for one snapshot response while continuously draining its update
/// stream. `loadChats` can emit more than the bounded update queue's capacity
/// before its response; blocking only on the response would then deadlock the
/// TDLib receive thread behind its own backpressure until the timeout.
#[allow(clippy::too_many_arguments)]
fn wait_for_snapshot_response(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    mut pending: gramdrive_source_tdjson::PendingRequest,
    machine: &mut SnapshotMachine,
    folders: &mut FolderCatalogMachine,
    live: &mut UpdateMachine,
    mut content_live: Option<&mut LiveMachine>,
    mut stories: Option<&mut StoryMachine>,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
) -> Result<Result<serde_json::Value, gramdrive_source_tdjson::TdError>, SessionFailure> {
    let deadline = std::time::Instant::now() + REQUEST_TIMEOUT;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(SessionFailure::SOURCE);
        }
        while let Ok(update) = updates.try_recv() {
            folders.on_update(&update);
            machine.on_update(&update);
            live.on_update(&update);
            if let Some(content_live) = content_live.as_deref_mut() {
                route_content_live_update(store, scope, content_live, &update)?;
            }
            if let Some(stories) = stories.as_deref_mut() {
                stories.on_update(&update);
            }
        }
        match pending.wait_timeout(Duration::from_millis(10)) {
            Ok(outcome) => return Ok(outcome),
            Err(still_pending) => pending = still_pending,
        }
        if std::time::Instant::now() >= deadline {
            return Err(SessionFailure::SOURCE);
        }
    }
}

fn snapshot_failure(error: SnapshotError) -> SessionFailure {
    match error {
        SnapshotError::Request { request, .. } => match request {
            gramdrive_source_tdjson::SnapshotRequest::LoadChats => SessionFailure::SNAPSHOT_LOAD,
            gramdrive_source_tdjson::SnapshotRequest::GetChats => SessionFailure::SNAPSHOT_LIST,
            gramdrive_source_tdjson::SnapshotRequest::GetChat => SessionFailure::SNAPSHOT_CHAT,
        },
        _ => SessionFailure::STORAGE,
    }
}

fn apply_snapshot_commit(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    commit: &ListCommit,
) -> Result<(), SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    let protected_at_ms = now_ms();
    let mut protected_chats = Vec::new();
    let mut unprotected_chats = Vec::new();
    for chat in &commit.chats {
        let key = ChatKey {
            scope,
            chat_id: ChatId(chat.chat_id),
        };
        let was_protected = txn
            .read()
            .chat(&key)
            .map_err(|_| SessionFailure {
                category: "snapshot-read-chat-storage",
                retryable: true,
            })?
            .is_some_and(|record| record.is_protected);
        txn.upsert_chat(&snapshot_chat_record(scope, chat)?)
            .map_err(|_| SessionFailure {
                category: "snapshot-upsert-chat-storage",
                retryable: true,
            })?;
        if chat.is_protected {
            txn.protect_chat_stories(&key, protected_at_ms)
                .map_err(|_| SessionFailure {
                    category: "snapshot-protect-stories-storage",
                    retryable: true,
                })?;
            protected_chats.push(key);
        } else if was_protected {
            txn.restart_story_chat_scan(&key, protected_at_ms)
                .map_err(|_| SessionFailure {
                    category: "snapshot-restart-story-storage",
                    retryable: true,
                })?;
            unprotected_chats.push(key);
        }
    }
    let list = ChatListKey {
        scope,
        kind: commit.list,
    };
    let entries: Vec<_> = commit
        .entries
        .iter()
        .map(|entry| ChatListEntry {
            chat_id: ChatId(entry.chat_id),
            sort_order: entry.sort_order,
            pinned: entry.pinned,
        })
        .collect();
    txn.replace_chat_list_with_audit(&list, &entries, true, now_ms())
        .map_err(|error| match error {
            // Do not save the list's resume token when its proposed shrink
            // lacks a positive Telegram departure witness. Dropping this
            // transaction preserves both the prior membership and its
            // checkpoint, so recovery retries rather than publishing a
            // false Finder disappearance.
            StateError::UnsafeChatListShrink { .. } => SessionFailure {
                category: "snapshot-membership-incomplete",
                retryable: true,
            },
            _ => SessionFailure {
                category: "snapshot-replace-list-storage",
                retryable: true,
            },
        })?;
    txn.put_namespace_bootstrap(&NamespaceBootstrapRecord {
        scope,
        resume_token: commit.resume_token.clone(),
        updated_at_ms: now_ms(),
    })
    .map_err(|_| SessionFailure {
        category: "snapshot-save-checkpoint-storage",
        retryable: true,
    })?;
    for chat in protected_chats {
        enforce_chat_protection_txn(&txn, chat, protected_at_ms)
            .map_err(|failure| failure.storage_stage("snapshot-enforce-protection-storage"))?;
    }
    for chat in unprotected_chats {
        release_chat_protection_txn(&txn, chat)
            .map_err(|failure| failure.storage_stage("snapshot-release-protection-storage"))?;
    }
    txn.commit().map_err(|_| SessionFailure {
        category: "snapshot-commit-transaction-storage",
        retryable: true,
    })
}

fn snapshot_chat_record(
    scope: gramdrive_model::identity::AccountScope,
    chat: &gramdrive_source_tdjson::ChatSnapshot,
) -> Result<ChatRecord, SessionFailure> {
    Ok(ChatRecord {
        key: ChatKey {
            scope,
            chat_id: ChatId(chat.chat_id),
        },
        chat_type: chat_type(chat.kind),
        title: chat.title.clone(),
        username: chat.username.clone(),
        is_protected: chat.is_protected,
        archive_mode: false,
        metadata_version: chat_version(
            chat.chat_id,
            chat.kind,
            &chat.title,
            chat.username.as_deref(),
            chat.is_protected,
        )?,
        left_at_ms: None,
        deleted_at_ms: None,
        last_update_at_ms: None,
    })
}

fn chat_type(kind: SnapshotChatKind) -> ChatType {
    match kind {
        SnapshotChatKind::Private => ChatType::Private,
        SnapshotChatKind::Group => ChatType::Group,
        SnapshotChatKind::Supergroup => ChatType::Supergroup,
        SnapshotChatKind::Channel => ChatType::Channel,
    }
}

fn apply_live_batch(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    folders: &mut FolderCatalogMachine,
    live: &mut UpdateMachine,
    content_live: Option<&mut LiveMachine>,
    stories: Option<&mut StoryMachine>,
    io: SessionIo<'_>,
) -> Result<bool, SessionFailure> {
    let mut batch = live.take_batch();
    if !batch.unresolved.is_empty() {
        resolve_gaps(
            store,
            scope,
            folders,
            live,
            content_live,
            stories,
            io.client,
            io.updates,
            io.cancelled,
            &batch.unresolved,
        )?;
        let resolved = live.take_batch();
        batch.chats.extend(resolved.chats);
        batch.memberships.extend(resolved.memberships);
    }
    if batch.chats.is_empty() && batch.memberships.is_empty() {
        return Ok(false);
    }
    apply_update_batch(store, scope, &batch)
}

fn purge_disallowed_materializations(
    hydrator: &Hydrator,
    account: gramdrive_model::identity::AccountKey,
) -> Result<(), SessionFailure> {
    hydrator
        .purge_disallowed_attachment_materializations(account)
        .map_err(|_| SessionFailure::STORAGE)?;
    hydrator
        .purge_disallowed_story_materializations(account)
        .map_err(|_| SessionFailure::STORAGE)?;
    hydrator
        .resume_retention_purge(account)
        .map_err(|_| SessionFailure::STORAGE)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn resolve_gaps(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    folders: &mut FolderCatalogMachine,
    live: &mut UpdateMachine,
    mut content_live: Option<&mut LiveMachine>,
    mut stories: Option<&mut StoryMachine>,
    client: &TdClient,
    updates: &UpdateStream,
    cancelled: &AtomicBool,
    unresolved: &[i64],
) -> Result<(), SessionFailure> {
    for &chat_id in unresolved {
        if cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut pending = client
            .request(json!({"@type": "getChat", "chat_id": chat_id}))
            .map_err(|_| SessionFailure::SOURCE)?;
        let deadline = std::time::Instant::now() + REQUEST_TIMEOUT;
        let chat = loop {
            while let Ok(update) = updates.try_recv() {
                folders.on_update(&update);
                live.on_update(&update);
                if let Some(content_live) = content_live.as_deref_mut() {
                    route_content_live_update(store, scope, content_live, &update)?;
                }
                if let Some(stories) = stories.as_deref_mut() {
                    stories.on_update(&update);
                }
            }
            match pending.wait_timeout(Duration::from_millis(10)) {
                Ok(outcome) => break outcome.map_err(|_| SessionFailure::SOURCE)?,
                Err(still_pending) => pending = still_pending,
            }
            if cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(SessionFailure::SOURCE);
            }
        };
        live.on_update(&json!({"@type": "updateNewChat", "chat": chat}));
    }
    Ok(())
}

fn apply_update_batch(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    batch: &UpdateBatch,
) -> Result<bool, SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    let protected_at_ms = now_ms();
    let mut protected_chats = Vec::new();
    let mut unprotected_chats = Vec::new();
    let mut projection_changed = false;
    let mut provider_changed = false;
    for chat in &batch.chats {
        let key = ChatKey {
            scope,
            chat_id: ChatId(chat.chat_id),
        };
        let existing = txn.read().chat(&key).map_err(|_| SessionFailure::STORAGE)?;
        let record = live_chat_record(scope, chat, existing.as_ref())?;
        let chat_changed = existing.as_ref() != Some(&record);
        let listed = txn
            .read()
            .chat_has_list_membership(&key)
            .map_err(|_| SessionFailure::STORAGE)?;
        projection_changed |= chat_changed && listed;
        provider_changed |= chat_changed && listed;
        txn.upsert_chat(&record)
            .map_err(|_| SessionFailure::STORAGE)?;
        if chat.is_protected {
            txn.protect_chat_stories(&key, protected_at_ms)
                .map_err(|_| SessionFailure::STORAGE)?;
            protected_chats.push(key);
        } else if existing.is_some_and(|record| record.is_protected) {
            txn.restart_story_chat_scan(&key, protected_at_ms)
                .map_err(|_| SessionFailure::STORAGE)?;
            unprotected_chats.push(key);
        }
    }
    let mut existing_lists: HashMap<ChatListKind, HashMap<i64, ChatListEntry>> = HashMap::new();
    for membership in &batch.memberships {
        let list_kind = match membership {
            MembershipChange::Set { list, .. } | MembershipChange::Removed { list, .. } => *list,
        };
        let entries = match existing_lists.entry(list_kind) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let entries = txn
                    .read()
                    .chat_list(&ChatListKey {
                        scope,
                        kind: list_kind,
                    })
                    .map_err(|_| SessionFailure::STORAGE)?
                    .into_iter()
                    .map(|entry| (entry.chat_id.0, entry))
                    .collect();
                entry.insert(entries)
            }
        };
        match membership {
            MembershipChange::Set {
                list,
                chat_id,
                sort_order,
                pinned,
            } => {
                let entry = ChatListEntry {
                    chat_id: ChatId(*chat_id),
                    sort_order: *sort_order,
                    pinned: *pinned,
                };
                let existing = entries.get(chat_id).copied();
                if existing != Some(entry) {
                    provider_changed = true;
                    projection_changed |= existing.is_none();
                }
                txn.upsert_chat_list_entry(&ChatListKey { scope, kind: *list }, &entry)
                    .map_err(|_| SessionFailure::STORAGE)?;
                entries.insert(*chat_id, entry);
            }
            MembershipChange::Removed { list, chat_id } => {
                // A live position stream is not a complete membership
                // snapshot. During startup/backfill TDLib can transiently
                // report an order-0 position before the stable list arrives;
                // treating that absence as authoritative used to remove the
                // final membership and let account-wide reconciliation
                // tombstone the whole Finder subtree. Only the canonical
                // chat's durable departure/deletion marker is a positive
                // witness for destructive removal. Missing chat state is
                // unknown, not absence, and therefore also fails closed.
                let departure_witnessed = txn
                    .read()
                    .chat(&ChatKey {
                        scope,
                        chat_id: ChatId(*chat_id),
                    })
                    .map_err(|_| SessionFailure::STORAGE)?
                    .is_some_and(|chat| chat.left_at_ms.is_some() || chat.deleted_at_ms.is_some());
                if !departure_witnessed {
                    continue;
                }
                let existed = entries.remove(chat_id).is_some();
                provider_changed |= existed;
                projection_changed |= existed;
                txn.remove_chat_list_entry(&ChatListKey { scope, kind: *list }, ChatId(*chat_id))
                    .map_err(|_| SessionFailure::STORAGE)?;
            }
        }
    }
    for chat in protected_chats {
        enforce_chat_protection_txn(&txn, chat, protected_at_ms)?;
    }
    for chat in unprotected_chats {
        release_chat_protection_txn(&txn, chat)?;
    }
    if projection_changed {
        reconcile_projection_txn(&txn, scope)?;
    }
    txn.commit().map_err(|_| SessionFailure::STORAGE)?;
    Ok(provider_changed)
}

/// Enforces a chat-level Telegram restriction inside the metadata transaction.
///
/// Event identity stays as a minimal sync tombstone, while every stored body,
/// attachment byte owner, generated render publication, cache entry, pin, and
/// provider-visible availability is revoked together. Physical files are
/// journalled for crash-idempotent deletion after commit.
fn enforce_chat_protection_txn(
    txn: &gramdrive_state::WriteTxn<'_>,
    chat: ChatKey,
    protected_at_ms: i64,
) -> Result<(), SessionFailure> {
    txn.purge_restricted_chat_message_content(&chat)
        .map_err(|_| SessionFailure::STORAGE)?;

    let mut attachment_keys: Vec<AttachmentKey> = txn
        .read()
        .attachment_projections_of_chat(&chat)
        .map_err(|_| SessionFailure::STORAGE)?
        .into_iter()
        .map(|projection| projection.attachment.facts.key)
        .collect();
    for key in txn
        .read()
        .retained_attachment_keys(chat.scope.account)
        .map_err(|_| SessionFailure::STORAGE)?
        .into_iter()
        .filter(|key| key.message.chat == chat)
    {
        if !attachment_keys.contains(&key) {
            attachment_keys.push(key);
        }
    }
    for key in attachment_keys {
        let canonical = ItemKey::Canonical(CanonicalKey::Attachment(key)).id();
        let mut items = vec![canonical.clone()];
        items.extend(
            txn.read()
                .appearances_of(&canonical)
                .map_err(|_| SessionFailure::STORAGE)?
                .into_iter()
                .map(|item| item.id),
        );
        for item in items {
            txn.queue_restricted_cache_purge(chat.scope.account, &item, protected_at_ms)
                .map_err(|_| SessionFailure::STORAGE)?;
        }
        txn.queue_retained_attachment_purge(chat.scope.account, &canonical, protected_at_ms)
            .map_err(|_| SessionFailure::STORAGE)?;
        txn.unlink_attachment_blob(&key)
            .map_err(|_| SessionFailure::STORAGE)?;
    }
    txn.redact_protected_chat_attachments(&chat)
        .map_err(|_| SessionFailure::STORAGE)?;

    skip_policy_excluded_documents_txn(txn, chat, protected_at_ms)?;
    txn.purge_unreferenced_blobs(chat.scope.account)
        .map_err(|_| SessionFailure::STORAGE)?;
    Ok(())
}

/// Removes every generated document of a protected chat from the dirty
/// worklist, while retaining a durable policy-excluded record for accounting.
///
/// This is used both by the protection transition and by the render drain:
/// the latter repairs rows created after the transition (or recovered from an
/// older installed profile) before they can consume every bounded tick.
fn skip_policy_excluded_documents_txn(
    txn: &gramdrive_state::WriteTxn<'_>,
    chat: ChatKey,
    skipped_at_ms: i64,
) -> Result<(), SessionFailure> {
    for item_id in txn
        .read()
        .generated_document_items_of_chat(&chat)
        .map_err(|_| SessionFailure::STORAGE)?
    {
        let ItemKey::Appearance(AppearanceKey {
            item: CanonicalKey::GeneratedDoc(_),
            ..
        }) = item_id.key()
        else {
            continue;
        };
        txn.queue_restricted_cache_purge(chat.scope.account, &item_id, skipped_at_ms)
            .map_err(|_| SessionFailure::STORAGE)?;
        txn.skip_render_due_to_policy(&item_id, skipped_at_ms)
            .map_err(|_| SessionFailure::STORAGE)?;
        let Some(mut item) = txn
            .read()
            .item(&item_id)
            .map_err(|_| SessionFailure::STORAGE)?
        else {
            continue;
        };
        item.availability = ItemAvailability::Restricted;
        item.content = Some(FileFacts {
            mime_type: item
                .content
                .as_ref()
                .and_then(|facts| facts.mime_type.clone()),
            logical_size: None,
            content_version: None,
        });
        item.metadata_version =
            stable_version(format!("protected-message-render-v1|{}", item_id.text()).as_bytes())?;
        txn.upsert_item(&item)
            .map_err(|_| SessionFailure::STORAGE)?;
    }
    Ok(())
}

fn skip_policy_excluded_documents(
    store: &mut StateStore,
    chat: ChatKey,
    skipped_at_ms: i64,
) -> Result<(), SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    skip_policy_excluded_documents_txn(&txn, chat, skipped_at_ms)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

/// Re-enables only empty generated documents after chat protection is lifted.
///
/// Purged event bodies and attachment locators are intentionally irreversible;
/// a subsequent allowed Telegram observation may populate new state, but this
/// transition cannot resurrect pre-restriction plaintext or bytes.
fn release_chat_protection_txn(
    txn: &gramdrive_state::WriteTxn<'_>,
    chat: ChatKey,
) -> Result<(), SessionFailure> {
    for item_id in txn
        .read()
        .generated_document_items_of_chat(&chat)
        .map_err(|_| SessionFailure::STORAGE)?
    {
        let ItemKey::Appearance(AppearanceKey {
            item: CanonicalKey::GeneratedDoc(_),
            ..
        }) = item_id.key()
        else {
            continue;
        };
        let Some(mut item) = txn
            .read()
            .item(&item_id)
            .map_err(|_| SessionFailure::STORAGE)?
        else {
            continue;
        };
        item.availability = ItemAvailability::Fetchable;
        item.content = Some(FileFacts::default());
        item.metadata_version =
            stable_version(format!("released-message-render-v1|{}", item_id.text()).as_bytes())?;
        txn.upsert_item(&item)
            .map_err(|_| SessionFailure::STORAGE)?;
        txn.mark_render_dirty(&item_id)
            .map_err(|_| SessionFailure::STORAGE)?;
    }
    Ok(())
}

fn live_chat_record(
    scope: gramdrive_model::identity::AccountScope,
    chat: &ChatMetadata,
    existing: Option<&ChatRecord>,
) -> Result<ChatRecord, SessionFailure> {
    Ok(ChatRecord {
        key: ChatKey {
            scope,
            chat_id: ChatId(chat.chat_id),
        },
        chat_type: chat_type(chat.kind),
        title: chat.title.clone(),
        username: chat.username.clone(),
        is_protected: chat.is_protected,
        archive_mode: existing.is_some_and(|record| record.archive_mode),
        metadata_version: chat_version(
            chat.chat_id,
            chat.kind,
            &chat.title,
            chat.username.as_deref(),
            chat.is_protected,
        )?,
        left_at_ms: existing.and_then(|record| record.left_at_ms),
        deleted_at_ms: existing.and_then(|record| record.deleted_at_ms),
        last_update_at_ms: None,
    })
}

fn persist_folders(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    folders: &FolderCatalogMachine,
) -> Result<(), SessionFailure> {
    let records: Vec<_> = folders
        .definitions()
        .into_iter()
        .map(|folder| FolderRecord {
            scope,
            folder_id: folder.id,
            title: folder.title,
            position: folder.position,
        })
        .collect();
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    txn.replace_folders(scope, &records)
        .map_err(|_| SessionFailure::STORAGE)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

#[cfg(test)]
fn rebuild_shallow_projection(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
) -> Result<(), SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    reconcile_projection_scope_txn(&txn, scope, None, ProjectionDepth::Shallow)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

fn rebuild_projection(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
) -> Result<(), SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    reconcile_projection_txn(&txn, scope)?;
    // A preserved profile can already contain protected chats before this
    // version first opens it. Reconciliation may create their `.chat.json`
    // render rows after the original protection transition, so repair every
    // protected chat in the same transaction rather than waiting for those
    // lowest-watermark rows to consume bounded render ticks one quantum at a
    // time.
    let protected_chats = txn
        .read()
        .chats(scope)
        .map_err(|_| SessionFailure::STORAGE)?
        .into_iter()
        .filter(|chat| chat.is_protected && chat.deleted_at_ms.is_none())
        .map(|chat| chat.key)
        .collect::<Vec<_>>();
    let skipped_at_ms = now_ms();
    for chat in protected_chats {
        skip_policy_excluded_documents_txn(&txn, chat, skipped_at_ms)?;
    }
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

fn rebuild_chat_projection(store: &mut StateStore, chat: ChatKey) -> Result<(), SessionFailure> {
    let txn = store.write_txn().map_err(|_| SessionFailure::STORAGE)?;
    reconcile_chat_projection_txn(&txn, chat)?;
    txn.commit().map_err(|_| SessionFailure::STORAGE)
}

#[cfg(test)]
pub(crate) fn apply_story_commit_and_rebuild_for_test(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
    commit: &StoryCommit,
    observed_at_ms: i64,
) -> Result<(), &'static str> {
    apply_story_commit(store, scope, commit, observed_at_ms).map_err(|_| "story commit failed")?;
    rebuild_projection(store, scope).map_err(|_| "projection rebuild failed")
}

/// The first and last correspondence instant of one chat, and of each of its
/// civil-month partitions (BUG-260728-2qfzbd).
///
/// This is where a chat folder's Date Created and Date Modified come from.
/// Before it existed, every directory in the namespace carried a null
/// timestamp, which Finder renders as 1 Jan 1970 — a date that is not merely
/// ugly but *wrong*: the correspondence it names has a real beginning and a
/// real most-recent message, both already indexed. Deriving the dates from
/// the message index (rather than from when the folder happened to be
/// projected) also makes them idempotent: reconciling twice writes the same
/// two numbers, so a restart never churns the provider.
#[derive(Debug, Clone, Default)]
struct CorrespondenceBounds {
    /// Oldest and newest indexed instant anywhere in the chat.
    chat: Option<(i64, i64)>,
    /// When Telegram last said anything about the chat itself. Used only
    /// when the chat holds no indexed correspondence at all — a folder the
    /// crawler has not reached yet still has to be dated with something
    /// truthful, and the epoch is not it.
    metadata_fallback_ms: Option<i64>,
    /// Oldest and newest indexed instant inside each civil month.
    months: BTreeMap<MonthStamp, (i64, i64)>,
    /// Oldest and newest instant among the chat's *active* stories, which
    /// live outside the monthly partitions.
    active_stories: Option<(i64, i64)>,
}

impl CorrespondenceBounds {
    /// Folds one indexed instant known to belong to `stamp` into both the
    /// month bounds and the chat bounds.
    fn observe(&mut self, stamp: MonthStamp, instant_ms: i64) {
        extend(&mut self.chat, instant_ms);
        let month = self.months.entry(stamp).or_insert((instant_ms, instant_ms));
        month.0 = month.0.min(instant_ms);
        month.1 = month.1.max(instant_ms);
    }

    /// Folds one active-story instant in: it dates the `Active Stories`
    /// directory and counts toward the chat, but belongs to no month.
    fn observe_active_story(&mut self, instant_ms: i64) {
        extend(&mut self.chat, instant_ms);
        extend(&mut self.active_stories, instant_ms);
    }

    fn month(&self, stamp: MonthStamp) -> Option<(i64, i64)> {
        self.months.get(&stamp).copied()
    }
}

/// Widens an optional `(oldest, newest)` window to include `instant_ms`.
fn extend(window: &mut Option<(i64, i64)>, instant_ms: i64) {
    match window {
        Some((oldest, newest)) => {
            *oldest = (*oldest).min(instant_ms);
            *newest = (*newest).max(instant_ms);
        }
        None => *window = Some((instant_ms, instant_ms)),
    }
}

/// Everything the projection knows about one directory node that is not
/// derivable from its name: its correspondence dates and the exact logical
/// size of the indexed descendants below it (BUG-260728-2qfzbd).
///
/// `aggregate_size` is a sum of *known* descendant sizes. A descendant whose
/// size Telegram has not reported yet contributes zero rather than an
/// estimate — SYNC-032 keeps exact size a source fact, and a folder size
/// that silently mixes measurements with guesses is worse than one that
/// grows as the index does.
///
/// It is `None` for a directory that owns no rollup: a chat list or a folder
/// catalog holds chats, not correspondence, and summing them would publish a
/// number no product surface asks for over an unbounded child count. `None`
/// and `Some(0)` are deliberately different answers — "nothing is claimed
/// here" against "this subtree is indexed and holds no bytes" — and only the
/// kinds v16 backfills (chat, month, `Active Stories`) ever carry the latter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DirectoryFacts {
    created_at_ms: Option<i64>,
    modified_at_ms: Option<i64>,
    aggregate_size: Option<u64>,
}

#[derive(Debug, Clone)]
struct AttachmentItemMetadata {
    mime_type: Option<String>,
    exact_size: Option<u64>,
    content_version: ContentVersion,
    /// Stable facts known before tree naming. The provider metadata version
    /// is derived later, after sibling collision resolution supplies the
    /// final parent and safe name.
    metadata_seed: String,
    availability: ItemAvailability,
    telegram_message_timestamp_ms: i64,
}

#[derive(Debug, Clone)]
struct StoryItemMetadata {
    mime_type: Option<String>,
    exact_size: Option<u64>,
    content_version: ContentVersion,
    metadata_seed: String,
    availability: ItemAvailability,
    source_timestamp_ms: i64,
}

fn reconcile_projection_txn(
    txn: &gramdrive_state::WriteTxn<'_>,
    scope: gramdrive_model::identity::AccountScope,
) -> Result<(), SessionFailure> {
    reconcile_projection_scope_txn(txn, scope, None, ProjectionDepth::Deep)
}

fn reconcile_chat_projection_txn(
    txn: &gramdrive_state::WriteTxn<'_>,
    chat: ChatKey,
) -> Result<(), SessionFailure> {
    let canonical = ItemKey::Canonical(CanonicalKey::Chat(chat)).id();
    let stored_live_appearances = txn
        .read()
        .appearances_of(&canonical)
        .map_err(|_| SessionFailure {
            category: "projection-chat-appearance-storage",
            retryable: true,
        })?
        .into_iter()
        .filter(|appearance| appearance.deleted_at_ms.is_none())
        .map(|appearance| appearance.id)
        .collect::<HashSet<_>>();
    let folders = txn.read().folders(chat.scope).map_err(|_| SessionFailure {
        category: "projection-catalog-storage",
        retryable: true,
    })?;
    let mut expected_appearances = HashSet::new();
    for kind in [
        ChatListKind::Main,
        ChatListKind::Archive,
        ChatListKind::Stories,
    ]
    .into_iter()
    .chain(
        folders
            .iter()
            .map(|folder| ChatListKind::Folder(folder.folder_id)),
    ) {
        let is_member = txn
            .read()
            .chat_list(&ChatListKey {
                scope: chat.scope,
                kind,
            })
            .map_err(|_| SessionFailure {
                category: "projection-catalog-storage",
                retryable: true,
            })?
            .iter()
            .any(|entry| entry.chat_id == chat.chat_id);
        if is_member {
            expected_appearances.insert(
                ItemKey::Appearance(AppearanceKey {
                    view: kind,
                    item: CanonicalKey::Chat(chat),
                })
                .id(),
            );
        }
    }
    let projection_is_complete = expected_appearances
        .iter()
        .all(|appearance| stored_live_appearances.contains(appearance));
    if !projection_is_complete {
        return reconcile_projection_scope_txn(txn, chat.scope, None, ProjectionDepth::Deep);
    }
    reconcile_projection_scope_txn(txn, chat.scope, Some(chat.chat_id.0), ProjectionDepth::Deep)
}

fn reconcile_projection_scope_txn(
    txn: &gramdrive_state::WriteTxn<'_>,
    scope: gramdrive_model::identity::AccountScope,
    target_chat_id: Option<i64>,
    depth: ProjectionDepth,
) -> Result<(), SessionFailure> {
    let account = txn
        .read()
        .account(scope.account)
        .map_err(|_| SessionFailure {
            category: "projection-account-storage",
            retryable: true,
        })?
        .ok_or(SessionFailure {
            category: "projection-account-storage",
            retryable: true,
        })?;
    let folders = txn.read().folders(scope).map_err(|_| SessionFailure {
        category: "projection-catalog-storage",
        retryable: true,
    })?;
    let mut chats = txn.read().listed_chats(scope).map_err(|_| SessionFailure {
        category: "projection-catalog-storage",
        retryable: true,
    })?;
    // A chat may have lost its final list membership immediately before a
    // chat-scoped reconcile. Fall back to the listed-account diff in that
    // case so its former appearances are tombstoned; it is not a missing
    // canonical chat or a reason to traverse every unlisted source chat.
    let target_chat_id =
        target_chat_id.filter(|target| chats.iter().any(|chat| chat.key.chat_id.0 == *target));
    if let Some(target_chat_id) = target_chat_id {
        chats.retain(|chat| chat.key.chat_id.0 == target_chat_id);
    }
    let protected_chats: BTreeSet<i64> = chats
        .iter()
        .filter(|chat| chat.is_protected)
        .map(|chat| chat.key.chat_id.0)
        .collect();
    let timezone =
        DisplayTimeZone::named(&account.display_timezone).map_err(|_| SessionFailure {
            category: "projection-timezone-storage",
            retryable: true,
        })?;
    let mut memberships: BTreeMap<i64, Vec<ChatListKind>> = BTreeMap::new();
    for kind in [
        ChatListKind::Main,
        ChatListKind::Archive,
        ChatListKind::Stories,
    ]
    .into_iter()
    .chain(
        folders
            .iter()
            .map(|folder| ChatListKind::Folder(folder.folder_id)),
    ) {
        for entry in txn
            .read()
            .chat_list(&ChatListKey { scope, kind })
            .map_err(|_| SessionFailure {
                category: "projection-catalog-storage",
                retryable: true,
            })?
        {
            if target_chat_id.is_some_and(|target| entry.chat_id.0 != target) {
                continue;
            }
            memberships.entry(entry.chat_id.0).or_default().push(kind);
        }
    }
    let mut message_months = BTreeMap::new();
    let mut correspondence: BTreeMap<i64, CorrespondenceBounds> = BTreeMap::new();
    let mut tree_attachments: BTreeMap<i64, Vec<TreeAttachmentRecord>> = BTreeMap::new();
    let mut attachment_metadata = HashMap::new();
    let mut tree_stories: BTreeMap<i64, Vec<TreeStoryRecord>> = BTreeMap::new();
    let mut story_metadata = HashMap::new();
    for chat in &chats {
        if depth == ProjectionDepth::Shallow {
            message_months.insert(chat.key.chat_id.0, Vec::new());
            correspondence.insert(
                chat.key.chat_id.0,
                CorrespondenceBounds {
                    metadata_fallback_ms: chat.last_update_at_ms,
                    ..CorrespondenceBounds::default()
                },
            );
            tree_attachments.insert(chat.key.chat_id.0, Vec::new());
            tree_stories.insert(chat.key.chat_id.0, Vec::new());
            continue;
        }
        let mut months = BTreeSet::new();
        // One pass over the chat's distinct message instants answers both
        // questions the namespace has about time: which civil months exist,
        // and what the first and last correspondence instant is inside each
        // of them. The chat and month directories are dated from exactly
        // that — never from a projection clock, so a rebuild is idempotent
        // (BUG-260728-2qfzbd).
        let mut bounds = CorrespondenceBounds {
            metadata_fallback_ms: chat.last_update_at_ms,
            ..CorrespondenceBounds::default()
        };
        for instant in txn
            .read()
            .message_instants(&chat.key)
            .map_err(|_| SessionFailure {
                category: "projection-content-storage",
                retryable: true,
            })?
        {
            let (year, month) = gramdrive_engine::render::civil::year_month_in_timezone(
                instant,
                timezone.timezone(),
            );
            let stamp = MonthStamp {
                year: u16::try_from(year).map_err(|_| SessionFailure::STORAGE)?,
                month: u8::try_from(month).map_err(|_| SessionFailure::STORAGE)?,
            };
            months.insert(stamp);
            bounds.observe(stamp, instant);
        }
        message_months.insert(chat.key.chat_id.0, months.into_iter().collect::<Vec<_>>());
        correspondence.insert(chat.key.chat_id.0, bounds);
        let projections = txn
            .read()
            .attachment_projections_of_chat(&chat.key)
            .map_err(|_| SessionFailure {
                category: "projection-content-storage",
                retryable: true,
            })?;
        let mut records = Vec::with_capacity(projections.len());
        for projection in projections {
            let facts = &projection.attachment.facts;
            let (year, month) = gramdrive_engine::render::civil::year_month_in_timezone(
                projection.telegram_message_timestamp_ms,
                timezone.timezone(),
            );
            let month = MonthStamp {
                year: u16::try_from(year).map_err(|_| SessionFailure::STORAGE)?,
                month: u8::try_from(month).map_err(|_| SessionFailure::STORAGE)?,
            };
            let leaf = attachment_leaf_name(
                facts.logical_kind.tag(),
                facts.telegram_representation.tag(),
                facts.source_name.as_deref(),
                facts.mime_type.as_deref(),
            );
            let prefix = gramdrive_engine::render::civil::filename_timestamp_in_timezone(
                projection.telegram_message_timestamp_ms,
                timezone.timezone(),
            );
            let display_name = format!("{prefix} {}", leaf.as_str());
            let canonical = ItemKey::Canonical(CanonicalKey::Attachment(facts.key)).id();
            let metadata_seed = format!(
                "{}|{}|{}|{}|{}|{}|{}",
                display_name,
                facts.logical_kind.tag(),
                facts.telegram_representation.tag(),
                facts.fidelity.tag(),
                facts.mime_type.as_deref().unwrap_or(""),
                facts
                    .exact_size
                    .map_or(String::new(), |size| size.to_string()),
                projection.telegram_message_timestamp_ms,
            );
            let availability = if chat.is_protected {
                ItemAvailability::Restricted
            } else {
                match facts.availability {
                    StateAttachmentAvailability::Fetchable => ItemAvailability::Fetchable,
                    StateAttachmentAvailability::Restricted => ItemAvailability::Restricted,
                    StateAttachmentAvailability::Unavailable
                    | StateAttachmentAvailability::ViewOnce => ItemAvailability::Unavailable,
                }
            };
            attachment_metadata.insert(
                canonical,
                AttachmentItemMetadata {
                    mime_type: facts.mime_type.clone(),
                    exact_size: facts.exact_size,
                    content_version: facts.content_version.clone(),
                    metadata_seed,
                    availability,
                    telegram_message_timestamp_ms: projection.telegram_message_timestamp_ms,
                },
            );
            records.push(TreeAttachmentRecord {
                message_id: facts.key.message.message_id,
                index: facts.key.index,
                month,
                display_name,
                size: facts.exact_size,
                content: projection.attachment.blob_hash,
            });
        }
        tree_attachments.insert(chat.key.chat_id.0, records);

        let projections = txn
            .read()
            .story_projections_of_chat(&chat.key)
            .map_err(|_| SessionFailure {
                category: "projection-content-storage",
                retryable: true,
            })?;
        let mut records = Vec::with_capacity(projections.len());
        for projection in projections {
            let facts = &projection.story.facts;
            let appearance = &projection.appearance;
            let canonical = ItemKey::Canonical(CanonicalKey::Story(facts.key)).id();
            let availability = match facts.content_state {
                StoryContentState::Protected => ItemAvailability::Restricted,
                StoryContentState::Available
                    if facts.can_be_forwarded
                        && facts.availability == StateAttachmentAvailability::Fetchable
                        && appearance
                            .removed_at_ms
                            .is_none_or(|_| projection.story.blob_hash.is_some()) =>
                {
                    ItemAvailability::Fetchable
                }
                StoryContentState::Available
                | StoryContentState::MetadataPending
                | StoryContentState::Unsupported
                | StoryContentState::LiveUnavailable
                | StoryContentState::Inaccessible => ItemAvailability::Unavailable,
            };
            story_metadata.insert(
                canonical,
                StoryItemMetadata {
                    mime_type: facts.mime_type.clone(),
                    exact_size: facts.exact_size,
                    content_version: facts.content_version.clone(),
                    metadata_seed: format!(
                        "story-v1|{}|{}|{}|{}|{}|{}",
                        facts.key.poster.chat_id.0,
                        facts.key.story_id.0,
                        appearance.display_name,
                        facts.mime_type.as_deref().unwrap_or(""),
                        facts
                            .exact_size
                            .map_or(String::new(), |size| size.to_string()),
                        match availability {
                            ItemAvailability::Fetchable => "fetchable",
                            ItemAvailability::Restricted => "restricted",
                            ItemAvailability::Unavailable => "unavailable",
                        },
                    ),
                    availability,
                    source_timestamp_ms: facts.source_timestamp_ms,
                },
            );
            // A month can exist because of a persistent story appearance
            // alone, and `Active Stories` never has a month at all. Fold the
            // story instants in so those directories are dated too.
            if let Some(bounds) = correspondence.get_mut(&chat.key.chat_id.0) {
                match appearance.location {
                    StoryAppearanceLocation::Month { year, month } => {
                        bounds.observe(MonthStamp { year, month }, facts.source_timestamp_ms);
                    }
                    StoryAppearanceLocation::Active => {
                        bounds.observe_active_story(facts.source_timestamp_ms);
                    }
                }
            }
            records.push(TreeStoryRecord {
                story_id: facts.key.story_id,
                location: appearance.location,
                display_name: appearance.display_name.clone(),
                size: facts.exact_size,
                content: projection.story.blob_hash,
            });
        }
        tree_stories.insert(chat.key.chat_id.0, records);
    }

    let account_created_at_ms = account.created_at_ms;
    let tree = TreeProjection::new(
        TreeAccountRecord {
            account_id: account.account.account_id,
            namespace_version: account.namespace_version,
            display_name: account.display_name,
        },
        folders
            .iter()
            .map(|folder| TreeFolderRecord {
                folder_id: folder.folder_id,
                title: folder.title.clone(),
            })
            .collect(),
        chats
            .iter()
            .map(|chat| TreeChatRecord {
                chat_id: chat.key.chat_id,
                title: chat.title.clone(),
                username: chat.username.clone(),
                memberships: memberships
                    .get(&chat.key.chat_id.0)
                    .cloned()
                    .unwrap_or_default(),
                message_months: message_months
                    .get(&chat.key.chat_id.0)
                    .cloned()
                    .unwrap_or_default(),
                attachments: tree_attachments
                    .get(&chat.key.chat_id.0)
                    .cloned()
                    .unwrap_or_default(),
                stories: tree_stories
                    .get(&chat.key.chat_id.0)
                    .cloned()
                    .unwrap_or_default(),
            })
            .collect(),
        DocSchemas {
            chat_json: gramdrive_engine::render::chat_json::CHAT_SCHEMA_FAMILY,
            messages_ndjson: gramdrive_model::identity::SchemaFamily(1),
            month_markdown: gramdrive_model::identity::SchemaFamily(1),
            order_json: gramdrive_model::identity::SchemaFamily(1),
        },
    )
    .map_err(|_| SessionFailure {
        category: "projection-tree-model-storage",
        retryable: true,
    })?;

    if target_chat_id.is_none() {
        upsert_fixed_root_structure(txn, scope, tree.root_id(), account_created_at_ms).map_err(
            |_| SessionFailure {
                category: "projection-root-storage",
                retryable: true,
            },
        )?;
    }

    let root_nodes = tree
        .children(
            &tree.root_id(),
            None,
            NonZeroUsize::new(4).ok_or(SessionFailure {
                category: "projection-tree-navigation-storage",
                retryable: true,
            })?,
        )
        .map_err(|_| SessionFailure {
            category: "projection-tree-navigation-storage",
            retryable: true,
        })?
        .nodes;
    let catalog = root_nodes
        .iter()
        .find(|node| node.kind == NodeKind::FolderCatalog)
        .ok_or(SessionFailure {
            category: "projection-tree-model-storage",
            retryable: true,
        })?;
    let folder_nodes = tree
        .children(&catalog.id, None, NonZeroUsize::MAX)
        .map_err(|_| SessionFailure {
            category: "projection-tree-navigation-storage",
            retryable: true,
        })?
        .nodes;
    let chat_versions: BTreeMap<i64, MetadataVersion> = chats
        .into_iter()
        .map(|chat| (chat.key.chat_id.0, chat.metadata_version))
        .collect();
    let mut context = ProjectionContext {
        chat_versions: &chat_versions,
        attachment_metadata: &attachment_metadata,
        story_metadata: &story_metadata,
        protected_chats: &protected_chats,
        account_created_at_ms,
        preserve_existing_directory_facts: depth == ProjectionDepth::Shallow,
        directory_facts: HashMap::new(),
    };
    if target_chat_id.is_none() {
        reconcile_nodes(txn, &catalog.id, &folder_nodes, &context)?;
    }

    for list in root_nodes
        .iter()
        .chain(folder_nodes.iter())
        .filter(|node| node.kind == NodeKind::ChatList)
    {
        let nodes: Vec<TreeNode> = tree
            .children(&list.id, None, NonZeroUsize::MAX)
            .map_err(|_| SessionFailure {
                category: "projection-tree-navigation-storage",
                retryable: true,
            })?
            .nodes
            .into_iter()
            .filter(|node| node.kind == NodeKind::Chat)
            .collect();
        // Directory dates and size rollups are derived bottom-up before any
        // row is written, so each node is upserted exactly once per pass —
        // a second corrective write would journal a provider change that
        // nothing actually caused.
        for chat_node in &nodes {
            collect_directory_facts(txn, &tree, chat_node, &correspondence, &mut context)?;
        }
        if target_chat_id.is_none() {
            reconcile_nodes(txn, &list.id, &nodes, &context)?;
        } else {
            // A chat-scoped pass must not reconcile the list — that would
            // tombstone every sibling it deliberately did not load. The chat
            // row's own rollup and dates still moved, though, so refresh
            // exactly that row in place.
            for chat_node in &nodes {
                refresh_directory_row(txn, chat_node, &list.id, &context)?;
            }
        }
        if depth == ProjectionDepth::Shallow {
            continue;
        }
        for chat_node in &nodes {
            let chat_children = tree
                .children(&chat_node.id, None, NonZeroUsize::MAX)
                .map_err(|_| SessionFailure {
                    category: "projection-tree-navigation-storage",
                    retryable: true,
                })?
                .nodes;
            reconcile_nodes(txn, &chat_node.id, &chat_children, &context)?;
            for container in chat_children
                .iter()
                .filter(|node| matches!(node.kind, NodeKind::Month | NodeKind::ActiveStories))
            {
                let children = tree
                    .children(&container.id, None, NonZeroUsize::MAX)
                    .map_err(|_| SessionFailure {
                        category: "projection-tree-navigation-storage",
                        retryable: true,
                    })?
                    .nodes;
                reconcile_nodes(txn, &container.id, &children, &context)?;
            }
        }
    }
    Ok(())
}

/// The read-only side inputs every reconciliation pass shares.
struct ProjectionContext<'a> {
    chat_versions: &'a BTreeMap<i64, MetadataVersion>,
    attachment_metadata: &'a HashMap<ItemId, AttachmentItemMetadata>,
    story_metadata: &'a HashMap<ItemId, StoryItemMetadata>,
    protected_chats: &'a BTreeSet<i64>,
    /// Dates the fixed root structure and the folder chat lists carry: they
    /// hold no correspondence of their own, and the account's own creation
    /// time is the one truthful non-epoch answer available.
    account_created_at_ms: i64,
    /// Shallow list publication keeps already-computed chat rollups/dates.
    preserve_existing_directory_facts: bool,
    /// Correspondence dates and descendant size rollups, keyed by node id
    /// (appearance-scoped: the same chat under Main and under a folder is
    /// two nodes with the same facts).
    directory_facts: HashMap<Vec<u8>, DirectoryFacts>,
}

impl ProjectionContext<'_> {
    fn directory(&self, node: &ItemId) -> Option<DirectoryFacts> {
        self.directory_facts.get(node.as_bytes()).copied()
    }
}

/// Computes the correspondence dates and exact descendant size rollup of one
/// chat appearance and every directory below it (BUG-260728-2qfzbd).
///
/// Bottom-up: each month (and `Active Stories`) sums its own children, then
/// the chat sums the containers plus its direct files. Leaf sizes come from
/// the same metadata the reconciliation is about to write for attachments
/// and stories, and from the last publication for generated documents — so
/// the rollup is exactly "the size of what is indexed", never a download.
fn collect_directory_facts(
    txn: &gramdrive_state::WriteTxn<'_>,
    tree: &TreeProjection,
    chat_node: &TreeNode,
    correspondence: &BTreeMap<i64, CorrespondenceBounds>,
    context: &mut ProjectionContext<'_>,
) -> Result<(), SessionFailure> {
    let CanonicalKey::Chat(chat) = chat_node.canonical else {
        return Ok(());
    };
    let bounds = correspondence.get(&chat.chat_id.0);
    let navigation = |_: gramdrive_model::tree::ChildrenError| SessionFailure {
        category: "projection-tree-navigation-storage",
        retryable: true,
    };
    let chat_children = tree
        .children(&chat_node.id, None, NonZeroUsize::MAX)
        .map_err(navigation)?
        .nodes;
    // A chat folder is dated from its correspondence; failing that, from the
    // last time Telegram said anything about the chat; failing that, from
    // when this namespace came into existence. The folder demonstrably has
    // existed since then, and every one of those answers beats the epoch
    // Finder shows for an absent timestamp.
    let account_created_at_ms = context.account_created_at_ms;
    let window = bounds
        .and_then(|bounds| bounds.chat)
        .or_else(|| bounds.and_then(|bounds| bounds.metadata_fallback_ms.map(|ms| (ms, ms))))
        .or(Some((account_created_at_ms, account_created_at_ms)));
    let mut chat_rollup = 0u64;
    for child in &chat_children {
        let contribution = match child.kind {
            NodeKind::Month | NodeKind::ActiveStories => {
                let window = match child.canonical {
                    CanonicalKey::MonthDir(dir) => bounds.and_then(|bounds| {
                        bounds.month(MonthStamp {
                            year: dir.year,
                            month: dir.month,
                        })
                    }),
                    _ => bounds.and_then(|bounds| bounds.active_stories),
                };
                let mut container_rollup = 0u64;
                for leaf in tree
                    .children(&child.id, None, NonZeroUsize::MAX)
                    .map_err(navigation)?
                    .nodes
                {
                    container_rollup =
                        container_rollup.saturating_add(indexed_leaf_size(txn, &leaf, context)?);
                }
                context.directory_facts.insert(
                    child.id.as_bytes().to_vec(),
                    DirectoryFacts {
                        created_at_ms: window.map(|(oldest, _)| oldest),
                        modified_at_ms: window.map(|(_, newest)| newest),
                        aggregate_size: Some(container_rollup),
                    },
                );
                container_rollup
            }
            _ => indexed_leaf_size(txn, child, context)?,
        };
        chat_rollup = chat_rollup.saturating_add(contribution);
    }
    let chat_facts = DirectoryFacts {
        created_at_ms: window.map(|(oldest, _)| oldest),
        modified_at_ms: window.map(|(_, newest)| newest),
        aggregate_size: Some(chat_rollup),
    };
    context
        .directory_facts
        .insert(chat_node.id.as_bytes().to_vec(), chat_facts);
    Ok(())
}

/// Rewrites one already-projected directory row's correspondence dates and
/// descendant rollup, leaving its siblings untouched (BUG-260728-2qfzbd).
///
/// The chat-scoped reconciliation path exists precisely so that publishing
/// one month does not walk the whole account, so it cannot call
/// [`reconcile_nodes`] on the chat list: that would tombstone every sibling
/// the pass deliberately did not load. The stored safe name is kept for the
/// same reason — collision resolution is a decision about the *whole* list.
/// The metadata version is derived from exactly the inputs the full pass
/// uses, so the two paths agree and never flap.
///
/// A node with no stored row yet is left alone: creating it is the full
/// pass's job, and it carries the sibling naming this one cannot decide.
fn refresh_directory_row(
    txn: &gramdrive_state::WriteTxn<'_>,
    node: &TreeNode,
    parent: &ItemId,
    context: &ProjectionContext<'_>,
) -> Result<(), SessionFailure> {
    let Some(existing) = txn.read().item(&node.id).map_err(|_| SessionFailure {
        category: "projection-node-read-storage",
        retryable: true,
    })?
    else {
        return Ok(());
    };
    if existing.deleted_at_ms.is_some() {
        return Ok(());
    }
    let facts = context.directory(&node.id).unwrap_or_default();
    let base = match node.canonical {
        CanonicalKey::Chat(chat) => match context.chat_versions.get(&chat.chat_id.0) {
            Some(version) => version.as_str().to_owned(),
            None => node.display_name.clone(),
        },
        _ => node.display_name.clone(),
    };
    txn.upsert_item(&ItemRecord {
        id: node.id.clone(),
        parent: Some(parent.clone()),
        display_name: node.display_name.clone(),
        safe_name: existing.safe_name.clone(),
        metadata_version: directory_version(&base, Some(facts))?,
        content: None,
        aggregate_size: facts.aggregate_size,
        availability: existing.availability,
        created_at_ms: facts.created_at_ms.or(existing.created_at_ms),
        modified_at_ms: facts.modified_at_ms.or(existing.modified_at_ms),
        deleted_at_ms: None,
    })
    .map_err(projection_node_upsert_failure)
}

/// The exact indexed size of one file node, or zero when the source has not
/// reported one yet. Never a download and never an estimate.
fn indexed_leaf_size(
    txn: &gramdrive_state::WriteTxn<'_>,
    node: &TreeNode,
    context: &ProjectionContext<'_>,
) -> Result<u64, SessionFailure> {
    let canonical = ItemKey::Canonical(node.canonical).id();
    if let Some(attachment) = context.attachment_metadata.get(&canonical) {
        return Ok(attachment.exact_size.unwrap_or_default());
    }
    if let Some(story) = context.story_metadata.get(&canonical) {
        return Ok(story.exact_size.unwrap_or_default());
    }
    // Generated documents: whatever the last publication recorded. An
    // unrendered document has no size yet and contributes nothing.
    Ok(txn
        .read()
        .item(&node.id)
        .map_err(|_| SessionFailure {
            category: "projection-node-read-storage",
            retryable: true,
        })?
        .and_then(|item| item.content)
        .and_then(|content| content.logical_size)
        .unwrap_or_default())
}

fn reconcile_nodes(
    txn: &gramdrive_state::WriteTxn<'_>,
    parent: &ItemId,
    nodes: &[TreeNode],
    context: &ProjectionContext<'_>,
) -> Result<(), SessionFailure> {
    let ProjectionContext {
        chat_versions,
        attachment_metadata,
        story_metadata,
        protected_chats,
        account_created_at_ms,
        ..
    } = context;
    let naming_ids: Vec<ItemId> = nodes
        .iter()
        .map(|node| {
            if matches!(node.kind, NodeKind::Attachment | NodeKind::StoryAppearance) {
                ItemKey::Canonical(node.canonical).id()
            } else {
                node.id.clone()
            }
        })
        .collect();
    let sibling_inputs: Vec<_> = nodes
        .iter()
        .zip(&naming_ids)
        .map(|(node, naming_id)| SiblingName {
            id: naming_id,
            raw: &node.display_name,
            kind: if node.kind.is_directory() {
                NameKind::Directory
            } else {
                NameKind::File
            },
            fixed: false,
        })
        .collect();
    let safe_names = resolve_siblings(&sibling_inputs);
    let desired: BTreeSet<Vec<u8>> = nodes
        .iter()
        .map(|node| node.id.as_bytes().to_vec())
        .collect();
    let stored_children = txn
        .read()
        .stored_children(parent)
        .map_err(|_| SessionFailure {
            category: "projection-children-read-storage",
            retryable: true,
        })?;
    for existing in &stored_children {
        if !desired.contains(existing.id.as_bytes()) {
            tombstone_projection_subtree(txn, &existing.id)
                .map_err(|failure| failure.storage_stage("projection-tombstone-storage"))?;
        }
    }

    // SQLite enforces the live sibling-name index after every statement. A
    // batch can validly rename A -> B while the current B is also moving to a
    // free name. Apply that dependency from the free end of the chain, without
    // temporary provider-visible names or unstable identities.
    let mut occupied: HashMap<String, Vec<u8>> = stored_children
        .iter()
        .filter(|item| desired.contains(item.id.as_bytes()))
        .map(|item| (item.safe_name.clone(), item.id.as_bytes().to_vec()))
        .collect();
    let existing_names: HashMap<Vec<u8>, String> = stored_children
        .iter()
        .map(|item| (item.id.as_bytes().to_vec(), item.safe_name.clone()))
        .collect();
    let mut remaining: Vec<usize> = (0..nodes.len()).collect();
    let mut ordered = Vec::with_capacity(nodes.len());
    while !remaining.is_empty() {
        let mut progressed = false;
        let mut next = Vec::new();
        for index in remaining {
            let node_id = nodes[index].id.as_bytes().to_vec();
            let destination = safe_names[index].as_str();
            let available = occupied
                .get(destination)
                .is_none_or(|occupant| *occupant == node_id);
            if !available {
                next.push(index);
                continue;
            }
            if let Some(previous) = existing_names.get(&node_id)
                && previous != destination
            {
                occupied.remove(previous);
            }
            occupied.insert(destination.to_owned(), node_id);
            ordered.push(index);
            progressed = true;
        }
        if !progressed {
            return Err(SessionFailure {
                category: "projection-sibling-rename-cycle",
                retryable: false,
            });
        }
        remaining = next;
    }

    for index in ordered {
        let node = &nodes[index];
        let safe_name = &safe_names[index];
        let existing = txn.read().item(&node.id).map_err(|_| SessionFailure {
            category: "projection-node-read-storage",
            retryable: true,
        })?;
        let canonical_id = ItemKey::Canonical(node.canonical).id();
        let attachment = attachment_metadata.get(&canonical_id);
        let story = story_metadata.get(&canonical_id);
        let source_restricted = matches!(
            &node.canonical,
            CanonicalKey::GeneratedDoc(document)
                if matches!(document.partition, DocPartition::Month { .. })
                    && protected_chats.contains(&document.chat.chat_id.0)
        );
        // Correspondence dates and the descendant rollup are part of a
        // directory's provider-visible metadata, so they belong in its
        // version: a folder whose size grew but whose version did not is a
        // folder the system keeps showing at the old size.
        let facts = if node.kind.is_directory() {
            // A directory with no collected facts is one that owns no
            // correspondence: a chat list, a folder catalog, the account
            // root. It still needs a truthful date — the namespace's own
            // creation time, which beats the epoch Finder shows for an
            // absent timestamp — but it claims no size rollup, exactly as
            // the v16 backfill leaves it NULL.
            Some(context.directory(&node.id).unwrap_or_else(|| {
                if context.preserve_existing_directory_facts {
                    existing.as_ref().map_or(
                        DirectoryFacts {
                            created_at_ms: Some(*account_created_at_ms),
                            modified_at_ms: Some(*account_created_at_ms),
                            aggregate_size: None,
                        },
                        |item| DirectoryFacts {
                            created_at_ms: item.created_at_ms,
                            modified_at_ms: item.modified_at_ms,
                            aggregate_size: item.aggregate_size,
                        },
                    )
                } else {
                    DirectoryFacts {
                        created_at_ms: Some(*account_created_at_ms),
                        modified_at_ms: Some(*account_created_at_ms),
                        aggregate_size: None,
                    }
                }
            }))
        } else {
            None
        };
        let version = match (attachment, story, &node.canonical, existing.as_ref()) {
            (Some(attachment), _, _, _) => attachment_metadata_version(
                attachment,
                parent,
                &node.display_name,
                safe_name.as_str(),
            )?,
            (None, Some(story), _, _) => {
                story_metadata_version(story, parent, &node.display_name, safe_name.as_str())?
            }
            (None, None, CanonicalKey::GeneratedDoc(_), Some(existing)) => {
                existing.metadata_version.clone()
            }
            (None, None, CanonicalKey::Chat(chat), _) => {
                let base = match chat_versions.get(&chat.chat_id.0) {
                    Some(version) => version.as_str().to_owned(),
                    None => node.display_name.clone(),
                };
                directory_version(&base, facts)?
            }
            (None, None, _, _) if node.kind.is_directory() => {
                directory_version(&node.display_name, facts)?
            }
            (None, None, _, _) => stable_version(node.display_name.as_bytes())?,
        };
        txn.upsert_item(&ItemRecord {
            id: node.id.clone(),
            parent: Some(parent.clone()),
            display_name: node.display_name.clone(),
            safe_name: safe_name.as_str().to_owned(),
            metadata_version: version,
            content: if node.kind.is_directory() {
                None
            } else if source_restricted {
                Some(FileFacts::default())
            } else if let Some(attachment) = attachment {
                Some(FileFacts {
                    mime_type: attachment.mime_type.clone(),
                    logical_size: attachment.exact_size,
                    content_version: Some(attachment.content_version.clone()),
                })
            } else if let Some(story) = story {
                Some(FileFacts {
                    mime_type: story.mime_type.clone(),
                    logical_size: story.exact_size,
                    content_version: Some(story.content_version.clone()),
                })
            } else {
                Some(
                    existing
                        .as_ref()
                        .and_then(|item| item.content.clone())
                        .unwrap_or_default(),
                )
            },
            availability: if source_restricted {
                ItemAvailability::Restricted
            } else {
                attachment.map_or_else(
                    || {
                        story.map_or_else(
                            || {
                                existing
                                    .as_ref()
                                    .map_or(ItemAvailability::Fetchable, |item| item.availability)
                            },
                            |story| story.availability,
                        )
                    },
                    |attachment| attachment.availability,
                )
            },
            aggregate_size: facts.and_then(|facts| facts.aggregate_size),
            // Every node that has a source instant publishes it as both its
            // creation and its modification date. An attachment and a story
            // appearance are immutable once observed — a Telegram edit
            // produces a new revision, never a rewrite in place — so their
            // one honest date is the moment they were sent. Directories take
            // their correspondence window. Generated documents keep the
            // publication's own dates: the render pipeline owns that row's
            // content facts and versions, and a second owner here would make
            // the metadata version oscillate between passes.
            created_at_ms: match (attachment, story, facts) {
                (Some(attachment), _, _) => Some(attachment.telegram_message_timestamp_ms),
                (None, Some(story), _) => Some(story.source_timestamp_ms),
                (None, None, Some(facts)) => facts
                    .created_at_ms
                    .or_else(|| existing.as_ref().and_then(|item| item.created_at_ms)),
                (None, None, None) => existing.as_ref().and_then(|item| item.created_at_ms),
            },
            modified_at_ms: match (attachment, story, facts) {
                (Some(attachment), _, _) => Some(attachment.telegram_message_timestamp_ms),
                (None, Some(story), _) => Some(story.source_timestamp_ms),
                (None, None, Some(facts)) => facts
                    .modified_at_ms
                    .or_else(|| existing.as_ref().and_then(|item| item.modified_at_ms)),
                (None, None, None) => existing.as_ref().and_then(|item| item.modified_at_ms),
            },
            deleted_at_ms: None,
        })
        .map_err(projection_node_upsert_failure)?;
        if let CanonicalKey::GeneratedDoc(document) = node.canonical
            && let Some(class) = DocClass::for_key(&document)
        {
            txn.ensure_render_state(&node.id, class.renderer_version(), class.schema_version())
                .map_err(|_| SessionFailure {
                    category: "projection-render-state-storage",
                    retryable: true,
                })?;
            if class == DocClass::ChatJson {
                let source = txn
                    .read()
                    .chat(&document.chat)
                    .map_err(|_| SessionFailure::STORAGE)?
                    .ok_or(SessionFailure::STORAGE)?;
                let expected = compose_chat_metadata(&source)
                    .map_err(|_| SessionFailure::RENDER)?
                    .content_version;
                let current = existing
                    .as_ref()
                    .and_then(|item| item.content.as_ref())
                    .and_then(|facts| facts.content_version.as_ref());
                if current != Some(&expected) {
                    txn.mark_render_dirty(&node.id)
                        .map_err(|_| SessionFailure::STORAGE)?;
                }
            }
        }
    }
    Ok(())
}

/// Tombstones a removed projection branch from the leaves upward.
///
/// A month containing only a removed profile story disappears from the chat
/// tree. Its child must be tombstoned too: direct item lookup, cache-retention
/// cleanup, and the change journal must not leave an apparently live orphan
/// behind a deleted parent.
fn tombstone_projection_subtree(
    txn: &gramdrive_state::WriteTxn<'_>,
    item: &ItemId,
) -> Result<(), SessionFailure> {
    for child in txn
        .read()
        .stored_children(item)
        .map_err(|_| SessionFailure::STORAGE)?
    {
        tombstone_projection_subtree(txn, &child.id)?;
    }
    let version = stable_version(item.as_bytes())?;
    txn.tombstone_item_with_provenance(
        item,
        now_ms(),
        &version,
        gramdrive_state::repo::TombstoneProvenance::Reconcile,
    )
    .map_err(|_| SessionFailure::STORAGE)?;
    Ok(())
}

fn story_metadata_version(
    story: &StoryItemMetadata,
    parent: &ItemId,
    display_name: &str,
    safe_name: &str,
) -> Result<MetadataVersion, SessionFailure> {
    stable_version(
        format!(
            "story-v2|{}|{}|{}|{}",
            story.metadata_seed,
            parent.text(),
            display_name,
            safe_name,
        )
        .as_bytes(),
    )
}

/// Versions the final provider-visible attachment projection. In particular,
/// deterministic collision resolution is part of metadata identity: when a
/// newly discovered sibling changes an existing placeholder's safe name,
/// Finder must observe a new metadata version while the item identifier stays
/// stable.
fn attachment_metadata_version(
    attachment: &AttachmentItemMetadata,
    parent: &ItemId,
    display_name: &str,
    safe_name: &str,
) -> Result<MetadataVersion, SessionFailure> {
    let availability = match attachment.availability {
        ItemAvailability::Fetchable => "fetchable",
        ItemAvailability::Restricted => "restricted",
        ItemAvailability::Unavailable => "unavailable",
    };
    stable_version(
        format!(
            "attachment-v3|{}|{}|{}|{}|{}",
            attachment.metadata_seed,
            parent.text(),
            display_name,
            safe_name,
            availability,
        )
        .as_bytes(),
    )
}

fn namespace_counts(
    store: &mut StateStore,
    scope: gramdrive_model::identity::AccountScope,
) -> Result<NamespaceProgress, SessionFailure> {
    let txn = store.read_txn().map_err(|_| SessionFailure::STORAGE)?;
    let canonical = txn.chats(scope).map_err(|_| SessionFailure::STORAGE)?.len();
    let folders = txn.folders(scope).map_err(|_| SessionFailure::STORAGE)?;
    let mut appearances = 0usize;
    for kind in [
        ChatListKind::Main,
        ChatListKind::Archive,
        ChatListKind::Stories,
    ]
    .into_iter()
    .chain(
        folders
            .into_iter()
            .map(|folder| ChatListKind::Folder(folder.folder_id)),
    ) {
        appearances = appearances.saturating_add(
            txn.chat_list(&ChatListKey { scope, kind })
                .map_err(|_| SessionFailure::STORAGE)?
                .len(),
        );
    }
    Ok(NamespaceProgress::Ready {
        canonical_chat_count: u64::try_from(canonical).unwrap_or(u64::MAX),
        appearance_count: u64::try_from(appearances).unwrap_or(u64::MAX),
    })
}

fn chat_version(
    chat_id: i64,
    kind: SnapshotChatKind,
    title: &str,
    username: Option<&str>,
    protected: bool,
) -> Result<MetadataVersion, SessionFailure> {
    let text = format!(
        "{chat_id}|{kind:?}|{title}|{}|{protected}",
        username.unwrap_or("")
    );
    stable_version(text.as_bytes())
}

/// Versions a directory node from its name plus the facts a provider shows
/// for it: the correspondence window and the descendant size rollup, which
/// is absent for the kinds that own none.
///
/// Delegates to the shared model helper, which the state layer's targeted
/// rollup refresh also uses. One derivation, two owners, identical tokens
/// for identical state — so a publication-time rollup and a full
/// reconciliation can never undo each other (BUG-260728-2qfzbd).
fn directory_version(
    base: &str,
    facts: Option<DirectoryFacts>,
) -> Result<MetadataVersion, SessionFailure> {
    let facts = facts.unwrap_or_default();
    gramdrive_model::version::directory_metadata_version(
        base,
        facts.created_at_ms,
        facts.modified_at_ms,
        facts.aggregate_size,
    )
    .map_err(|_| SessionFailure::STORAGE)
}

fn stable_version(bytes: &[u8]) -> Result<MetadataVersion, SessionFailure> {
    MetadataVersion::new(format!("namespace-{:016x}", stable_hash(bytes)))
        .map_err(|_| SessionFailure::STORAGE)
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn now_ms() -> i64 {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{CancellationToken, ProgressListener, TransferProgress};
    use gramdrive_engine::backfill::WaitReason;
    use gramdrive_engine::render_pipeline::GeneratedFileLease;
    use gramdrive_model::identity::{
        AccountKey, AccountScope, ActiveStoriesKey, AppearanceKey, AttachmentIndex, ContentHash,
        MessageId, MonthDirKey, NamespaceVersion, StoryAppearanceKey,
    };
    use gramdrive_source_tdjson::message::normalize_message;
    use gramdrive_state::repo::{AccountRecord, MessageEventKind, RetentionMode, SourceKind};

    #[derive(Default)]
    struct RecordingNamespaceProgress {
        values: Mutex<Vec<NamespaceProgress>>,
    }

    struct NoopHydrationProgress;

    impl ProgressListener for NoopHydrationProgress {
        fn on_progress(&self, _progress: TransferProgress) {}
    }

    impl NamespaceProgressListener for RecordingNamespaceProgress {
        fn on_progress(&self, progress: NamespaceProgress) {
            self.values
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(progress);
        }
    }

    impl RecordingNamespaceProgress {
        fn snapshot(&self) -> Vec<NamespaceProgress> {
            self.values
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    #[test]
    fn storage_stage_refines_only_generic_storage_failures() {
        let staged = SessionFailure::STORAGE.storage_stage("snapshot-storage");
        assert_eq!(staged.category, "snapshot-storage");
        assert!(staged.retryable);

        let source = SessionFailure::SOURCE.storage_stage("snapshot-storage");
        assert_eq!(source.category, "source");
        assert!(source.retryable);
    }

    #[test]
    fn busy_content_ticks_always_yield_but_idle_polls_are_not_extended() {
        assert_eq!(content_loop_delay(true), CONTENT_LOOP_BUSY_YIELD);
        assert_eq!(
            content_loop_delay(false),
            Duration::ZERO,
            "the existing idle poll is already the wait"
        );
    }

    #[test]
    fn projection_upsert_maps_sibling_constraint_without_exposing_names() {
        let mut store = store();
        let txn = store.write_txn().expect("write");
        let parent = ItemKey::Canonical(CanonicalKey::Account(scope().account)).id();
        let record = |chat_id| ItemRecord {
            aggregate_size: None,
            id: ItemKey::Appearance(AppearanceKey {
                view: ChatListKind::Main,
                item: CanonicalKey::Chat(ChatKey {
                    scope: scope(),
                    chat_id: ChatId(chat_id),
                }),
            })
            .id(),
            parent: Some(parent.clone()),
            display_name: "private source name".to_owned(),
            safe_name: "same-safe-name".to_owned(),
            metadata_version: MetadataVersion::new(format!("v{chat_id}")).expect("version"),
            content: None,
            availability: ItemAvailability::Fetchable,
            created_at_ms: None,
            modified_at_ms: None,
            deleted_at_ms: None,
        };
        txn.upsert_item(&record(101)).expect("first sibling");
        let category = projection_node_upsert_failure(
            txn.upsert_item(&record(102))
                .expect_err("sibling collision"),
        )
        .category;
        assert_eq!(category, "projection-sibling-name-conflict");
        assert!(!category.contains("private"));
    }

    #[test]
    fn projection_reorders_acyclic_sibling_rename_chain() {
        let (blocked_id, blocker_id) = (100..140)
            .flat_map(|left| (100..140).map(move |right| (left, right)))
            .filter(|(left, right)| left != right)
            .find(|(left, right)| {
                let appearance = |chat_id| {
                    ItemKey::Appearance(AppearanceKey {
                        view: ChatListKind::Main,
                        item: CanonicalKey::Chat(ChatKey {
                            scope: scope(),
                            chat_id: ChatId(chat_id),
                        }),
                    })
                    .id()
                };
                appearance(*left).as_bytes() < appearance(*right).as_bytes()
            })
            .expect("ordered identities");

        let mut store = store();
        let txn = store.write_txn().expect("initial chat transaction");
        txn.upsert_chat(
            &snapshot_chat_record(scope(), &chat(blocked_id, "Alpha")).expect("blocked record"),
        )
        .expect("blocked chat");
        txn.upsert_chat(
            &snapshot_chat_record(scope(), &chat(blocker_id, "Beta")).expect("blocker record"),
        )
        .expect("blocker chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[
                ChatListEntry {
                    chat_id: ChatId(blocked_id),
                    sort_order: 2,
                    pinned: false,
                },
                ChatListEntry {
                    chat_id: ChatId(blocker_id),
                    sort_order: 1,
                    pinned: false,
                },
            ],
        )
        .expect("memberships");
        txn.commit().expect("commit initial chats");
        rebuild_projection(&mut store, scope()).expect("initial projection");

        let txn = store.write_txn().expect("rename transaction");
        txn.upsert_chat(
            &snapshot_chat_record(scope(), &chat(blocked_id, "Beta")).expect("renamed blocked"),
        )
        .expect("rename blocked");
        txn.upsert_chat(
            &snapshot_chat_record(scope(), &chat(blocker_id, "Gamma")).expect("renamed blocker"),
        )
        .expect("rename blocker");
        reconcile_projection_txn(&txn, scope()).expect("dependency-ordered projection");
        txn.commit().expect("commit rename chain");
    }

    #[test]
    fn live_replay_suppresses_duplicate_provider_signal_but_reorder_signals() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        rebuild_projection(&mut store, scope()).expect("initial projection");

        let mut batch = UpdateBatch::default();
        batch.chats.push(ChatMetadata {
            chat_id: 100,
            kind: SnapshotChatKind::Private,
            title: "Chat 100".to_owned(),
            username: None,
            is_protected: false,
            photo: None,
        });
        batch.memberships.push(MembershipChange::Set {
            list: ChatListKind::Main,
            chat_id: 100,
            sort_order: 1,
            pinned: false,
        });
        assert!(
            !apply_update_batch(&mut store, scope(), &batch).expect("idempotent replay"),
            "an exact TDLib startup replay must not signal the provider"
        );

        batch.memberships[0] = MembershipChange::Set {
            list: ChatListKind::Main,
            chat_id: 100,
            sort_order: 9,
            pinned: true,
        };
        assert!(
            apply_update_batch(&mut store, scope(), &batch).expect("reorder"),
            "a real list-order change must signal the provider"
        );
        assert!(
            !apply_update_batch(&mut store, scope(), &batch).expect("replayed reorder"),
            "replaying the new order must become signal-idempotent"
        );
        assert_eq!(
            store
                .read_txn()
                .expect("read")
                .chat_list(&ChatListKey {
                    scope: scope(),
                    kind: ChatListKind::Main,
                })
                .expect("main list"),
            vec![ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 9,
                pinned: true,
            }]
        );
    }

    #[test]
    fn unwitnessed_live_final_membership_removal_preserves_projected_subtree() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveSnapshot {
                chat_id: 100,
                order: 70,
                stories: vec![story(191, false, StoryContentKind::Video)],
            },
            1_000,
        )
        .expect("active story");
        rebuild_projection(&mut store, scope()).expect("ready projection");
        let txn = store.write_txn().expect("seed readiness cursor");
        let readiness = txn
            .publish_namespace_readiness(scope(), 1_100)
            .expect("publish readiness");
        txn.advance_namespace_projection(
            scope(),
            readiness.generation,
            Some(ChatId(100)),
            true,
            1_101,
        )
        .expect("advance readiness cursor");
        txn.commit().expect("commit readiness cursor");
        let readiness_before = store
            .read_txn()
            .expect("read readiness before refresh")
            .namespace_readiness(scope())
            .expect("readiness lookup before refresh")
            .expect("readiness before refresh");
        let guarded_live_before = store
            .connection()
            .query_row(
                "SELECT count(*) FROM items
                  WHERE deleted_at_ms IS NULL
                    AND kind IN ('chat', 'active_stories', 'generated_doc', 'story_appearance')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("guarded live kinds before refresh");
        assert!(
            guarded_live_before >= 4,
            "the fixture must exercise the installed four-kind deletion shape"
        );

        let anchor = store
            .read_txn()
            .expect("read journal anchor")
            .change_journal_state()
            .expect("journal anchor")
            .latest_sequence;
        let batch = UpdateBatch {
            memberships: vec![MembershipChange::Removed {
                list: ChatListKind::Main,
                chat_id: 100,
            }],
            ..UpdateBatch::default()
        };

        assert!(
            !apply_update_batch(&mut store, scope(), &batch).expect("unwitnessed live removal"),
            "an unstable membership refresh cannot signal a provider deletion"
        );
        let tombstone_count = store
            .connection()
            .query_row(
                "SELECT count(*) FROM items WHERE deleted_at_ms IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("tombstone count");
        let guarded_live_after = store
            .connection()
            .query_row(
                "SELECT count(*) FROM items
                  WHERE deleted_at_ms IS NULL
                    AND kind IN ('chat', 'active_stories', 'generated_doc', 'story_appearance')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("guarded live kinds after refresh");
        let read = store.read_txn().expect("read preserved projection");
        assert_eq!(
            read.namespace_readiness(scope())
                .expect("readiness lookup after refresh")
                .expect("readiness after refresh"),
            readiness_before,
            "the rejected live absence cannot regress readiness or its durable cursor"
        );
        assert_eq!(
            read.chat_list(&ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            })
            .expect("preserved membership")
            .len(),
            1,
            "membership remains resumable until a positive departure witness arrives"
        );
        assert_eq!(
            tombstone_count, 0,
            "the chat, Active Stories, generated document, and story appearance stay live"
        );
        assert_eq!(guarded_live_after, guarded_live_before);
        assert!(
            read.item_changes_since(scope().account, anchor, u32::MAX)
                .expect("provider changes")
                .iter()
                .all(|change| change.item.deleted_at_ms.is_none()),
            "the production live-update path emits no didDeleteItems"
        );
        drop(read);

        let txn = store.write_txn().expect("record departure witness");
        let mut departed =
            snapshot_chat_record(scope(), &chat(100, "Chat 100")).expect("departed chat record");
        departed.left_at_ms = Some(2_000);
        txn.upsert_chat(&departed).expect("departure witness");
        txn.commit().expect("commit departure witness");

        assert!(
            apply_update_batch(&mut store, scope(), &batch).expect("witnessed live removal"),
            "a positive departure witness still publishes the legitimate deletion"
        );
        let read = store.read_txn().expect("read witnessed removal");
        assert!(
            read.chat_list(&ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            })
            .expect("removed membership")
            .is_empty()
        );
        assert!(
            read.item_changes_since(scope().account, anchor, u32::MAX)
                .expect("witnessed provider changes")
                .iter()
                .any(|change| change.item.deleted_at_ms.is_some()),
            "the witnessed subtree removal emits provider deletion changes"
        );
        drop(read);
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT count(DISTINCT kind) FROM items
                      WHERE deleted_at_ms IS NOT NULL
                        AND kind IN ('chat', 'active_stories', 'generated_doc', 'story_appearance')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("witnessed tombstone kinds"),
            4,
            "the positive witness removes the legitimate four-kind subtree"
        );
    }

    fn scope() -> AccountScope {
        AccountScope {
            account: AccountKey {
                account_id: AccountId(7),
            },
            namespace_version: NamespaceVersion(1),
        }
    }

    fn store() -> StateStore {
        initialized_store(StateStore::open_in_memory().expect("open"))
    }

    fn initialized_store(mut store: StateStore) -> StateStore {
        let txn = store.write_txn().expect("write");
        txn.upsert_account(&AccountRecord {
            account: scope().account,
            source_kind: SourceKind::LocalTdlib,
            display_name: "Account".to_owned(),
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
        let root = ItemKey::Canonical(CanonicalKey::Account(scope().account)).id();
        txn.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: root.clone(),
            parent: None,
            display_name: "Account".to_owned(),
            safe_name: "Account".to_owned(),
            metadata_version: MetadataVersion::new("v1").expect("version"),
            content: None,
            availability: ItemAvailability::Fetchable,
            created_at_ms: None,
            modified_at_ms: None,
            deleted_at_ms: None,
        })
        .expect("root");
        upsert_fixed_root_structure(&txn, scope(), root, 1).expect("roots");
        txn.commit().expect("commit");
        store
    }

    fn chat(id: i64, title: &str) -> gramdrive_source_tdjson::ChatSnapshot {
        gramdrive_source_tdjson::ChatSnapshot {
            chat_id: id,
            kind: SnapshotChatKind::Private,
            title: title.to_owned(),
            username: None,
            is_protected: false,
        }
    }

    fn add_chat(store: &mut StateStore, id: i64, protected: bool) {
        let mut snapshot = chat(id, &format!("Chat {id}"));
        snapshot.is_protected = protected;
        let txn = store.write_txn().expect("write");
        txn.upsert_chat(&snapshot_chat_record(scope(), &snapshot).expect("record"))
            .expect("chat");
        txn.commit().expect("commit");
    }

    fn add_main_chat(store: &mut StateStore, id: i64, protected: bool) {
        add_chat(store, id, protected);
        let txn = store.write_txn().expect("write membership");
        txn.upsert_chat_list_entry(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &ChatListEntry {
                chat_id: ChatId(id),
                sort_order: 1,
                pinned: false,
            },
        )
        .expect("main membership");
        txn.commit().expect("commit membership");
    }

    fn story(
        story_id: i64,
        posted_to_profile: bool,
        content_kind: StoryContentKind,
    ) -> StoryObservation {
        let (can_be_forwarded, mime_type, exact_size) = match content_kind {
            StoryContentKind::Photo => (true, Some("image/jpeg".to_owned()), Some(1_024)),
            StoryContentKind::Video => (true, Some("video/mp4".to_owned()), Some(4_096)),
            _ => (false, None, None),
        };
        let content_version = format!("story-{story_id}-v1");
        let locators = match content_kind {
            StoryContentKind::Photo | StoryContentKind::Video => {
                vec![gramdrive_source_tdjson::StoryContentLocator {
                    role: if content_kind == StoryContentKind::Photo {
                        "photo-size:x".to_owned()
                    } else {
                        "video-primary".to_owned()
                    },
                    file_type: if content_kind == StoryContentKind::Photo {
                        SourceStoryFileType::PhotoStory
                    } else {
                        SourceStoryFileType::VideoStory
                    },
                    is_primary: true,
                    local_file_id: Some(i32::try_from(story_id).unwrap_or(1)),
                    remote_file_id: Some(format!("remote-{story_id}")),
                    remote_unique_id: Some(format!("unique-{story_id}")),
                    size: exact_size,
                    expected_size: exact_size,
                    content_version: content_version.clone(),
                }]
            }
            _ => Vec::new(),
        };
        StoryObservation {
            poster_chat_id: 100,
            story_id,
            date_ms: 1_721_555_200_000,
            is_posted_to_chat_page: posted_to_profile,
            can_be_forwarded,
            content_kind,
            mime_type,
            exact_size,
            content_version,
            locators,
        }
    }

    fn ingest_unknown_chat_active_story_list_pass(state: &mut StateStore) {
        use gramdrive_source_tdjson::mock::MockTdJson;
        use gramdrive_source_tdjson::{RuntimeConfig, TdRuntime};

        let txn = state.write_txn().expect("start story-list pass");
        txn.start_story_list_pass(scope(), now_ms())
            .expect("seed account progress");
        txn.commit().expect("commit story-list pass");

        let mut content = ContentCoordinator::new(state, scope()).expect("coordinator");
        content
            .stories
            .set_account_identity(7, StoryAccountKind::Regular)
            .expect("account identity");
        content
            .stories
            .start_active_list_discovery()
            .expect("active-list discovery");

        let (sender, receiver, handle) = MockTdJson::new();
        handle.set_responder(|sent| {
            vec![
                json!({
                    "@type": "updateNewChat",
                    "chat": {
                        "@type": "chat",
                        "id": 777,
                        "type": {"@type": "chatTypePrivate", "user_id": 77},
                        "title": "Story-only chat",
                        "positions": []
                    },
                    "@client_id": sent.client_id,
                })
                .to_string(),
                json!({
                    "@type": "updateChatActiveStories",
                    "active_stories": {
                        "@type": "chatActiveStories",
                        "chat_id": 777,
                        "order": 70,
                        "stories": [{
                            "@type": "storyInfo",
                            "story_id": 12,
                            "date": 1_784_692_800,
                            "is_live": true
                        }]
                    },
                    "@client_id": sent.client_id,
                })
                .to_string(),
                json!({
                    "@type": "ok",
                    "@extra": sent.extra().expect("runtime request correlation"),
                    "@client_id": sent.client_id,
                })
                .to_string(),
            ]
        });
        let runtime = TdRuntime::start(
            sender,
            receiver,
            RuntimeConfig {
                receive_timeout: Duration::from_millis(5),
                update_queue_capacity: 8,
            },
        )
        .expect("runtime");
        let (client, updates) = runtime.create_client().expect("client");
        let StoryStep::Submit(request) = content.stories.next_step().expect("story-list step")
        else {
            panic!("story-list discovery must submit")
        };
        let pending = client.request(request).expect("story-list request");
        let mut folders = FolderCatalogMachine::new();
        let mut metadata = UpdateMachine::new();
        let cancelled = AtomicBool::new(false);

        wait_for_story_response(
            state,
            scope(),
            pending,
            &mut folders,
            &mut metadata,
            &mut content,
            &client,
            &updates,
            &cancelled,
        )
        .expect("owned-session response wait")
        .expect("story response");

        let chat_key = ChatKey {
            scope: scope(),
            chat_id: ChatId(777),
        };
        assert!(
            state
                .read_txn()
                .expect("read metadata checkpoint")
                .chat(&chat_key)
                .expect("chat lookup")
                .is_some(),
            "updateNewChat must be durable before the queued story commit is exposed"
        );

        for expected in ["active snapshot", "list progress"] {
            let StoryStep::Commit(commit) = content.stories.next_step().expect(expected) else {
                panic!("expected {expected} commit")
            };
            apply_story_commit(state, scope(), &commit, now_ms()).expect(expected);
        }

        let sent = handle.take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].request_type().as_deref(), Some("loadActiveStories"));
        runtime.shutdown();
    }

    #[test]
    fn story_commit_replay_transitions_one_canonical_row_to_profile() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        let active = story(91, false, StoryContentKind::Video);
        let snapshot = StoryCommit::ActiveSnapshot {
            chat_id: 100,
            order: 70,
            stories: vec![active],
        };
        apply_story_commit(&mut store, scope(), &snapshot, 1_000).expect("active snapshot");
        apply_story_commit(&mut store, scope(), &snapshot, 1_001).expect("duplicate snapshot");

        let profile = story(91, true, StoryContentKind::Video);
        apply_story_commit(&mut store, scope(), &StoryCommit::Upsert(profile), 1_002)
            .expect("profile transition");
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveSnapshot {
                chat_id: 100,
                order: 70,
                stories: Vec::new(),
            },
            1_003,
        )
        .expect("active expiry");

        assert_eq!(
            store
                .connection()
                .query_row("SELECT count(*) FROM stories", [], |row| row
                    .get::<_, i64>(0))
                .expect("story count"),
            1
        );
        let key = StoryKey {
            poster: ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            },
            story_id: StoryId(91),
        };
        let appearances = store
            .read_txn()
            .expect("read")
            .story_appearances(&key)
            .expect("appearances");
        assert_eq!(appearances.len(), 1);
        assert!(matches!(
            appearances[0].location,
            StoryAppearanceLocation::Month { .. }
        ));
    }

    #[test]
    fn story_list_only_chat_projects_in_stories_without_main_or_history_work() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        let active = StoryCommit::ActiveSnapshot {
            chat_id: 100,
            order: 900,
            stories: vec![story(91, false, StoryContentKind::Video)],
        };
        apply_story_commit(&mut store, scope(), &active, 1_000).expect("active snapshot");
        rebuild_projection(&mut store, scope()).expect("Stories projection");

        let chat = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let stories_list = ChatListKey {
            scope: scope(),
            kind: ChatListKind::Stories,
        };
        let stories_chat = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Stories,
            item: CanonicalKey::Chat(chat),
        })
        .id();
        let main_chat = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Main,
            item: CanonicalKey::Chat(chat),
        })
        .id();
        let read = store.read_txn().expect("read Stories projection");
        assert_eq!(
            read.chat_list(&stories_list).expect("Stories list"),
            vec![ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 900,
                pinned: false,
            }]
        );
        assert!(read.item(&stories_chat).expect("Stories chat").is_some());
        assert!(read.item(&main_chat).expect("Main chat").is_none());
        assert!(
            read.chat_sync_state(&chat)
                .expect("history state")
                .is_none(),
            "Stories membership must not make an unlisted chat schedulable for history"
        );
        drop(read);

        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveSnapshot {
                chat_id: 100,
                order: 900,
                stories: Vec::new(),
            },
            1_100,
        )
        .expect("active expiry");
        rebuild_projection(&mut store, scope()).expect("expiry projection");
        let read = store.read_txn().expect("read expiry projection");
        assert!(
            read.chat_list(&stories_list)
                .expect("Stories list")
                .is_empty()
        );
        assert!(
            read.item(&stories_chat)
                .expect("Stories chat")
                .is_some_and(|item| item.deleted_at_ms.is_some()),
            "the Stories appearance must disappear with its final active story"
        );
    }

    #[test]
    fn profile_pin_keeps_the_stories_appearance_after_active_expiry() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveSnapshot {
                chat_id: 100,
                order: 900,
                stories: vec![story(91, false, StoryContentKind::Video)],
            },
            1_000,
        )
        .expect("active snapshot");
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ProfilePage {
                chat_id: 100,
                generation: 1,
                stories: vec![story(91, true, StoryContentKind::Video)],
                pinned_story_ids: vec![91],
                next_from_story_id: None,
                complete: true,
            },
            1_010,
        )
        .expect("profile pin");
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveSnapshot {
                chat_id: 100,
                order: 900,
                stories: Vec::new(),
            },
            1_020,
        )
        .expect("active expiry");
        rebuild_projection(&mut store, scope()).expect("pinned projection");

        let chat = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let stories_list = ChatListKey {
            scope: scope(),
            kind: ChatListKind::Stories,
        };
        let stories_month = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Stories,
            item: CanonicalKey::MonthDir(MonthDirKey {
                chat,
                year: 2024,
                month: 7,
            }),
        })
        .id();
        let read = store.read_txn().expect("read pinned projection");
        assert_eq!(
            read.chat_list(&stories_list).expect("Stories list").len(),
            1
        );
        assert!(
            read.item(&stories_month)
                .expect("Stories month")
                .is_some_and(|item| item.deleted_at_ms.is_none()),
            "the profile-pinned story remains once in its direct month"
        );
    }

    #[test]
    fn story_projection_moves_active_to_one_month_and_replays_without_duplicates() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        let key = StoryKey {
            poster: ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            },
            story_id: StoryId(191),
        };
        let active = StoryCommit::ActiveSnapshot {
            chat_id: 100,
            order: 70,
            stories: vec![story(191, false, StoryContentKind::Video)],
        };
        apply_story_commit(&mut store, scope(), &active, 1_000).expect("active story");
        rebuild_projection(&mut store, scope()).expect("active projection");

        let active_parent = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Main,
            item: CanonicalKey::ActiveStories(ActiveStoriesKey { chat: key.poster }),
        })
        .id();
        let active_item = ItemKey::StoryAppearance(StoryAppearanceKey {
            story: key,
            view: ChatListKind::Main,
            location: StoryAppearanceLocation::Active,
        })
        .id();
        let read = store.read_txn().expect("read active projection");
        assert_eq!(
            read.stored_children(&active_parent)
                .expect("active children")
                .into_iter()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![active_item.clone()]
        );
        assert!(read.item(&active_item).expect("active item").is_some());
        assert_eq!(
            read.story_projections_of_chat(&key.poster)
                .expect("story projections")
                .len(),
            1
        );
        drop(read);

        let hash = ContentHash::Sha256([0x19; 32]);
        let txn = store.write_txn().expect("link materialized blob");
        txn.record_blob(scope().account, &hash, 4_096, 1_050)
            .expect("blob");
        txn.link_story_blob(&key, &hash, 1_050).expect("story blob");
        txn.commit().expect("commit blob");

        let profile = StoryCommit::Upsert(story(191, true, StoryContentKind::Video));
        apply_story_commit(&mut store, scope(), &profile, 1_100).expect("profile transition");
        rebuild_projection(&mut store, scope()).expect("profile projection");
        let month_parent = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Main,
            item: CanonicalKey::MonthDir(MonthDirKey {
                chat: key.poster,
                year: 2024,
                month: 7,
            }),
        })
        .id();
        let month_item = ItemKey::StoryAppearance(StoryAppearanceKey {
            story: key,
            view: ChatListKind::Main,
            location: StoryAppearanceLocation::Month {
                year: 2024,
                month: 7,
            },
        })
        .id();
        let read = store.read_txn().expect("read profile projection");
        assert!(
            read.item(&active_parent)
                .expect("old active container")
                .is_some_and(|item| item.deleted_at_ms.is_some()),
            "a profile-only story must not leave an empty Active Stories container"
        );
        assert!(
            read.item(&active_item)
                .expect("old active item")
                .is_some_and(|item| item.deleted_at_ms.is_some()),
            "the active appearance is removed by the transition"
        );
        assert_eq!(
            read.stored_children(&month_parent)
                .expect("month children")
                .into_iter()
                .filter(|item| matches!(item.id.key(), ItemKey::StoryAppearance(_)))
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![month_item.clone()]
        );
        assert_eq!(
            read.story(&key)
                .expect("canonical story")
                .expect("story")
                .blob_hash,
            Some(hash)
        );
        drop(read);

        // Normal active expiry cannot remove profile content.
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveSnapshot {
                chat_id: 100,
                order: 70,
                stories: Vec::new(),
            },
            1_200,
        )
        .expect("active expiry");
        rebuild_projection(&mut store, scope()).expect("expiry projection");
        assert!(
            store
                .read_txn()
                .expect("read expired profile")
                .item(&month_item)
                .expect("month item")
                .is_some_and(|item| item.deleted_at_ms.is_none())
        );

        // Relaunch/paging replay: the same bounded page and projection emit
        // neither another canonical row nor another item-journal change.
        apply_story_commit(&mut store, scope(), &profile, 1_300).expect("profile replay");
        rebuild_projection(&mut store, scope()).expect("first replay projection");
        let sequence = store
            .read_txn()
            .expect("read journal")
            .change_journal_state()
            .expect("journal state")
            .latest_sequence;
        rebuild_projection(&mut store, scope()).expect("relaunch projection replay");
        let read = store.read_txn().expect("read replay projection");
        assert_eq!(
            read.change_journal_state()
                .expect("journal state after replay")
                .latest_sequence,
            sequence
        );
        assert_eq!(
            read.story_projections_of_chat(&key.poster)
                .expect("canonical projection")
                .len(),
            1
        );
        drop(read);
        assert_eq!(
            store
                .connection()
                .query_row("SELECT count(*) FROM stories", [], |row| row
                    .get::<_, i64>(0))
                .expect("canonical story count"),
            1
        );
    }

    #[test]
    fn mirror_profile_removal_tombstones_the_story_leaf_inside_a_removed_month() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        let key = StoryKey {
            poster: ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            },
            story_id: StoryId(192),
        };
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::Upsert(story(192, true, StoryContentKind::Photo)),
            2_000,
        )
        .expect("profile story");
        rebuild_projection(&mut store, scope()).expect("profile projection");
        let month_item = ItemKey::StoryAppearance(StoryAppearanceKey {
            story: key,
            view: ChatListKind::Main,
            location: StoryAppearanceLocation::Month {
                year: 2024,
                month: 7,
            },
        })
        .id();

        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::Inaccessible {
                poster_chat_id: 100,
                story_id: 192,
            },
            2_100,
        )
        .expect("mirror removal");
        rebuild_projection(&mut store, scope()).expect("removed projection");
        let read = store.read_txn().expect("read removed projection");
        assert!(read.story(&key).expect("story query").is_none());
        assert!(
            read.item(&month_item)
                .expect("story item")
                .is_some_and(|item| item.deleted_at_ms.is_some()),
            "removing a story-only month must tombstone its leaf, not orphan it"
        );
    }

    #[test]
    fn audit_retains_removed_profile_story_but_not_an_ordinary_expired_active_story() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        let tx = store.write_txn().expect("audit policy transaction");
        tx.set_retention_mode(scope().account, RetentionMode::Audit, None, 1_000)
            .expect("set Audit");
        tx.commit().expect("commit Audit");

        let profile_key = StoryKey {
            poster: ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            },
            story_id: StoryId(194),
        };
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::Upsert(story(194, true, StoryContentKind::Video)),
            1_100,
        )
        .expect("profile story");
        let hash = ContentHash::Sha256([0x94; 32]);
        let tx = store.write_txn().expect("materialized profile story");
        tx.record_blob(scope().account, &hash, 4_096, 1_150)
            .expect("blob");
        tx.link_story_blob(&profile_key, &hash, 1_150)
            .expect("story blob");
        tx.commit().expect("commit materialized story");
        rebuild_projection(&mut store, scope()).expect("profile projection");

        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::Inaccessible {
                poster_chat_id: 100,
                story_id: 194,
            },
            1_200,
        )
        .expect("Audit profile removal");
        rebuild_projection(&mut store, scope()).expect("Audit retained projection");
        let month_item = ItemKey::StoryAppearance(StoryAppearanceKey {
            story: profile_key,
            view: ChatListKind::Main,
            location: StoryAppearanceLocation::Month {
                year: 2024,
                month: 7,
            },
        })
        .id();
        let read = store.read_txn().expect("read Audit projection");
        assert_eq!(
            read.story(&profile_key)
                .expect("story query")
                .expect("retained story")
                .blob_hash,
            Some(hash)
        );
        assert!(
            read.story_appearances(&profile_key)
                .expect("retained appearance")[0]
                .removed_at_ms
                .is_some()
        );
        assert!(
            read.item(&month_item)
                .expect("month item")
                .is_some_and(|item| item.deleted_at_ms.is_none())
        );
        drop(read);

        let ephemeral_key = StoryKey {
            poster: profile_key.poster,
            story_id: StoryId(195),
        };
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveSnapshot {
                chat_id: 100,
                order: 70,
                stories: vec![story(195, false, StoryContentKind::Photo)],
            },
            1_300,
        )
        .expect("ordinary active story");
        rebuild_projection(&mut store, scope()).expect("active projection");
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveSnapshot {
                chat_id: 100,
                order: 70,
                stories: Vec::new(),
            },
            1_400,
        )
        .expect("ordinary active expiry");
        rebuild_projection(&mut store, scope()).expect("expired projection");
        assert!(
            store
                .read_txn()
                .expect("read expired active")
                .story(&ephemeral_key)
                .expect("story query")
                .is_none(),
            "Audit never retains active-only ephemeral content"
        );
    }

    #[test]
    fn posting_success_rekeys_temporary_active_story_without_losing_appearance() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveSnapshot {
                chat_id: 100,
                order: 70,
                stories: vec![story(-9, false, StoryContentKind::MetadataPending)],
            },
            1_000,
        )
        .expect("temporary active");
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::PostSucceeded {
                old_story_id: -9,
                story: story(97, false, StoryContentKind::Video),
            },
            1_100,
        )
        .expect("posting success");

        let poster = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let read = store.read_txn().expect("read");
        assert!(
            read.story(&StoryKey {
                poster,
                story_id: StoryId(-9),
            })
            .expect("old story")
            .is_none()
        );
        let canonical = StoryKey {
            poster,
            story_id: StoryId(97),
        };
        assert!(read.story(&canonical).expect("new story").is_some());
        let appearances = read.story_appearances(&canonical).expect("new appearances");
        assert_eq!(appearances.len(), 1);
        assert_eq!(appearances[0].location, StoryAppearanceLocation::Active);
    }

    #[test]
    fn protected_story_commit_discards_locator_shaped_metadata() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        let mut protected = story(92, true, StoryContentKind::Protected);
        protected.can_be_forwarded = true;
        protected.mime_type = Some("secret/remote-file-id".to_owned());
        protected.exact_size = Some(999_999);
        protected.content_version = "secret/remote-file-id/version".to_owned();
        apply_story_commit(&mut store, scope(), &StoryCommit::Upsert(protected), 2_000)
            .expect("protected placeholder");

        let key = StoryKey {
            poster: ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            },
            story_id: StoryId(92),
        };
        let stored = store
            .read_txn()
            .expect("read")
            .story(&key)
            .expect("story")
            .expect("placeholder");
        assert_eq!(stored.facts.content_state, StoryContentState::Protected);
        assert_eq!(stored.facts.mime_type, None);
        assert_eq!(stored.facts.exact_size, None);
        assert!(!stored.facts.can_be_forwarded);
        assert_eq!(stored.blob_hash, None);
        assert!(stored.locators.is_empty());
        assert!(!stored.facts.content_version.as_str().contains("secret"));
    }

    #[test]
    fn protected_profile_story_projects_an_unavailable_placeholder_without_export_leakage() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        let mut protected = story(193, true, StoryContentKind::Protected);
        protected.can_be_forwarded = true;
        protected.mime_type = Some("secret-caption-and-locator".to_owned());
        protected.exact_size = Some(999_999);
        protected.content_version = "secret-caption-and-locator".to_owned();
        apply_story_commit(&mut store, scope(), &StoryCommit::Upsert(protected), 2_000)
            .expect("protected profile story");
        rebuild_projection(&mut store, scope()).expect("protected projection");

        let key = StoryKey {
            poster: ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            },
            story_id: StoryId(193),
        };
        let item = ItemKey::StoryAppearance(StoryAppearanceKey {
            story: key,
            view: ChatListKind::Main,
            location: StoryAppearanceLocation::Month {
                year: 2024,
                month: 7,
            },
        })
        .id();
        let read = store.read_txn().expect("read placeholder");
        let placeholder = read.item(&item).expect("item query").expect("placeholder");
        assert_eq!(placeholder.availability, ItemAvailability::Restricted);
        assert_eq!(
            placeholder
                .content
                .as_ref()
                .and_then(|facts| facts.mime_type.as_deref()),
            None
        );
        assert_eq!(
            placeholder
                .content
                .as_ref()
                .and_then(|facts| facts.logical_size),
            None
        );
        let canonical = read.story(&key).expect("story query").expect("story");
        assert!(canonical.locators.is_empty());
        assert_eq!(canonical.blob_hash, None);
        drop(read);

        let cache_root = std::env::temp_dir().join(format!(
            "gramdrive-protected-story-render-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&cache_root);
        render_pending_months(&mut store, &cache_root, 2_100).expect("render month exports");
        let exports = render_facts(&mut store, key.poster, 2024, 7);
        assert_eq!(exports.len(), 2);
        for export in exports {
            let text = String::from_utf8(export.bytes).expect("text export");
            assert!(!text.contains("secret-caption-and-locator"));
        }
        let _ = std::fs::remove_dir_all(cache_root);
    }

    #[test]
    fn chat_protection_redacts_existing_story_and_relaunches_after_removal() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::Upsert(story(95, true, StoryContentKind::Video)),
            1_000,
        )
        .expect("allowed story");
        let key = StoryKey {
            poster: ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            },
            story_id: StoryId(95),
        };
        let hash = ContentHash::Sha256([0x95; 32]);
        let allowed = store
            .read_txn()
            .expect("read allowed")
            .story(&key)
            .expect("story")
            .expect("allowed");
        assert_eq!(allowed.locators.len(), 1);
        assert_eq!(allowed.locators[0].role, "video-primary");
        assert!(allowed.locators[0].is_primary);
        assert_eq!(allowed.locators[0].local_file_id, Some(95));
        let txn = store.write_txn().expect("write blob");
        txn.record_blob(scope().account, &hash, 4_096, 1_100)
            .expect("blob");
        txn.link_story_blob(&key, &hash, 1_200).expect("link");
        txn.commit().expect("commit blob");

        let mut batch = UpdateBatch::default();
        batch.chats.push(ChatMetadata {
            chat_id: 100,
            kind: SnapshotChatKind::Private,
            title: "Chat 100".to_owned(),
            username: None,
            is_protected: true,
            photo: None,
        });
        apply_update_batch(&mut store, scope(), &batch).expect("protect chat");
        let read = store.read_txn().expect("read protected");
        let protected = read.story(&key).expect("story").expect("placeholder");
        assert_eq!(protected.facts.content_state, StoryContentState::Protected);
        assert_eq!(protected.facts.mime_type, None);
        assert_eq!(protected.facts.exact_size, None);
        assert_eq!(protected.blob_hash, None);
        assert!(protected.locators.is_empty());
        let progress = read
            .story_sync_progress(&key.poster)
            .expect("progress")
            .expect("seeded");
        assert_eq!(progress.phase, StorySyncPhase::Unavailable);
        assert_eq!(progress.failure_category.as_deref(), Some("chat-protected"));
        drop(read);

        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::Upsert(story(96, true, StoryContentKind::Video)),
            2_000,
        )
        .expect("buffered update while protected");
        let protected_live_key = StoryKey {
            poster: key.poster,
            story_id: StoryId(96),
        };
        let protected_live = store
            .read_txn()
            .expect("read protected update")
            .story(&protected_live_key)
            .expect("story")
            .expect("placeholder");
        assert_eq!(
            protected_live.facts.content_state,
            StoryContentState::Protected
        );
        assert_eq!(protected_live.facts.mime_type, None);
        assert_eq!(protected_live.facts.exact_size, None);
        assert!(protected_live.locators.is_empty());

        batch.chats[0].is_protected = false;
        apply_update_batch(&mut store, scope(), &batch).expect("remove protection");
        let resumed = store
            .read_txn()
            .expect("read resumed")
            .story_sync_progress(&key.poster)
            .expect("progress")
            .expect("seeded");
        assert_eq!(resumed.phase, StorySyncPhase::Pending);
        assert_eq!(
            resumed.archive_eligibility,
            StoryArchiveEligibility::Unknown
        );
        assert_eq!(resumed.failure_category, None);
    }

    #[test]
    fn profile_page_and_cursor_commit_share_one_transaction() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ProfilePage {
                chat_id: 100,
                generation: 3,
                stories: vec![story(93, true, StoryContentKind::Photo)],
                pinned_story_ids: vec![93],
                next_from_story_id: Some(93),
                complete: false,
            },
            3_000,
        )
        .expect("profile page");

        let chat = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let key = StoryKey {
            poster: chat,
            story_id: StoryId(93),
        };
        let read = store.read_txn().expect("read");
        assert!(read.story(&key).expect("story").is_some());
        let appearance = read
            .story_appearances(&key)
            .expect("appearances")
            .into_iter()
            .next()
            .expect("profile appearance");
        assert_eq!(appearance.profile_scan_generation, Some(3));
        assert_eq!(appearance.profile_pin_order, Some(0));
        let progress = read
            .story_sync_progress(&chat)
            .expect("progress")
            .expect("seeded");
        assert_eq!(progress.profile_cursor, Some(93));
        assert_eq!(progress.pages_committed, 1);
        assert_eq!(progress.stories_seen, 1);
    }

    #[test]
    fn active_list_progress_commit_is_durable_across_relaunch_passes() {
        let mut store = store();
        let txn = store.write_txn().expect("start list pass");
        txn.start_story_list_pass(scope(), 1_000).expect("start");
        txn.commit().expect("commit start");
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveListProgress { complete: false },
            1_100,
        )
        .expect("bounded page");
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ActiveListProgress { complete: true },
            1_200,
        )
        .expect("exhaustion");
        let exhausted = store
            .read_txn()
            .expect("read exhausted")
            .story_list_progress(scope())
            .expect("progress")
            .expect("seeded");
        assert_eq!(exhausted.generation, 1);
        assert_eq!(exhausted.pages_loaded, 1);
        assert!(exhausted.complete);

        let txn = store.write_txn().expect("relaunch list pass");
        txn.start_story_list_pass(scope(), 2_000).expect("restart");
        txn.commit().expect("commit restart");
        let relaunched = store
            .read_txn()
            .expect("read relaunched")
            .story_list_progress(scope())
            .expect("progress")
            .expect("seeded");
        assert_eq!(relaunched.generation, 2);
        assert_eq!(relaunched.pages_loaded, 1);
        assert!(!relaunched.complete);
    }

    #[test]
    fn owned_story_list_persists_unknown_chat_before_story_commit_across_relaunch() {
        let mut state = store();

        ingest_unknown_chat_active_story_list_pass(&mut state);
        ingest_unknown_chat_active_story_list_pass(&mut state);

        let chat = ChatKey {
            scope: scope(),
            chat_id: ChatId(777),
        };
        let story = StoryKey {
            poster: chat,
            story_id: StoryId(12),
        };
        assert_eq!(
            state
                .connection()
                .query_row(
                    "SELECT count(*) FROM stories
                     WHERE account_id = ?1 AND namespace_version = ?2
                       AND poster_chat_id = ?3 AND story_id = ?4",
                    [7_i64, 1, 777, 12],
                    |row| row.get::<_, i64>(0),
                )
                .expect("canonical story count"),
            1,
            "duplicate replay keeps one canonical story row"
        );
        let read = state.read_txn().expect("read replayed state");
        let appearances = read.story_appearances(&story).expect("active appearances");
        assert_eq!(appearances.len(), 1);
        assert_eq!(appearances[0].location, StoryAppearanceLocation::Active);
        let chat_progress = read
            .story_sync_progress(&chat)
            .expect("chat progress")
            .expect("chat progress seeded with metadata");
        assert!(chat_progress.active_complete);
        assert_eq!(chat_progress.pages_committed, 2);
        assert_eq!(chat_progress.stories_seen, 2);
        assert_eq!(chat_progress.failure_category, None);
        let account_progress = read
            .story_list_progress(scope())
            .expect("account progress")
            .expect("account progress seeded");
        assert_eq!(account_progress.generation, 2);
        assert_eq!(account_progress.pages_loaded, 2);
        assert!(!account_progress.complete);
    }

    #[test]
    fn story_short_profile_page_preserves_unseen_appearances_until_terminal_page() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        let chat = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ProfilePage {
                chat_id: 100,
                generation: 6,
                stories: vec![
                    story(80, true, StoryContentKind::Photo),
                    story(70, true, StoryContentKind::Photo),
                ],
                pinned_story_ids: Vec::new(),
                next_from_story_id: None,
                complete: true,
            },
            2_000,
        )
        .expect("prior profile snapshot");

        let mut progress = store
            .read_txn()
            .expect("read progress")
            .story_sync_progress(&chat)
            .expect("progress")
            .expect("seeded");
        progress.phase = StorySyncPhase::Syncing;
        progress.active_complete = true;
        progress.profile_cursor = None;
        progress.profile_scan_generation = 7;
        progress.profile_complete = false;
        let txn = store.write_txn().expect("start next profile scan");
        txn.put_story_sync_progress(&chat, &progress)
            .expect("persist next profile scan");
        txn.commit().expect("commit next profile scan");

        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ProfilePage {
                chat_id: 100,
                generation: 7,
                stories: vec![story(100, true, StoryContentKind::Photo)],
                pinned_story_ids: vec![100],
                next_from_story_id: Some(100),
                complete: false,
            },
            3_000,
        )
        .expect("commit short first page");

        for story_id in [80, 70] {
            assert!(
                store
                    .read_txn()
                    .expect("read unseen appearance")
                    .story(&StoryKey {
                        poster: chat,
                        story_id: StoryId(story_id),
                    })
                    .expect("story")
                    .is_some(),
                "short first page must not remove unseen story {story_id}"
            );
        }

        let committed = store
            .read_txn()
            .expect("read committed cursor")
            .story_sync_progress(&chat)
            .expect("progress")
            .expect("seeded");
        assert_eq!(committed.profile_cursor, Some(100));
        assert!(!committed.profile_complete);
        let mut relaunched = StoryMachine::new(7, StoryAccountKind::Regular).expect("machine");
        relaunched
            .enqueue_chat(StoryChatPlan {
                chat_id: 100,
                chat_kind: StoryChatKind::Private,
                cursor: StoryScanCursor {
                    active_complete: committed.active_complete,
                    profile_cursor: committed.profile_cursor,
                    profile_scan_generation: committed.profile_scan_generation,
                    profile_complete: committed.profile_complete,
                    archive_capability: SourceStoryArchiveCapability::Unknown,
                    archive_cursor: committed.archive_cursor,
                    archive_complete: committed.archive_complete,
                },
            })
            .expect("resume committed cursor");
        let StoryStep::Submit(request) = relaunched.next_step().expect("resume step") else {
            panic!("expected resumed profile request")
        };
        assert_eq!(
            request.get("@type").and_then(|value| value.as_str()),
            Some("getChatPostedToChatPageStories")
        );
        assert_eq!(
            request
                .get("from_story_id")
                .and_then(|value| value.as_i64()),
            Some(100)
        );
        drop(committed);

        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ProfilePage {
                chat_id: 100,
                generation: 7,
                stories: vec![story(80, true, StoryContentKind::Photo)],
                pinned_story_ids: Vec::new(),
                next_from_story_id: Some(80),
                complete: false,
            },
            4_000,
        )
        .expect("commit second short page");
        assert!(
            store
                .read_txn()
                .expect("read still unseen")
                .story(&StoryKey {
                    poster: chat,
                    story_id: StoryId(70),
                })
                .expect("story")
                .is_some()
        );

        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ProfilePage {
                chat_id: 100,
                generation: 7,
                stories: Vec::new(),
                pinned_story_ids: Vec::new(),
                next_from_story_id: None,
                complete: true,
            },
            5_000,
        )
        .expect("commit terminal page");
        let read = store.read_txn().expect("read reconciled profile");
        assert!(
            read.story(&StoryKey {
                poster: chat,
                story_id: StoryId(80),
            })
            .expect("observed story")
            .is_some()
        );
        assert!(
            read.story(&StoryKey {
                poster: chat,
                story_id: StoryId(70),
            })
            .expect("removed story")
            .is_none(),
            "absence is authoritative only after the true terminal page"
        );
    }

    #[test]
    fn unavailable_and_proven_ineligible_archive_rights_have_distinct_completion() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ArchiveCapability {
                chat_id: 100,
                capability: SourceStoryArchiveCapability::RightsUnavailable,
            },
            3_000,
        )
        .expect("persist unavailable rights");

        let progress = store
            .read_txn()
            .expect("read")
            .story_sync_progress(&ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            })
            .expect("progress")
            .expect("seeded");
        assert_eq!(
            progress.archive_eligibility,
            StoryArchiveEligibility::RightsUnavailable
        );
        assert!(!progress.archive_complete);
        drop(progress);

        apply_story_commit(
            &mut store,
            scope(),
            &StoryCommit::ArchiveCapability {
                chat_id: 100,
                capability: SourceStoryArchiveCapability::Ineligible,
            },
            4_000,
        )
        .expect("persist proven ineligibility");
        let proven = store
            .read_txn()
            .expect("read proven state")
            .story_sync_progress(&ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            })
            .expect("progress")
            .expect("seeded");
        assert_eq!(
            proven.archive_eligibility,
            StoryArchiveEligibility::Ineligible
        );
        assert!(proven.archive_complete);
    }

    fn message(chat_id: i64, message_id: i64, text: &str) -> MessageRecord {
        message_at(chat_id, message_id, text, 1_700_000_000 + message_id)
    }

    fn message_at(chat_id: i64, message_id: i64, text: &str, sent_at_secs: i64) -> MessageRecord {
        normalize_message(&json!({
            "@type": "message",
            "id": message_id,
            "chat_id": chat_id,
            "date": sent_at_secs,
            "sender_id": {"@type": "messageSenderUser", "user_id": 77},
            "can_be_saved": true,
            "content": {
                "@type": "messageText",
                "text": {"@type": "formattedText", "text": text, "entities": []}
            }
        }))
        .expect("message normalizes")
    }

    fn td_history_message_at(
        chat_id: i64,
        message_id: i64,
        sent_at_secs: i64,
    ) -> serde_json::Value {
        json!({
            "@type": "message",
            "id": message_id,
            "chat_id": chat_id,
            "date": sent_at_secs,
            "sender_id": {"@type": "messageSenderUser", "user_id": 77},
            "can_be_saved": true,
            "content": {
                "@type": "messageText",
                "text": {
                    "@type": "formattedText",
                    "text": format!("m{message_id}"),
                    "entities": [],
                },
            },
        })
    }

    fn image_document_at(
        chat_id: i64,
        message_id: i64,
        sent_at_secs: i64,
        file_id: i32,
    ) -> MessageRecord {
        normalize_message(&json!({
            "@type": "message",
            "id": message_id,
            "chat_id": chat_id,
            "date": sent_at_secs,
            "sender_id": {"@type": "messageSenderUser", "user_id": 77},
            "can_be_saved": true,
            "content": {
                "@type": "messageDocument",
                "caption": {"@type": "formattedText", "text": "", "entities": []},
                "document": {
                    "file_name": "scan?.png",
                    "mime_type": "image/png",
                    "document": {
                        "@type": "file",
                        "id": file_id,
                        "size": 4096,
                        "remote": {
                            "id": format!("remote-{file_id}"),
                            "unique_id": format!("unique-{file_id}")
                        }
                    }
                }
            }
        }))
        .expect("image document normalizes")
    }

    fn image_document_revision(
        local_file_id: i32,
        remote_file_id: &str,
        remote_unique_id: Option<&str>,
        preview_file_id: i32,
        preview_generation: i64,
        edited_at_secs: Option<i64>,
    ) -> MessageRecord {
        let mut wire = json!({
            "@type": "message",
            "id": 40,
            "chat_id": 100,
            "date": 1_700_000_000,
            "sender_id": {"@type": "messageSenderUser", "user_id": 77},
            "can_be_saved": true,
            "content": {
                "@type": "messageDocument",
                "caption": {"@type": "formattedText", "text": "", "entities": []},
                "document": {
                    "file_name": "stable.png",
                    "mime_type": "image/png",
                    "width": 640,
                    "height": 480,
                    "document": {
                        "@type": "file",
                        "id": local_file_id,
                        "size": 4096,
                        "remote": {
                            "id": remote_file_id,
                            "unique_id": remote_unique_id
                        }
                    },
                    "thumbnail": {
                        "format": {"@type": "thumbnailFormatJpeg"},
                        "width": 64 + preview_generation,
                        "height": 48 + preview_generation,
                        "file": {
                            "@type": "file",
                            "id": preview_file_id,
                            "size": 256 + preview_generation,
                            "remote": {
                                "id": format!("preview-remote-{preview_generation}"),
                                "unique_id": format!("preview-unique-{preview_generation}")
                            }
                        }
                    },
                    "minithumbnail": {
                        "width": 16,
                        "height": 12,
                        "data": format!("preview-inline-{preview_generation}")
                    }
                }
            }
        });
        if let Some(edited_at_secs) = edited_at_secs {
            wire["edit_date"] = json!(edited_at_secs);
        }
        normalize_message(&wire).expect("image document revision normalizes")
    }

    fn attachment_message_at(
        message_id: i64,
        can_be_saved: bool,
        self_destruct_type: Option<serde_json::Value>,
        content: serde_json::Value,
    ) -> MessageRecord {
        let mut wire = json!({
            "@type": "message",
            "id": message_id,
            "chat_id": 100,
            "date": 1_700_000_000 + message_id,
            "sender_id": {"@type": "messageSenderUser", "user_id": 77},
            "can_be_saved": can_be_saved,
            "content": content,
        });
        if let Some(self_destruct_type) = self_destruct_type {
            wire["self_destruct_type"] = self_destruct_type;
        }
        normalize_message(&wire).expect("attachment message normalizes")
    }

    fn colliding_attachment_names(records: Vec<MessageRecord>) -> BTreeMap<i64, String> {
        let mut store = store();
        let txn = store.write_txn().expect("write metadata");
        txn.set_display_timezone(scope().account, "Asia/Tbilisi", 10)
            .expect("display timezone");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");

        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records,
                window: Some(CrawlWindow {
                    oldest_message_id: 10,
                    newest_message_id: 20,
                }),
                history_complete: true,
                skipped_malformed: 0,
            },
            2_000,
        )
        .expect("history with attachments");

        let read = store.read_txn().expect("read projection");
        let mut names = BTreeMap::new();
        for (message_id, file_id) in [(10, 510), (20, 520)] {
            let key = AttachmentKey {
                message: MessageKey {
                    chat: ChatKey {
                        scope: scope(),
                        chat_id: ChatId(100),
                    },
                    message_id: MessageId(message_id),
                },
                index: AttachmentIndex(0),
            };
            let attachments = read
                .attachments_of_message(&key.message)
                .expect("attachment rows");
            assert_eq!(attachments.len(), 1);
            let facts = &attachments[0].facts;
            assert_eq!(facts.logical_kind, StateAttachmentLogicalKind::Photo);
            assert_eq!(
                facts.telegram_representation,
                StateTelegramRepresentation::OriginalDocument
            );
            assert_eq!(facts.fidelity, StateAttachmentFidelity::Original);
            assert_eq!(facts.source_name.as_deref(), Some("scan?.png"));
            assert_eq!(facts.mime_type.as_deref(), Some("image/png"));
            assert_eq!(facts.exact_size, Some(4096));
            assert_eq!(facts.telegram_local_file_id, Some(file_id));
            assert_eq!(
                facts.telegram_unique_id.as_deref(),
                Some(format!("unique-{file_id}").as_str())
            );
            assert_eq!(facts.availability, StateAttachmentAvailability::Fetchable);

            let canonical = ItemKey::Canonical(CanonicalKey::Attachment(key)).id();
            let appearances = read.appearances_of(&canonical).expect("appearances");
            assert_eq!(appearances.len(), 1);
            let item = &appearances[0];
            assert!(item.safe_name.starts_with("2023-11-15 02-13-20 scan_"));
            assert!(item.safe_name.ends_with(".png"));
            assert_eq!(
                item.content
                    .as_ref()
                    .and_then(|content| content.mime_type.as_deref()),
                Some("image/png")
            );
            assert_eq!(
                item.content
                    .as_ref()
                    .and_then(|content| content.logical_size),
                Some(4096)
            );
            assert!(
                item.content
                    .as_ref()
                    .and_then(|content| content.content_version.as_ref())
                    .is_some()
            );
            assert_eq!(item.created_at_ms, Some(1_700_000_000_000));
            names.insert(message_id, item.safe_name.clone());
        }
        names
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct RenderFact {
        item_id: Vec<u8>,
        content_version: String,
        modified_at_ms: i64,
        path: String,
        bytes: Vec<u8>,
    }

    fn render_facts(
        store: &mut StateStore,
        chat: ChatKey,
        year: u16,
        month: u8,
    ) -> Vec<RenderFact> {
        let txn = store.read_txn().expect("read render facts");
        let catalog = txn
            .month_render_catalog(chat, year, month)
            .expect("month catalog");
        let mut facts = Vec::new();
        for entry in catalog {
            let item = txn.item(&entry.item).expect("item read").expect("item");
            let cache = txn
                .cache_entry(&entry.item)
                .expect("cache read")
                .expect("cache");
            let version = item
                .content
                .and_then(|content| content.content_version)
                .expect("content version")
                .as_str()
                .to_owned();
            let path = cache.materialization_ref.expect("materialization path");
            let bytes = std::fs::read(&path).expect("materialized bytes");
            facts.push(RenderFact {
                item_id: entry.item.as_bytes().to_vec(),
                content_version: version,
                modified_at_ms: item.modified_at_ms.expect("modified time"),
                path,
                bytes,
            });
        }
        facts.sort_by(|left, right| left.item_id.cmp(&right.item_id));
        facts
    }

    fn drain_render_jobs(store: &mut StateStore, cache_root: &Path, observed_at_ms: i64) {
        for tick in 0..64 {
            let pending = {
                let read = store.read_txn().expect("read render worklist");
                plan_worklist(&read, u32::MAX)
                    .expect("render worklist")
                    .jobs
                    .len()
            };
            if pending == 0 {
                return;
            }
            render_pending_months(store, cache_root, observed_at_ms + tick)
                .expect("bounded render");
        }
        panic!("bounded render worklist did not drain");
    }

    #[test]
    fn multi_appearance_projection_keeps_one_canonical_chat() {
        let mut store = store();
        let txn = store.write_txn().expect("write");
        txn.replace_folders(
            scope(),
            &[FolderRecord {
                scope: scope(),
                folder_id: gramdrive_model::identity::FolderId(9),
                title: "Pinned".to_owned(),
                position: 0,
            }],
        )
        .expect("folders");
        let record = snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record");
        txn.upsert_chat(&record).expect("chat");
        for kind in [
            ChatListKind::Main,
            ChatListKind::Folder(gramdrive_model::identity::FolderId(9)),
        ] {
            txn.replace_chat_list(
                &ChatListKey {
                    scope: scope(),
                    kind,
                },
                &[ChatListEntry {
                    chat_id: ChatId(100),
                    sort_order: 10,
                    pinned: false,
                }],
            )
            .expect("membership");
        }
        txn.commit().expect("commit");
        rebuild_projection(&mut store, scope()).expect("projection");

        let read = store.read_txn().expect("read");
        assert_eq!(read.chats(scope()).expect("chats").len(), 1);
        let canonical = ItemKey::Canonical(CanonicalKey::Chat(ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        }))
        .id();
        assert_eq!(
            read.appearances_of(&canonical).expect("appearances").len(),
            2
        );
    }

    #[test]
    fn chat_scoped_history_publication_preserves_sibling_chat_subtrees() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        add_main_chat(&mut store, 200, false);
        rebuild_projection(&mut store, scope()).expect("initial projection");

        let commit = |chat_id, message_id| HistoryCommit {
            chat_id,
            records: vec![message_at(chat_id, message_id, "history", 1_784_937_600)],
            window: Some(CrawlWindow {
                oldest_message_id: message_id,
                newest_message_id: message_id,
            }),
            history_complete: true,
            skipped_malformed: 0,
        };
        apply_history_commit(&mut store, scope(), &commit(200, 20), 1_000)
            .expect("publish sibling chat");
        let sibling_month = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Main,
            item: CanonicalKey::MonthDir(MonthDirKey {
                chat: ChatKey {
                    scope: scope(),
                    chat_id: ChatId(200),
                },
                year: 2026,
                month: 7,
            }),
        })
        .id();
        assert!(
            store
                .read_txn()
                .expect("read sibling")
                .item(&sibling_month)
                .expect("sibling item")
                .is_some()
        );

        apply_history_commit(&mut store, scope(), &commit(100, 10), 2_000)
            .expect("publish target chat");
        let sibling = store
            .read_txn()
            .expect("read preserved sibling")
            .item(&sibling_month)
            .expect("sibling item")
            .expect("sibling month remains");
        assert_eq!(sibling.deleted_at_ms, None);
    }

    #[test]
    fn removing_a_folder_removes_only_its_appearance() {
        let mut store = store();
        let txn = store.write_txn().expect("write");
        let folder = gramdrive_model::identity::FolderId(9);
        txn.replace_folders(
            scope(),
            &[FolderRecord {
                scope: scope(),
                folder_id: folder,
                title: "Pinned".to_owned(),
                position: 0,
            }],
        )
        .expect("folders");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        for kind in [ChatListKind::Main, ChatListKind::Folder(folder)] {
            txn.replace_chat_list(
                &ChatListKey {
                    scope: scope(),
                    kind,
                },
                &[ChatListEntry {
                    chat_id: ChatId(100),
                    sort_order: 10,
                    pinned: false,
                }],
            )
            .expect("membership");
        }
        txn.commit().expect("commit");
        rebuild_projection(&mut store, scope()).expect("projection");

        let txn = store.write_txn().expect("write");
        txn.replace_folders(scope(), &[]).expect("remove folder");
        txn.commit().expect("commit");
        rebuild_projection(&mut store, scope()).expect("projection");
        let read = store.read_txn().expect("read");
        assert_eq!(
            read.chats(scope()).expect("canonical chat remains").len(),
            1
        );
        assert_eq!(
            read.chat_list(&ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            })
            .expect("main")
            .len(),
            1
        );
        assert!(
            read.chat_list(&ChatListKey {
                scope: scope(),
                kind: ChatListKind::Folder(folder),
            })
            .expect("folder")
            .is_empty()
        );
    }

    #[test]
    fn witnessed_final_membership_removal_tombstones_without_projecting_unlisted_chats() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        let unlisted = snapshot_chat_record(scope(), &chat(200, "Unlisted")).expect("record");
        let txn = store.write_txn().expect("write unlisted canonical chat");
        txn.upsert_chat(&unlisted).expect("unlisted chat");
        txn.commit().expect("commit unlisted chat");
        rebuild_projection(&mut store, scope()).expect("initial projection");

        let listed_key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let listed_item = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Main,
            item: CanonicalKey::Chat(listed_key),
        })
        .id();
        let unlisted_canonical = ItemKey::Canonical(CanonicalKey::Chat(unlisted.key)).id();
        assert!(
            store
                .read_txn()
                .expect("read initial")
                .appearances_of(&unlisted_canonical)
                .expect("unlisted appearances")
                .is_empty(),
            "canonical TDLib chats outside every list never enter projection"
        );

        let txn = store.write_txn().expect("remove final membership");
        let mut departed = snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record");
        departed.left_at_ms = Some(2_000);
        txn.upsert_chat(&departed).expect("departure witness");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[],
        )
        .expect("empty main list");
        txn.commit().expect("commit removal");
        rebuild_chat_projection(&mut store, listed_key).expect("fallback listed-account diff");
        let removed = store
            .read_txn()
            .expect("read removal")
            .item(&listed_item)
            .expect("appearance lookup")
            .expect("tombstone retained");
        assert!(removed.deleted_at_ms.is_some());
    }

    #[test]
    fn observed_1153_folder_and_1156_main_short_snapshots_are_journal_quiet_and_resumable() {
        // The installed incident had 140 disappeared chats and 32,144
        // descendant item tombstones. This fixture pins both source-shaped
        // short list commits (folder at 11:53, Main at 11:56) against the
        // 140-chat root cause; no chat carries a departure witness.
        const INCIDENT_CHAT_COUNT: i64 = 140;
        let folder = gramdrive_model::identity::FolderId(53);
        let mut store = store();
        let entries: Vec<_> = (1..=INCIDENT_CHAT_COUNT)
            .map(|chat_id| ChatListEntry {
                chat_id: ChatId(chat_id),
                sort_order: chat_id,
                pinned: false,
            })
            .collect();
        let txn = store.write_txn().expect("seed incident fixture");
        txn.replace_folders(
            scope(),
            &[FolderRecord {
                scope: scope(),
                folder_id: folder,
                title: "Observed folder".to_owned(),
                position: 0,
            }],
        )
        .expect("folder");
        for chat_id in 1..=INCIDENT_CHAT_COUNT {
            txn.upsert_chat(
                &snapshot_chat_record(scope(), &chat(chat_id, &format!("Chat {chat_id}")))
                    .expect("chat"),
            )
            .expect("chat row");
        }
        for list in [ChatListKind::Main, ChatListKind::Folder(folder)] {
            txn.replace_chat_list(
                &ChatListKey {
                    scope: scope(),
                    kind: list,
                },
                &entries,
            )
            .expect("baseline membership");
        }
        txn.put_namespace_bootstrap(&NamespaceBootstrapRecord {
            scope: scope(),
            resume_token: b"baseline-resume".to_vec(),
            updated_at_ms: 11 * 60 * 60 * 1_000,
        })
        .expect("baseline checkpoint");
        txn.publish_namespace_readiness(scope(), 1_000)
            .expect("last-known-good readiness");
        txn.commit().expect("seed commit");
        rebuild_projection(&mut store, scope()).expect("baseline projection");
        let anchor = store
            .read_txn()
            .expect("read journal")
            .change_journal_state()
            .expect("journal")
            .latest_sequence;

        for (observed_at_ms, list) in [
            (
                11 * 60 * 60 * 1_000 + 53 * 60 * 1_000,
                ChatListKind::Folder(folder),
            ),
            (11 * 60 * 60 * 1_000 + 56 * 60 * 1_000, ChatListKind::Main),
        ] {
            let commit = ListCommit {
                list,
                chats: vec![chat(1, "Chat 1")],
                entries: vec![gramdrive_source_tdjson::ListEntrySnapshot {
                    chat_id: 1,
                    sort_order: 1,
                    pinned: false,
                }],
                total_count: Some(1),
                excluded_secret: 0,
                excluded_unsupported: 0,
                excluded_removed: 0,
                resume_token: format!("unsafe-{observed_at_ms}").into_bytes(),
            };
            let failure = apply_snapshot_commit(&mut store, scope(), &commit)
                .expect_err("short snapshot must fail closed");
            assert_eq!(failure.category, "snapshot-membership-incomplete");
            assert!(failure.retryable);

            let read = store.read_txn().expect("read preserved snapshot");
            assert_eq!(
                read.chat_list(&ChatListKey {
                    scope: scope(),
                    kind: list,
                })
                .expect("list")
                .len(),
                INCIDENT_CHAT_COUNT as usize,
                "the short snapshot may not remove membership"
            );
            assert_eq!(
                read.namespace_bootstrap(scope())
                    .expect("checkpoint")
                    .expect("prior checkpoint")
                    .resume_token,
                b"baseline-resume",
                "the unsafe commit cannot advance the resumable checkpoint"
            );
            let readiness = read
                .namespace_readiness(scope())
                .expect("readiness")
                .expect("last-known-good readiness");
            assert_eq!(readiness.generation, 1);
            assert_eq!(readiness.projection_after_chat_id, None);
            assert!(!readiness.convergence_complete);
        }

        rebuild_shallow_projection(&mut store, scope()).expect("stable shallow reconciliation");
        let changes = store
            .read_txn()
            .expect("read changes")
            .item_changes_since(scope().account, anchor, u32::MAX)
            .expect("changes");
        assert!(
            changes
                .iter()
                .all(|change| change.item.deleted_at_ms.is_none()),
            "a rejected snapshot emits no provider didDeleteItems"
        );
    }

    #[test]
    fn post_ready_projection_convergence_is_bounded_and_resumes_from_durable_cursor() {
        let mut store = store();
        let txn = store.write_txn().expect("seed convergence");
        let entries = (1..=3)
            .map(|chat_id| ChatListEntry {
                chat_id: ChatId(chat_id),
                sort_order: chat_id,
                pinned: false,
            })
            .collect::<Vec<_>>();
        for chat_id in 1..=3 {
            txn.upsert_chat(
                &snapshot_chat_record(scope(), &chat(chat_id, &format!("Chat {chat_id}")))
                    .expect("chat"),
            )
            .expect("chat row");
        }
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &entries,
        )
        .expect("membership");
        txn.commit().expect("seed commit");
        rebuild_shallow_projection(&mut store, scope()).expect("shallow publication");
        let txn = store.write_txn().expect("publish readiness");
        txn.publish_namespace_readiness(scope(), 2_000)
            .expect("readiness");
        txn.commit().expect("readiness commit");

        assert_eq!(
            converge_projection_slice(&mut store, scope()).expect("slice one"),
            1
        );
        let first_cursor = store
            .read_txn()
            .expect("restart read")
            .namespace_readiness(scope())
            .expect("readiness")
            .expect("record")
            .projection_after_chat_id;
        assert_eq!(first_cursor, Some(ChatId(1)));

        assert_eq!(
            converge_projection_slice(&mut store, scope()).expect("slice two"),
            1
        );
        assert_eq!(
            converge_projection_slice(&mut store, scope()).expect("slice three"),
            1
        );
        assert_eq!(
            converge_projection_slice(&mut store, scope()).expect("finish"),
            0
        );
        let readiness = store
            .read_txn()
            .expect("read completion")
            .namespace_readiness(scope())
            .expect("readiness")
            .expect("record");
        assert_eq!(readiness.generation, 1);
        assert!(readiness.convergence_complete);
    }

    #[test]
    fn post_ready_projection_convergence_builds_a_fresh_chat_appearance() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        let chat_key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let canonical = ItemKey::Canonical(CanonicalKey::Chat(chat_key)).id();
        assert!(
            store
                .read_txn()
                .expect("read fresh state")
                .appearances_of(&canonical)
                .expect("fresh appearances")
                .is_empty()
        );

        let txn = store.write_txn().expect("publish readiness");
        txn.publish_namespace_readiness(scope(), 2_000)
            .expect("readiness");
        txn.commit().expect("readiness commit");

        assert_eq!(
            converge_projection_slice(&mut store, scope()).expect("fresh convergence slice"),
            1
        );
        let read = store.read_txn().expect("read projected state");
        let appearances = read.appearances_of(&canonical).expect("appearances");
        assert_eq!(appearances.len(), 1);
        assert!(
            read.children_page(&appearances[0].id, None, u32::MAX)
                .expect("chat children")
                .iter()
                .any(|child| matches!(
                    child.id.key(),
                    ItemKey::Appearance(AppearanceKey {
                        item: CanonicalKey::GeneratedDoc(_),
                        ..
                    })
                )),
            "fresh full-scope convergence creates generated-document children"
        );
    }

    #[test]
    fn post_ready_projection_convergence_repairs_partial_chat_appearances_and_is_restart_idempotent()
     {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let database = std::env::temp_dir().join(format!(
            "gramdrive-partial-chat-appearance-{}-{unique}.sqlite3",
            std::process::id()
        ));
        let folder = gramdrive_model::identity::FolderId(9);
        let target = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let sibling = ChatKey {
            scope: scope(),
            chat_id: ChatId(200),
        };
        let mut store = initialized_store(StateStore::open(&database).expect("open durable store"));
        let txn = store.write_txn().expect("seed partial appearance fixture");
        txn.replace_folders(
            scope(),
            &[FolderRecord {
                scope: scope(),
                folder_id: folder,
                title: "Pinned".to_owned(),
                position: 0,
            }],
        )
        .expect("folder");
        for chat_key in [target, sibling] {
            txn.upsert_chat(
                &snapshot_chat_record(
                    scope(),
                    &chat(chat_key.chat_id.0, &format!("Chat {}", chat_key.chat_id.0)),
                )
                .expect("chat record"),
            )
            .expect("chat row");
        }
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: target.chat_id,
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("target main membership");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Folder(folder),
            },
            &[ChatListEntry {
                chat_id: sibling.chat_id,
                sort_order: 20,
                pinned: false,
            }],
        )
        .expect("sibling folder membership");
        txn.commit().expect("seed commit");
        rebuild_projection(&mut store, scope()).expect("initial complete projection");

        let txn = store.write_txn().expect("introduce partial appearance");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Folder(folder),
            },
            &[
                ChatListEntry {
                    chat_id: target.chat_id,
                    sort_order: 30,
                    pinned: false,
                },
                ChatListEntry {
                    chat_id: sibling.chat_id,
                    sort_order: 20,
                    pinned: false,
                },
            ],
        )
        .expect("current folder membership");
        txn.publish_namespace_readiness(scope(), 2_000)
            .expect("readiness");
        txn.commit().expect("partial fixture commit");

        let target_canonical = ItemKey::Canonical(CanonicalKey::Chat(target)).id();
        assert_eq!(
            store
                .read_txn()
                .expect("read partial fixture")
                .appearances_of(&target_canonical)
                .expect("stored target appearances")
                .into_iter()
                .filter(|appearance| appearance.deleted_at_ms.is_none())
                .count(),
            1,
            "the fixture must store only a subset of two current memberships"
        );

        assert_eq!(
            converge_projection_slice(&mut store, scope()).expect("repairing convergence slice"),
            1
        );
        let main_target = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Main,
            item: CanonicalKey::Chat(target),
        })
        .id();
        let folder_target = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Folder(folder),
            item: CanonicalKey::Chat(target),
        })
        .id();
        let folder_sibling = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Folder(folder),
            item: CanonicalKey::Chat(sibling),
        })
        .id();
        {
            let read = store.read_txn().expect("read repaired projection");
            let target_appearances = read
                .appearances_of(&target_canonical)
                .expect("target appearances")
                .into_iter()
                .filter(|appearance| appearance.deleted_at_ms.is_none())
                .collect::<Vec<_>>();
            assert_eq!(target_appearances.len(), 2);
            for appearance in [&main_target, &folder_target] {
                let item = read
                    .item(appearance)
                    .expect("appearance lookup")
                    .expect("appearance");
                assert_eq!(item.deleted_at_ms, None);
                assert!(
                    read.children_page(appearance, None, u32::MAX)
                        .expect("appearance children")
                        .iter()
                        .any(|child| matches!(
                            child.id.key(),
                            ItemKey::Appearance(AppearanceKey {
                                item: CanonicalKey::GeneratedDoc(_),
                                ..
                            })
                        )),
                    "every repaired appearance has its generated-document children"
                );
            }
            assert_eq!(
                read.item(&folder_sibling)
                    .expect("sibling lookup")
                    .expect("unrelated sibling")
                    .deleted_at_ms,
                None,
                "full-scope fallback must not tombstone unrelated siblings"
            );
            assert_eq!(
                read.namespace_readiness(scope())
                    .expect("readiness")
                    .expect("readiness row")
                    .projection_after_chat_id,
                Some(target.chat_id),
                "the durable cursor advances only after the repairing transaction succeeds"
            );
        }

        assert_eq!(
            converge_projection_slice(&mut store, scope()).expect("remaining chat slice"),
            1
        );
        assert_eq!(
            converge_projection_slice(&mut store, scope()).expect("complete convergence"),
            0
        );
        let journal_before_restart = store
            .read_txn()
            .expect("read journal")
            .change_journal_state()
            .expect("journal state")
            .latest_sequence;
        drop(store);

        let mut store = StateStore::open(&database).expect("restart durable store");
        assert_eq!(
            converge_projection_slice(&mut store, scope()).expect("idempotent restart slice"),
            0
        );
        assert_eq!(
            store
                .read_txn()
                .expect("read restarted journal")
                .change_journal_state()
                .expect("restarted journal state")
                .latest_sequence,
            journal_before_restart,
            "a completed restart emits no provider-visible projection changes"
        );
        drop(store);
        for path in [
            database.clone(),
            database.with_extension("sqlite3-wal"),
            database.with_extension("sqlite3-shm"),
        ] {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn large_list_pages_follow_telegram_order_without_gaps() {
        let mut store = store();
        let txn = store.write_txn().expect("write");
        let mut entries = Vec::new();
        for id in 1..=1_000 {
            txn.upsert_chat(
                &snapshot_chat_record(scope(), &chat(id, &format!("Chat {id}"))).expect("record"),
            )
            .expect("chat");
            entries.push(ChatListEntry {
                chat_id: ChatId(id),
                sort_order: id,
                pinned: false,
            });
        }
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &entries,
        )
        .expect("list");
        txn.commit().expect("commit");
        rebuild_projection(&mut store, scope()).expect("projection");

        let read = store.read_txn().expect("read");
        let parent = ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
            scope: scope(),
            kind: ChatListKind::Main,
        }))
        .id();
        let mut after = None;
        let mut seen = Vec::new();
        loop {
            let page = read
                .children_page(&parent, after.as_ref(), 37)
                .expect("page");
            if page.is_empty() {
                break;
            }
            after = page.last().map(|item| item.id.clone());
            seen.extend(page);
        }
        assert_eq!(seen.len(), 1_000);
        let ids: Vec<i64> = seen
            .iter()
            .map(|item| match item.id.key() {
                ItemKey::Appearance(AppearanceKey {
                    item: CanonicalKey::Chat(chat),
                    ..
                }) => chat.chat_id.0,
                _ => panic!("chat appearance"),
            })
            .collect();
        assert_eq!(ids.first(), Some(&1_000));
        assert_eq!(ids.last(), Some(&1));
        assert_eq!(ids.iter().copied().collect::<BTreeSet<_>>().len(), 1_000);
    }

    #[test]
    fn folder_catalog_enumerates_telegram_order_not_folder_identity() {
        let mut store = store();
        let txn = store.write_txn().expect("write");
        txn.replace_folders(
            scope(),
            &[
                FolderRecord {
                    scope: scope(),
                    folder_id: gramdrive_model::identity::FolderId(90),
                    title: "First".to_owned(),
                    position: 0,
                },
                FolderRecord {
                    scope: scope(),
                    folder_id: gramdrive_model::identity::FolderId(2),
                    title: "Second".to_owned(),
                    position: 1,
                },
            ],
        )
        .expect("folders");
        txn.commit().expect("commit");
        rebuild_projection(&mut store, scope()).expect("projection");

        let catalog = ItemKey::Canonical(CanonicalKey::FolderCatalog(
            gramdrive_model::identity::FolderCatalogKey { scope: scope() },
        ))
        .id();
        let read = store.read_txn().expect("read");
        let folders = read.children_page(&catalog, None, 100).expect("folders");
        assert_eq!(
            folders
                .iter()
                .map(|item| item.display_name.as_str())
                .collect::<Vec<_>>(),
            ["First", "Second"]
        );
    }

    #[test]
    fn snapshot_wait_drains_updates_larger_than_the_runtime_queue() {
        use gramdrive_source_tdjson::mock::MockTdJson;
        use gramdrive_source_tdjson::{RuntimeConfig, TdRuntime};

        let (sender, receiver, handle) = MockTdJson::new();
        handle.set_responder(|sent| {
            let mut events: Vec<_> = (0..20)
                .map(|index| {
                    json!({
                        "@type": "updateOption",
                        "name": format!("fixed-{index}"),
                        "value": {"@type": "optionValueEmpty"},
                        "@client_id": sent.client_id,
                    })
                    .to_string()
                })
                .collect();
            events.push(
                json!({
                    "@type": "ok",
                    "@extra": sent.extra().expect("extra"),
                    "@client_id": sent.client_id,
                })
                .to_string(),
            );
            events
        });
        let runtime = TdRuntime::start(
            sender,
            receiver,
            RuntimeConfig {
                receive_timeout: Duration::from_millis(5),
                update_queue_capacity: 2,
            },
        )
        .expect("runtime");
        let (client, updates) = runtime.create_client().expect("client");
        let mut machine =
            SnapshotMachine::new(SnapshotPlan::new(vec![ChatListKind::Main])).expect("machine");
        let SnapshotStep::Submit(request) = machine.next_step().expect("step") else {
            panic!("submit")
        };
        let pending = client.request(request).expect("request");
        let mut folders = FolderCatalogMachine::new();
        let mut live = UpdateMachine::new();
        let mut state = store();
        let cancelled = AtomicBool::new(false);

        let response = wait_for_snapshot_response(
            &mut state,
            scope(),
            pending,
            &mut machine,
            &mut folders,
            &mut live,
            None,
            None,
            &updates,
            &cancelled,
        )
        .expect("wait")
        .expect("response");
        assert_eq!(response["@type"], "ok");
        runtime.shutdown();
    }

    #[test]
    fn ready_item_local_metadata_failure_preserves_projection_and_later_recovers() {
        use gramdrive_source_tdjson::mock::MockTdJson;
        use gramdrive_source_tdjson::{RuntimeConfig, TdRuntime};
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        let mut state = store();
        add_main_chat(&mut state, 100, false);
        apply_history_commit(
            &mut state,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![message(100, 1, "preserved")],
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 1,
                }),
                history_complete: true,
                skipped_malformed: 0,
            },
            1_000,
        )
        .expect("seed preserved history");
        rebuild_projection(&mut state, scope()).expect("seed projection");
        initialize_content_progress(&mut state, scope()).expect("seed progress");

        let preserved_item = ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Main,
            item: CanonicalKey::Chat(ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            }),
        })
        .id();
        let preserved_chat = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let (preserved_cursor, preserved_retention) = {
            let read = state.read_txn().expect("read preserved state");
            assert!(
                read.item(&preserved_item)
                    .expect("read projected chat")
                    .is_some()
            );
            (
                read.chat_sync_state(&preserved_chat)
                    .expect("read cursor")
                    .expect("cursor exists"),
                read.account(scope().account)
                    .expect("read account")
                    .expect("account exists")
                    .retention_mode,
            )
        };

        let attempts = Arc::new(AtomicUsize::new(0));
        let responder_attempts = Arc::clone(&attempts);
        let (sender, receiver, handle) = MockTdJson::new();
        handle.set_responder(move |sent| {
            if responder_attempts.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                vec![
                    json!({
                        "@type": "error",
                        "code": 503,
                        "message": "synthetic unavailable",
                        "@extra": sent.extra().expect("correlation"),
                        "@client_id": sent.client_id,
                    })
                    .to_string(),
                ]
            } else {
                vec![
                    json!({
                        "@type": "chat",
                        "id": 999,
                        "type": {"@type": "chatTypePrivate", "user_id": 99},
                        "title": "Recovered chat",
                        "has_protected_content": false,
                        "positions": [{
                            "@type": "chatPosition",
                            "list": {"@type": "chatListMain"},
                            "order": "99",
                            "is_pinned": false
                        }],
                        "@extra": sent.extra().expect("correlation"),
                        "@client_id": sent.client_id,
                    })
                    .to_string(),
                ]
            }
        });
        let runtime = TdRuntime::start(
            sender,
            receiver,
            RuntimeConfig {
                receive_timeout: Duration::from_millis(5),
                update_queue_capacity: 16,
            },
        )
        .expect("runtime");
        let (client, updates) = runtime.create_client().expect("client");
        let cancelled = AtomicBool::new(false);
        let mut folders = FolderCatalogMachine::new();
        let mut metadata = UpdateMachine::new();
        let mut content = ContentCoordinator::new(&mut state, scope()).expect("coordinator");
        let concrete_listener = Arc::new(RecordingNamespaceProgress::default());
        let listener: Arc<dyn NamespaceProgressListener> = concrete_listener.clone();
        listener.on_progress(namespace_counts(&mut state, scope()).expect("initial ready"));

        let unknown_update = json!({
            "@type": "updateNewMessage",
            "message": {
                "@type": "message",
                "id": 7,
                "chat_id": 999,
                "date": 1_700_000_007,
                "sender_id": {"@type": "messageSenderUser", "user_id": 99},
                "can_be_saved": true,
                "content": {
                    "@type": "messageText",
                    "text": {"@type": "formattedText", "text": "new", "entities": []}
                }
            }
        });
        route_content_live_update(&mut state, scope(), &mut content.live, &unknown_update)
            .expect("route first unknown update");
        assert!(
            !drive_live_steps(
                &mut state,
                scope(),
                &mut folders,
                &mut metadata,
                &mut content,
                &client,
                &updates,
                &cancelled,
                &listener,
            )
            .expect("item-local failure stays in the session")
        );

        {
            let read = state.read_txn().expect("read after degradation");
            assert!(
                read.item(&preserved_item)
                    .expect("read projected chat")
                    .is_some()
            );
            assert_eq!(
                read.chat_sync_state(&preserved_chat)
                    .expect("read cursor")
                    .expect("cursor exists"),
                preserved_cursor
            );
            assert_eq!(
                read.account(scope().account)
                    .expect("read account")
                    .expect("account exists")
                    .retention_mode,
                preserved_retention
            );
        }
        assert!(matches!(
            concrete_listener.snapshot().as_slice(),
            [
                NamespaceProgress::Ready { .. },
                NamespaceProgress::Degraded {
                    category,
                    retryable: true
                }
            ] if category == "chat-metadata"
        ));

        route_content_live_update(&mut state, scope(), &mut content.live, &unknown_update)
            .expect("route recovery update");
        assert!(
            drive_live_steps(
                &mut state,
                scope(),
                &mut folders,
                &mut metadata,
                &mut content,
                &client,
                &updates,
                &cancelled,
                &listener,
            )
            .expect("later metadata recovery")
        );
        listener.on_progress(namespace_counts(&mut state, scope()).expect("recovered ready"));

        let read = state.read_txn().expect("read recovered state");
        assert!(
            read.chat(&ChatKey {
                scope: scope(),
                chat_id: ChatId(999),
            })
            .expect("read recovered chat")
            .is_some()
        );
        assert!(
            read.item(&preserved_item)
                .expect("read preserved projection")
                .is_some()
        );
        assert_eq!(
            read.chat_sync_state(&preserved_chat)
                .expect("read cursor")
                .expect("cursor exists"),
            preserved_cursor
        );
        drop(read);
        assert_eq!(
            concrete_listener.snapshot().last(),
            Some(&namespace_counts(&mut state, scope()).expect("final ready"))
        );
        runtime.shutdown();
    }

    #[test]
    fn delayed_snapshot_response_bounds_known_chat_live_state_and_relaunches_full_recovery() {
        use gramdrive_source_tdjson::live::MAX_TRACKED_REFRESHES_PER_CHAT;
        use gramdrive_source_tdjson::mock::MockTdJson;
        use gramdrive_source_tdjson::{RuntimeConfig, TdRuntime};

        let mut state = store();
        add_chat(&mut state, 100, false);
        apply_history_commit(
            &mut state,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![message(100, 1, "one")],
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 1,
                }),
                history_complete: true,
                skipped_malformed: 0,
            },
            1_000,
        )
        .expect("seed history");
        let mut content = ContentCoordinator::new(&mut state, scope()).expect("coordinator");

        let (sender, receiver, handle) = MockTdJson::new();
        handle.set_responder(move |sent| {
            let mut events = Vec::new();
            for message_id in 2..=600i64 {
                events.push(
                    json!({
                        "@type": "updateNewMessage",
                        "message": {
                            "@type": "message",
                            "id": message_id,
                            "chat_id": 100,
                            "date": 1_700_000_000 + message_id,
                            "sender_id": {"@type": "messageSenderUser", "user_id": 77},
                            "can_be_saved": true,
                            "content": {
                                "@type": "messageText",
                                "text": {"@type": "formattedText", "text": "live", "entities": []}
                            }
                        },
                        "@client_id": sent.client_id,
                    })
                    .to_string(),
                );
            }
            events.push(
                json!({
                    "@type": "updateDeleteMessages",
                    "chat_id": 100,
                    "message_ids": [1],
                    "is_permanent": true,
                    "from_cache": false,
                    "@client_id": sent.client_id,
                })
                .to_string(),
            );
            for message_id in 1..=(MAX_TRACKED_REFRESHES_PER_CHAT as i64 + 50) {
                events.push(
                    json!({
                        "@type": "updateMessageContent",
                        "chat_id": 100,
                        "message_id": message_id,
                        "@client_id": sent.client_id,
                    })
                    .to_string(),
                );
            }
            events.push(
                json!({
                    "@type": "ok",
                    "@extra": sent.extra().expect("extra"),
                    "@client_id": sent.client_id,
                })
                .to_string(),
            );
            events
        });
        let runtime = TdRuntime::start(
            sender,
            receiver,
            RuntimeConfig {
                receive_timeout: Duration::from_millis(5),
                update_queue_capacity: 2,
            },
        )
        .expect("runtime");
        let (client, updates) = runtime.create_client().expect("client");
        let mut snapshot =
            SnapshotMachine::new(SnapshotPlan::new(vec![ChatListKind::Main])).expect("snapshot");
        let SnapshotStep::Submit(request) = snapshot.next_step().expect("step") else {
            panic!("submit")
        };
        let pending = client.request(request).expect("request");
        let mut folders = FolderCatalogMachine::new();
        let mut metadata = UpdateMachine::new();
        let cancelled = AtomicBool::new(false);

        wait_for_snapshot_response(
            &mut state,
            scope(),
            pending,
            &mut snapshot,
            &mut folders,
            &mut metadata,
            Some(&mut content.live),
            Some(&mut content.stories),
            &updates,
            &cancelled,
        )
        .expect("wait")
        .expect("response");

        let usage = content.live.buffer_usage();
        assert_eq!(usage.tracked_ready_changes, 0);
        assert_eq!(usage.tracked_refreshes, MAX_TRACKED_REFRESHES_PER_CHAT);
        assert!(matches!(
            content.live.next_step().expect("recovery"),
            LiveStep::RecoveryRequired { chat_id: 100 }
        ));
        require_full_live_recovery(&mut state, scope(), 100).expect("durable recovery marker");
        drop(content); // Crash boundary: only durable rows survive.

        let key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let read = state.read_txn().expect("read");
        let sync = read.chat_sync_state(&key).expect("sync").expect("row");
        assert_eq!(
            sync.window,
            Some(SyncWindow {
                oldest: MessageId(1),
                newest: MessageId(1),
            })
        );
        assert!(
            sync.history_complete,
            "the recovery obligation must not erase the monotonic cursor"
        );
        let progress = read
            .chat_content_progress(&key)
            .expect("progress")
            .expect("row");
        assert_eq!(progress.phase, ChatContentPhase::Degraded);
        assert_eq!(
            progress.failure_category.as_deref(),
            Some("live-refresh-overflow")
        );
        let events = read.events_after(&key, 0, 1_000).expect("events");
        assert!(events.iter().any(|event| {
            event.message_id == MessageId(1) && event.kind == MessageEventKind::Deleted
        }));
        drop(read);

        let relaunched = ContentCoordinator::new(&mut state, scope()).expect("relaunch");
        assert_eq!(relaunched.live.buffer_usage().tracked_refreshes, 0);
        let mut crawl = crawl_for_chat(
            &mut state,
            scope(),
            ChatId(100),
            BackfillPriority::Background,
        )
        .expect("recovery crawl");
        let CrawlStep::Submit(request) = crawl.next_step().expect("recovery step") else {
            panic!("full recovery must submit history")
        };
        assert_eq!(request["from_message_id"], 0);
        runtime.shutdown();
    }

    #[test]
    fn edit_during_pending_response_is_crash_durable_before_live_steps_run() {
        use gramdrive_source_tdjson::mock::MockTdJson;
        use gramdrive_source_tdjson::{RuntimeConfig, TdRuntime};

        let mut state = store();
        add_chat(&mut state, 100, false);
        apply_history_commit(
            &mut state,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![message(100, 1, "original")],
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 1,
                }),
                history_complete: true,
                skipped_malformed: 0,
            },
            1_000,
        )
        .expect("seed complete history");
        let mut content = ContentCoordinator::new(&mut state, scope()).expect("coordinator");

        let (sender, receiver, handle) = MockTdJson::new();
        handle.set_responder(move |sent| {
            vec![
                json!({
                    "@type": "updateMessageContent",
                    "chat_id": 100,
                    "message_id": 1,
                    "@client_id": sent.client_id,
                })
                .to_string(),
                // Deliberately no response carrying sent.extra(): the source
                // request remains pending at the simulated crash boundary.
            ]
        });
        let runtime = TdRuntime::start(
            sender,
            receiver,
            RuntimeConfig {
                receive_timeout: Duration::from_millis(5),
                update_queue_capacity: 2,
            },
        )
        .expect("runtime");
        let (client, updates) = runtime.create_client().expect("client");
        let pending = client
            .request(json!({"@type": "getChats", "limit": 1}))
            .expect("pending request");
        let update = updates
            .recv_timeout(Duration::from_secs(1))
            .expect("edit update while response is pending");

        // This is the production intake path. The test never calls either
        // recovery-marker helper and never consumes LiveMachine::next_step.
        route_content_live_update(&mut state, scope(), &mut content.live, &update)
            .expect("route edit");
        route_content_live_update(
            &mut state,
            scope(),
            &mut content.live,
            &json!({
                "@type": "updateNewMessage",
                "message": {
                    "@type": "message",
                    "id": 2,
                    "chat_id": 100,
                    "date": 1_700_000_002,
                    "sender_id": {"@type": "messageSenderUser", "user_id": 77},
                    "can_be_saved": true,
                    "content": {
                        "@type": "messageText",
                        "text": {"@type": "formattedText", "text": "new", "entities": []}
                    }
                }
            }),
        )
        .expect("unrelated live commit cannot clear recovery");
        apply_history_commit(
            &mut state,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![message(100, 1, "original")],
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 1,
                }),
                history_complete: true,
                skipped_malformed: 0,
            },
            1_500,
        )
        .expect("already in-flight history page cannot clear recovery");
        assert!(
            pending.wait_timeout(Duration::from_millis(10)).is_err(),
            "the unrelated TDLib response must still be pending"
        );
        drop(content); // Crash: all in-memory targeted refresh state is lost.
        runtime.shutdown();

        let key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let read = state.read_txn().expect("read durable crash state");
        let sync = read.chat_sync_state(&key).expect("sync").expect("row");
        assert_eq!(
            sync.window,
            Some(SyncWindow {
                oldest: MessageId(1),
                newest: MessageId(1),
            })
        );
        assert!(
            sync.history_complete,
            "durable recovery intent must not regress the last checkpoint"
        );
        let progress = read
            .chat_content_progress(&key)
            .expect("progress")
            .expect("row");
        assert_eq!(progress.phase, ChatContentPhase::Degraded);
        assert_eq!(
            progress.failure_category.as_deref(),
            Some("live-edit-pending")
        );
        assert!(progress.retryable);
        drop(read);

        let _relaunched = ContentCoordinator::new(&mut state, scope()).expect("relaunch from DB");
        let mut crawl = crawl_for_chat(
            &mut state,
            scope(),
            ChatId(100),
            BackfillPriority::Background,
        )
        .expect("recovery crawl");
        let CrawlStep::Submit(request) = crawl.next_step().expect("recovery step") else {
            panic!("durable edit recovery must start history")
        };
        assert_eq!(request["from_message_id"], 0);
        assert_eq!(request["@type"], "getChatHistory");
        put_content_progress(
            &mut state,
            key,
            content_progress(ChatContentPhase::Syncing, None, false, 0, None),
        )
        .expect("production scheduler claims the zero-anchored crawl generation");
        crawl
            .on_response(Ok(json!({
                "@type": "messages",
                "total_count": 2,
                "messages": [
                    {
                        "@type": "message",
                        "id": 2,
                        "chat_id": 100,
                        "date": 1_700_000_002,
                        "sender_id": {"@type": "messageSenderUser", "user_id": 77},
                        "can_be_saved": true,
                        "content": {
                            "@type": "messageText",
                            "text": {"@type": "formattedText", "text": "new", "entities": []}
                        }
                    },
                    {
                        "@type": "message",
                        "id": 1,
                        "chat_id": 100,
                        "date": 1_700_000_001,
                        "edit_date": 1_700_000_100,
                        "sender_id": {"@type": "messageSenderUser", "user_id": 77},
                        "can_be_saved": true,
                        "content": {
                            "@type": "messageText",
                            "text": {"@type": "formattedText", "text": "edited", "entities": []}
                        }
                    }
                ]
            })))
            .expect("edited history page");
        let CrawlStep::Commit(commit) = crawl.next_step().expect("recovery commit") else {
            panic!("edited page must commit")
        };
        apply_history_commit(&mut state, scope(), &commit, 2_000).expect("apply recovery");
        apply_history_commit(&mut state, scope(), &commit, 3_000).expect("replay recovery");
        let CrawlStep::Submit(request) = crawl.next_step().expect("history boundary request")
        else {
            panic!("recovery must confirm the beginning of history")
        };
        assert_eq!(request["from_message_id"], 1);
        crawl
            .on_response(Ok(json!({
                "@type": "messages",
                "total_count": 1,
                "messages": []
            })))
            .expect("empty history boundary");
        let CrawlStep::Commit(commit) = crawl.next_step().expect("boundary commit") else {
            panic!("empty boundary must commit completion")
        };
        apply_history_commit(&mut state, scope(), &commit, 4_000).expect("apply completion");
        assert!(matches!(crawl.next_step().expect("done"), CrawlStep::Done));

        let read = state.read_txn().expect("read recovered state");
        let events = read.events_after(&key, 0, 10).expect("events");
        assert_eq!(
            events.len(),
            3,
            "seed, new message, and one edit without replay effects"
        );
        assert!(
            read.chat_sync_state(&key)
                .expect("sync")
                .expect("row")
                .history_complete
        );
    }

    #[test]
    fn replayed_edit_signal_does_not_reset_a_newer_durable_cursor() {
        let mut state = store();
        add_chat(&mut state, 100, false);
        let edited = normalize_message(&json!({
            "@type": "message",
            "id": 1,
            "chat_id": 100,
            "date": 1_700_000_001,
            "edit_date": 1_700_000_100,
            "sender_id": {"@type": "messageSenderUser", "user_id": 77},
            "can_be_saved": true,
            "content": {
                "@type": "messageText",
                "text": {"@type": "formattedText", "text": "edited", "entities": []}
            }
        }))
        .expect("edited message");
        apply_history_commit(
            &mut state,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![edited],
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 1,
                }),
                history_complete: true,
                skipped_malformed: 0,
            },
            2_000,
        )
        .expect("seed edited history");
        let mut content = ContentCoordinator::new(&mut state, scope()).expect("coordinator");

        route_content_live_update(
            &mut state,
            scope(),
            &mut content.live,
            &json!({
                "@type": "updateMessageEdited",
                "chat_id": 100,
                "message_id": 1,
                "edit_date": 1_700_000_100
            }),
        )
        .expect("replayed edit");

        let sync = state
            .read_txn()
            .expect("read")
            .chat_sync_state(&ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            })
            .expect("sync")
            .expect("row");
        assert_eq!(
            sync.window,
            Some(SyncWindow {
                oldest: MessageId(1),
                newest: MessageId(1),
            })
        );
        assert!(sync.history_complete);
    }

    #[test]
    fn empty_lists_are_ready_and_checkpoint_resume_is_idempotent() {
        let mut store = store();
        let token = br#"{"version":1,"completed":["main"]}"#.to_vec();
        let txn = store.write_txn().expect("write");
        txn.put_namespace_bootstrap(&NamespaceBootstrapRecord {
            scope: scope(),
            resume_token: token.clone(),
            updated_at_ms: 1,
        })
        .expect("checkpoint");
        txn.commit().expect("commit");
        let read = store.read_txn().expect("read");
        assert_eq!(
            read.namespace_bootstrap(scope())
                .expect("read")
                .expect("present")
                .resume_token,
            token
        );
        assert!(
            read.chat_list(&ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            })
            .expect("empty")
            .is_empty()
        );
    }

    #[test]
    fn composed_history_commit_is_atomic_idempotent_and_resumable() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        initialize_content_progress(&mut store, scope()).expect("initialize progress");
        let commit = HistoryCommit {
            chat_id: 100,
            records: vec![message(100, 3, "three"), message(100, 2, "two")],
            window: Some(CrawlWindow {
                oldest_message_id: 2,
                newest_message_id: 3,
            }),
            history_complete: false,
            skipped_malformed: 0,
        };

        apply_history_commit(&mut store, scope(), &commit, 2_000).expect("first commit");
        // Crash/replay boundary: the page can be submitted again because a
        // response raced a crash. Identity replay appends no duplicate effect.
        apply_history_commit(&mut store, scope(), &commit, 3_000).expect("replay commit");

        let key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let read = store.read_txn().expect("read");
        let events = read.events_after(&key, 0, 100).expect("events");
        assert_eq!(events.len(), 2);
        assert!(
            events
                .iter()
                .all(|event| event.kind == MessageEventKind::Observed)
        );
        let payload = events[0].payload.as_ref().expect("normalized payload");
        assert_eq!(payload.schema, NORMALIZED_MESSAGE_SCHEMA_FAMILY);
        let decoded: MessageRecord = serde_json::from_slice(&payload.bytes).expect("decode JSON");
        assert_eq!(decoded.chat_id, 100);
        assert_eq!(
            read.chat_sync_state(&key)
                .expect("sync")
                .expect("row")
                .window,
            Some(SyncWindow {
                oldest: MessageId(2),
                newest: MessageId(3),
            })
        );
        assert_eq!(
            read.chat_content_progress(&key)
                .expect("progress")
                .expect("row")
                .phase,
            ChatContentPhase::Syncing
        );
        drop(read);

        let mut resumed = crawl_for_chat(
            &mut store,
            scope(),
            ChatId(100),
            BackfillPriority::Background,
        )
        .expect("resume machine");
        let CrawlStep::Submit(request) = resumed.next_step().expect("next") else {
            panic!("resumed crawl must request a page")
        };
        assert_eq!(request["@type"], "getChatHistory");
        assert!(!request.to_string().contains("downloadFile"));
    }

    #[test]
    fn inclusive_multi_page_history_restarts_recovers_and_publishes_both_months() {
        const JUNE_20_SECS: i64 = 1_782_345_600;
        const JULY_20_SECS: i64 = 1_784_937_600;

        let mut store = store();
        let txn = store.write_txn().expect("write metadata");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");
        rebuild_projection(&mut store, scope()).expect("initial projection");

        let cache_root = std::env::temp_dir().join(format!(
            "gramdrive-history-cross-month-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&cache_root);

        let mut crawl = CrawlMachine::new(CrawlPlan {
            chats: vec![ChatCrawl::new(100)],
            page_size: 2,
        })
        .expect("fresh crawl");
        let CrawlStep::Submit(anchor) = crawl.next_step().expect("anchor request") else {
            panic!("fresh crawl must anchor")
        };
        assert_eq!(anchor["from_message_id"], 0);
        crawl
            .on_response(Ok(json!({
                "@type": "messages",
                "messages": [
                    td_history_message_at(100, 4, JULY_20_SECS),
                    td_history_message_at(100, 3, JULY_20_SECS),
                ],
            })))
            .expect("anchor response");
        let CrawlStep::Commit(anchor_commit) = crawl.next_step().expect("anchor commit") else {
            panic!("anchor must commit")
        };
        apply_history_commit(&mut store, scope(), &anchor_commit, 1_000).expect("persist anchor");
        render_pending_months(&mut store, &cache_root, 1_100).expect("publish newest month");
        let chat_key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        assert_eq!(render_facts(&mut store, chat_key, 2026, 7).len(), 2);
        assert!(render_facts(&mut store, chat_key, 2026, 6).is_empty());

        // Crash/relaunch after the first page: only the durable window is
        // carried into a fresh crawl machine.
        drop(crawl);
        let mut resumed = crawl_for_chat(
            &mut store,
            scope(),
            ChatId(100),
            BackfillPriority::Background,
        )
        .expect("resume from durable cursor");
        let CrawlStep::Submit(catch_up) = resumed.next_step().expect("catch-up request") else {
            panic!("resume must reconnect to the present")
        };
        assert_eq!(catch_up["from_message_id"], 0);
        resumed
            .on_response(Ok(json!({
                "@type": "messages",
                "messages": [
                    td_history_message_at(100, 4, JULY_20_SECS),
                    td_history_message_at(100, 3, JULY_20_SECS),
                ],
            })))
            .expect("inclusive catch-up response");
        let CrawlStep::Commit(replay) = resumed.next_step().expect("catch-up commit") else {
            panic!("catch-up must commit its idempotent overlap")
        };
        apply_history_commit_with_publication(&mut store, scope(), &replay, 1_200, false)
            .expect("persist overlap without rebuilding the provider tree");

        let CrawlStep::Submit(older_page) = resumed.next_step().expect("older page request") else {
            panic!("resume must continue backward")
        };
        assert_eq!(older_page["from_message_id"], 3);
        resumed
            .on_response(Ok(json!({
                "@type": "messages",
                "messages": [
                    td_history_message_at(100, 3, JULY_20_SECS),
                    td_history_message_at(100, 2, JUNE_20_SECS),
                ],
            })))
            .expect("inclusive older page");
        let CrawlStep::Commit(june_commit) = resumed.next_step().expect("june commit") else {
            panic!("cross-month page must commit")
        };
        apply_history_commit_with_publication(&mut store, scope(), &june_commit, 1_300, false)
            .expect("persist older month inside the bounded history slice");
        render_pending_months(&mut store, &cache_root, 1_400).expect("publish older month");
        assert!(
            render_facts(&mut store, chat_key, 2026, 6).is_empty(),
            "durable page commits do not rebuild the whole provider tree mid-slice"
        );
        let mut coordinator =
            ContentCoordinator::new(&mut store, scope()).expect("publication coordinator");
        coordinator.active_chat = Some(100);
        coordinator.history_projection_pending = true;
        assert!(
            publish_pending_history_projection(&mut store, scope(), &mut coordinator)
                .expect("publish at slice boundary")
        );
        assert!(
            !publish_pending_history_projection(&mut store, scope(), &mut coordinator)
                .expect("idempotent publication boundary"),
            "one slice emits one provider-visible publication signal"
        );
        render_pending_months(&mut store, &cache_root, 1_450)
            .expect("render after slice publication");
        assert_eq!(render_facts(&mut store, chat_key, 2026, 6).len(), 2);
        assert_eq!(render_facts(&mut store, chat_key, 2026, 7).len(), 2);

        // A transient source failure keeps the identical request and cursor
        // obligation; success resumes without duplicate identities.
        let CrawlStep::Submit(before_retry) = resumed.next_step().expect("retryable request")
        else {
            panic!("crawl must continue after publishing the older month")
        };
        resumed
            .on_response(Err(gramdrive_source_tdjson::TdError::Td {
                code: 500,
                message: "transport unavailable".to_owned(),
            }))
            .expect("transient failure arms backoff");
        assert!(matches!(
            resumed.next_step().expect("backoff"),
            CrawlStep::Backoff(_)
        ));
        let CrawlStep::Submit(after_retry) = resumed.next_step().expect("retry request") else {
            panic!("transient failure must reissue")
        };
        assert_eq!(after_retry, before_retry);
        resumed
            .on_response(Ok(json!({
                "@type": "messages",
                "messages": [
                    td_history_message_at(100, 2, JUNE_20_SECS),
                    td_history_message_at(100, 1, JUNE_20_SECS),
                ],
            })))
            .expect("retry succeeds");
        let CrawlStep::Commit(last_data) = resumed.next_step().expect("last data commit") else {
            panic!("retry success must commit")
        };
        apply_history_commit(&mut store, scope(), &last_data, 1_500).expect("persist retry page");

        let CrawlStep::Submit(boundary) = resumed.next_step().expect("boundary request") else {
            panic!("crawl must prove the beginning")
        };
        assert_eq!(boundary["from_message_id"], 1);
        resumed
            .on_response(Ok(json!({
                "@type": "messages",
                "messages": [td_history_message_at(100, 1, JUNE_20_SECS)],
            })))
            .expect("inclusive terminal boundary");
        let CrawlStep::Commit(terminal) = resumed.next_step().expect("terminal commit") else {
            panic!("boundary-only page must commit completion")
        };
        assert!(terminal.history_complete);
        apply_history_commit(&mut store, scope(), &terminal, 1_600).expect("persist completion");
        assert!(matches!(
            resumed.next_step().expect("done"),
            CrawlStep::Done
        ));

        let read = store.read_txn().expect("read result");
        assert_eq!(
            read.events_after(&chat_key, 0, 100).expect("events").len(),
            4,
            "inclusive overlap and retry replay append no duplicate identities"
        );
        let sync = read
            .chat_sync_state(&chat_key)
            .expect("sync")
            .expect("sync row");
        assert_eq!(
            sync.window,
            Some(SyncWindow {
                oldest: MessageId(1),
                newest: MessageId(4),
            })
        );
        assert!(sync.history_complete);
        drop(read);
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn render_publication_is_bounded_per_tick_and_idle_ticks_do_not_signal() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        rebuild_projection(&mut store, scope()).expect("initial projection");
        let timezone = DisplayTimeZone::named("UTC").expect("UTC");
        let records = (0..18)
            .map(|index| {
                let year = 2025 + index / 12;
                let month = 1 + index % 12;
                let (start_ms, _) = timezone
                    .month_bounds_ms(year as u16, month as u8)
                    .expect("month bounds");
                message_at(
                    100,
                    i64::from(index + 1),
                    "bounded",
                    start_ms / 1_000 + 3_600,
                )
            })
            .collect::<Vec<_>>();
        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records,
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 18,
                }),
                history_complete: false,
                skipped_malformed: 0,
            },
            2_000,
        )
        .expect("history");
        let pending_jobs = |store: &mut StateStore| {
            let read = store.read_txn().expect("read worklist");
            plan_worklist(&read, u32::MAX).expect("worklist").jobs.len()
        };
        let before = pending_jobs(&mut store);
        assert!(before > MAX_RENDER_WORKLIST_ITEMS_PER_TICK as usize);

        let cache_root = std::env::temp_dir().join(format!(
            "gramdrive-bounded-render-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&cache_root);
        assert!(render_pending_months(&mut store, &cache_root, 2_100).expect("bounded render"));
        let after_one = pending_jobs(&mut store);
        assert!(
            after_one > 0 && after_one < before,
            "one tick must make progress without draining an unbounded worklist"
        );
        let mut remaining = after_one;
        for tick in 0..before {
            if remaining == 0 {
                break;
            }
            assert!(
                render_pending_months(
                    &mut store,
                    &cache_root,
                    2_200 + i64::try_from(tick).expect("tick fits i64"),
                )
                .expect("drain bounded render")
            );
            let next = pending_jobs(&mut store);
            assert!(
                next < remaining,
                "every bounded publication tick must make durable progress"
            );
            remaining = next;
        }
        assert_eq!(pending_jobs(&mut store), 0);
        assert!(
            !render_pending_months(&mut store, &cache_root, 2_300).expect("idle render"),
            "an idle publication tick must not produce provider signaling"
        );
        let _ = std::fs::remove_dir_all(cache_root);
    }

    #[test]
    fn leased_generated_document_defers_its_version_without_blocking_other_render_work() {
        let mut store = store();
        let target = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let background = ChatKey {
            scope: scope(),
            chat_id: ChatId(200),
        };
        add_main_chat(&mut store, target.chat_id.0, false);
        add_main_chat(&mut store, background.chat_id.0, false);
        rebuild_projection(&mut store, scope()).expect("initial projection");
        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: background.chat_id.0,
                records: vec![message_at(
                    background.chat_id.0,
                    1,
                    "backfill-1",
                    1_700_000_000,
                )],
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 1,
                }),
                history_complete: false,
                skipped_malformed: 0,
            },
            1_500,
        )
        .expect("initial active-backfill page");

        let cache_root = std::env::temp_dir().join(format!(
            "gramdrive-leased-render-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&cache_root);
        assert!(render_pending_months(&mut store, &cache_root, 2_000).expect("initial render"));
        for tick in 1..=4 {
            if !render_pending_months(&mut store, &cache_root, 2_000 + tick)
                .expect("drain initial bounded worklist")
            {
                break;
            }
        }
        assert!(
            store
                .read_txn()
                .expect("read drained worklist")
                .dirty_render_items(u32::MAX)
                .expect("drained worklist")
                .is_empty(),
            "the contention fixture starts from fully published documents"
        );

        let (target_item, target_path, target_version, background_item, background_version) = {
            let read = store.read_txn().expect("read initial publications");
            let target_item = read
                .chat_render_catalog(target)
                .expect("target catalog")
                .into_iter()
                .next()
                .expect("target generated document")
                .item;
            let background_item = read
                .month_render_catalog(background, 2023, 11)
                .expect("background monthly catalog")
                .into_iter()
                .find(|entry| entry.format == gramdrive_model::identity::DocFormat::Markdown)
                .expect("background Markdown document")
                .item;
            let target_cache = read
                .cache_entry(&target_item)
                .expect("target cache read")
                .expect("target cache row");
            let target_path = target_cache
                .materialization_ref
                .expect("target materialization path");
            (
                target_item.clone(),
                target_path,
                target_cache.content_version,
                background_item.clone(),
                read.cache_entry(&background_item)
                    .expect("background cache read")
                    .expect("background cache row")
                    .content_version,
            )
        };
        let target_bytes = std::fs::read(&target_path).expect("target generated bytes");
        let lease = GeneratedFileLease::acquire(Path::new(&target_path)).expect("hydration lease");

        let txn = store.write_txn().expect("dirty target metadata");
        let mut record = txn
            .read()
            .chat(&target)
            .expect("target chat read")
            .expect("target chat row");
        record.title = "target-v2".to_owned();
        record.metadata_version = MetadataVersion::new("target-v2").expect("metadata version");
        txn.upsert_chat(&record).expect("target chat update");
        txn.mark_render_dirty(&target_item)
            .expect("dirty target document");
        txn.commit().expect("commit concurrent work");
        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: background.chat_id.0,
                records: vec![message_at(
                    background.chat_id.0,
                    2,
                    "backfill-2",
                    1_700_000_001,
                )],
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 2,
                }),
                history_complete: false,
                skipped_malformed: 0,
            },
            2_500,
        )
        .expect("concurrent active-backfill page");

        assert!(
            render_pending_months(&mut store, &cache_root, 3_000)
                .expect("render around leased target"),
            "the unleased background document must still publish in the same bounded tick"
        );
        {
            let read = store.read_txn().expect("read deferred target");
            let target_cache = read
                .cache_entry(&target_item)
                .expect("target cache read")
                .expect("target cache row");
            assert_eq!(target_cache.content_version, target_version);
            assert_eq!(
                target_cache.materialization_ref.as_deref(),
                Some(target_path.as_str())
            );
            assert!(
                read.render_state(&target_item)
                    .expect("target render state")
                    .expect("target render row")
                    .dirty,
                "the deferred target remains durably eligible for the next tick"
            );
            let background_cache = read
                .cache_entry(&background_item)
                .expect("background cache read")
                .expect("background cache row");
            assert_ne!(background_cache.content_version, background_version);
            assert!(
                !read
                    .render_state(&background_item)
                    .expect("background render state")
                    .expect("background render row")
                    .dirty,
                "historical backfill continues while the foreground clone is active"
            );
        }
        assert_eq!(
            std::fs::read(&target_path).expect("leased target bytes"),
            target_bytes,
            "the File Provider clone source remains exact and version-stable"
        );

        drop(lease);
        assert!(render_pending_months(&mut store, &cache_root, 4_000).expect("retry target"));
        let read = store.read_txn().expect("read retried target");
        let target_cache = read
            .cache_entry(&target_item)
            .expect("target cache read")
            .expect("target cache row");
        assert_ne!(target_cache.content_version, target_version);
        assert!(
            !read
                .render_state(&target_item)
                .expect("target render state")
                .expect("target render row")
                .dirty
        );
        drop(read);
        let _ = std::fs::remove_dir_all(cache_root);
    }

    #[test]
    fn leased_month_member_keeps_pair_atomic_while_other_backfill_publishes_then_retries() {
        let mut store = store();
        let target = ChatKey {
            scope: scope(),
            chat_id: ChatId(300),
        };
        let background = ChatKey {
            scope: scope(),
            chat_id: ChatId(400),
        };
        for chat in [target, background] {
            add_main_chat(&mut store, chat.chat_id.0, false);
            apply_history_commit(
                &mut store,
                scope(),
                &HistoryCommit {
                    chat_id: chat.chat_id.0,
                    records: vec![message_at(chat.chat_id.0, 1, "initial", 1_700_000_000)],
                    window: Some(CrawlWindow {
                        oldest_message_id: 1,
                        newest_message_id: 1,
                    }),
                    history_complete: false,
                    skipped_malformed: 0,
                },
                1_500,
            )
            .expect("initial history page");
        }
        rebuild_projection(&mut store, scope()).expect("initial projection");
        let cache_root = std::env::temp_dir().join(format!(
            "gramdrive-leased-month-pair-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&cache_root);
        assert!(render_pending_months(&mut store, &cache_root, 2_000).expect("initial render"));
        for tick in 1..=4 {
            if !render_pending_months(&mut store, &cache_root, 2_000 + tick)
                .expect("drain initial monthly worklist")
            {
                break;
            }
        }
        assert!(
            store
                .read_txn()
                .expect("read drained monthly worklist")
                .dirty_render_items(u32::MAX)
                .expect("drained monthly worklist")
                .is_empty(),
            "the monthly contention fixture starts fully published"
        );

        let target_catalog = store
            .read_txn()
            .expect("read target catalog")
            .month_render_catalog(target, 2023, 11)
            .expect("target monthly catalog");
        assert_eq!(
            target_catalog.len(),
            2,
            "fixture needs one complete format pair"
        );
        let target_before = {
            let read = store.read_txn().expect("read target pair");
            target_catalog
                .iter()
                .map(|entry| {
                    let cache = read
                        .cache_entry(&entry.item)
                        .expect("target cache query")
                        .expect("target cache row");
                    let path = cache
                        .materialization_ref
                        .expect("target materialization reference");
                    (
                        entry.format,
                        entry.item.clone(),
                        cache.content_version,
                        path.clone(),
                        std::fs::read(path).expect("target exact bytes"),
                    )
                })
                .collect::<Vec<_>>()
        };
        let markdown_path = target_before
            .iter()
            .find(|(format, ..)| *format == gramdrive_model::identity::DocFormat::Markdown)
            .map(|(_, _, _, path, _)| path)
            .expect("target Markdown path");
        let lease = GeneratedFileLease::acquire(Path::new(markdown_path))
            .expect("lease one monthly pair member");

        for chat in [target, background] {
            apply_history_commit(
                &mut store,
                scope(),
                &HistoryCommit {
                    chat_id: chat.chat_id.0,
                    records: vec![message_at(chat.chat_id.0, 2, "next", 1_700_000_001)],
                    window: Some(CrawlWindow {
                        oldest_message_id: 1,
                        newest_message_id: 2,
                    }),
                    history_complete: false,
                    skipped_malformed: 0,
                },
                2_500,
            )
            .expect("concurrent backfill page");
        }
        let background_before = {
            let read = store.read_txn().expect("read background before");
            read.month_render_catalog(background, 2023, 11)
                .expect("background catalog")
                .into_iter()
                .map(|entry| {
                    (
                        entry.item.clone(),
                        read.cache_entry(&entry.item)
                            .expect("background cache query")
                            .expect("background cache row")
                            .content_version,
                    )
                })
                .collect::<Vec<_>>()
        };

        assert!(
            render_pending_months(&mut store, &cache_root, 3_000)
                .expect("render with leased monthly pair"),
            "unrelated historical work must publish in the same bounded tick"
        );
        {
            let read = store.read_txn().expect("read deferred pair");
            for (_, item, version, path, bytes) in &target_before {
                let cache = read
                    .cache_entry(item)
                    .expect("target cache query")
                    .expect("target cache row");
                assert_eq!(&cache.content_version, version);
                assert_eq!(cache.materialization_ref.as_deref(), Some(path.as_str()));
                assert_eq!(std::fs::read(path).expect("leased pair bytes"), *bytes);
                assert!(
                    read.render_state(item)
                        .expect("target render query")
                        .expect("target render row")
                        .dirty,
                    "both monthly members remain durably retryable"
                );
            }
            for (item, version) in &background_before {
                let cache = read
                    .cache_entry(item)
                    .expect("background cache query")
                    .expect("background cache row");
                assert_ne!(&cache.content_version, version);
                assert!(
                    !read
                        .render_state(item)
                        .expect("background render query")
                        .expect("background render row")
                        .dirty,
                    "unrelated backfill publication makes durable progress"
                );
            }
        }

        drop(lease);
        assert!(render_pending_months(&mut store, &cache_root, 4_000).expect("retry pair"));
        let read = store.read_txn().expect("read published replacement pair");
        let replacement_refs = target_before
            .iter()
            .map(|(_, item, version, old_path, _)| {
                let cache = read
                    .cache_entry(item)
                    .expect("replacement cache query")
                    .expect("replacement cache row");
                assert_ne!(&cache.content_version, version);
                assert!(
                    !read
                        .render_state(item)
                        .expect("replacement render query")
                        .expect("replacement render row")
                        .dirty
                );
                assert!(
                    !Path::new(old_path).exists(),
                    "old pair generation is reclaimed"
                );
                cache
                    .materialization_ref
                    .expect("replacement materialization reference")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            Path::new(&replacement_refs[0]).parent(),
            Path::new(&replacement_refs[1]).parent(),
            "Markdown and NDJSON publish from one immutable generation"
        );
        drop(read);
        let _ = std::fs::remove_dir_all(cache_root);
    }

    #[tokio::test]
    async fn wal_blocked_publication_does_not_hold_lease_mutex_from_foreground_cache_hit() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let data_root = std::env::temp_dir().join(format!(
            "gramdrive-publication-lock-order-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&data_root).expect("data root");
        let root_text = data_root.to_string_lossy().into_owned();
        let layout = shared_state_layout(root_text.clone()).expect("shared layout");
        std::fs::create_dir_all(&layout.state_dir).expect("state directory");
        let mut store = initialized_store(
            StateStore::open(&layout.database_file).expect("open durable contention state"),
        );
        let foreground = ChatKey {
            scope: scope(),
            chat_id: ChatId(500),
        };
        let background = ChatKey {
            scope: scope(),
            chat_id: ChatId(600),
        };
        add_main_chat(&mut store, foreground.chat_id.0, false);
        add_main_chat(&mut store, background.chat_id.0, false);
        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: background.chat_id.0,
                records: vec![message_at(
                    background.chat_id.0,
                    1,
                    "foreground-month",
                    1_700_000_000,
                )],
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 1,
                }),
                history_complete: false,
                skipped_malformed: 0,
            },
            1_500,
        )
        .expect("foreground monthly cache fixture");
        rebuild_projection(&mut store, scope()).expect("initial projection");
        assert!(
            render_pending_months(&mut store, Path::new(&layout.cache_dir), 2_000)
                .expect("initial publication")
        );
        while render_pending_months(&mut store, Path::new(&layout.cache_dir), 2_001)
            .expect("drain initial worklist")
        {}

        let foreground_item = store
            .read_txn()
            .expect("read foreground catalog")
            .month_render_catalog(background, 2023, 11)
            .expect("foreground catalog")
            .into_iter()
            .find(|entry| entry.format == gramdrive_model::identity::DocFormat::Markdown)
            .expect("foreground generated document")
            .item;
        let foreground_cache = store
            .read_txn()
            .expect("read foreground cache")
            .cache_entry(&foreground_item)
            .expect("foreground cache query")
            .expect("foreground cache row");
        let foreground_version = foreground_cache.content_version;
        let foreground_path = foreground_cache
            .materialization_ref
            .expect("foreground materialization reference");
        let foreground_bytes = std::fs::read(&foreground_path).expect("foreground exact bytes");

        let background_item = store
            .read_txn()
            .expect("read background catalog")
            .chat_render_catalog(background)
            .expect("background catalog")
            .into_iter()
            .next()
            .expect("background generated document")
            .item;
        let publication_base = store
            .read_txn()
            .expect("read background cache")
            .cache_entry(&background_item)
            .expect("background cache query")
            .expect("background cache row")
            .materialization_ref
            .and_then(|path| {
                Path::new(&path)
                    .parent()
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
            })
            .expect("background publication base");
        let generation_count_before = std::fs::read_dir(&publication_base)
            .expect("initial publication base")
            .count();
        let txn = store.write_txn().expect("dirty background publication");
        let mut background_record = txn
            .read()
            .chat(&background)
            .expect("background chat query")
            .expect("background chat row");
        background_record.title = "background-v2".to_owned();
        background_record.metadata_version =
            MetadataVersion::new("background-v2").expect("background metadata version");
        txn.upsert_chat(&background_record)
            .expect("background chat update");
        txn.mark_render_dirty(&background_item)
            .expect("dirty background render");
        txn.commit().expect("commit dirty background");
        drop(store);

        let hydrator = Hydrator::shared(&root_text).expect("hydrator");
        let (writer_ready, writer_ready_wait) = std::sync::mpsc::sync_channel(0);
        let (writer_release, writer_release_wait) = std::sync::mpsc::sync_channel(0);
        let writer_database = layout.database_file.clone();
        let writer = std::thread::spawn(move || {
            let mut store = StateStore::open(&writer_database).expect("WAL writer state");
            let held = store.write_txn().expect("hold WAL writer");
            writer_ready.send(()).expect("announce WAL writer");
            writer_release_wait.recv().expect("release WAL writer");
            drop(held);
        });
        writer_ready_wait
            .recv_timeout(Duration::from_secs(1))
            .expect("WAL writer owns BEGIN IMMEDIATE");

        let (publication_done, publication_done_wait) = std::sync::mpsc::sync_channel(1);
        let publication_database = layout.database_file.clone();
        let publication_cache = layout.cache_dir.clone();
        let publisher = std::thread::spawn(move || {
            let mut store = StateStore::open(&publication_database).expect("publication state");
            let result = render_pending_months(&mut store, Path::new(&publication_cache), 3_000);
            publication_done.send(result).expect("report publication");
        });
        let staged_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::fs::read_dir(&publication_base)
            .expect("publication base during publication")
            .count()
            <= generation_count_before
        {
            assert!(
                std::time::Instant::now() < staged_deadline,
                "background publication stages its replacement before the foreground read"
            );
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            publication_done_wait
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "background publication remains blocked behind the held WAL writer"
        );

        let started = std::time::Instant::now();
        let hydrated = tokio::time::timeout(
            Duration::from_millis(500),
            Arc::clone(&hydrator).hydrate(
                scope().account.account_id.0,
                foreground_item.text().to_owned(),
                Some(foreground_version.as_str().to_owned()),
                Arc::new(NoopHydrationProgress),
                CancellationToken::new(),
            ),
        )
        .await
        .expect("foreground cache hit stays below the production latency bound")
        .expect("foreground generated cache hit");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "foreground lease cannot inherit SQLite's five-second busy timeout"
        );
        assert_eq!(hydrated.path, foreground_path);
        assert_eq!(
            std::fs::read(&hydrated.path).expect("hydrated exact bytes"),
            foreground_bytes
        );

        writer_release.send(()).expect("release WAL writer");
        writer.join().expect("WAL writer thread");
        assert!(
            publication_done_wait
                .recv_timeout(Duration::from_secs(2))
                .expect("publication result after WAL release")
                .expect("retryable publication succeeds"),
            "background publication makes progress after the writer releases"
        );
        publisher.join().expect("publisher thread");
        hydrator
            .release_hydration_lease(hydrated.lease_id.expect("generated lease id"))
            .expect("release foreground lease");
        drop(hydrator);
        let _ = std::fs::remove_dir_all(data_root);
    }

    #[test]
    fn protected_lowest_sorted_render_rows_are_skipped_then_eligible_rows_publish() {
        let mut store = store();
        for chat_id in 100..=105 {
            add_main_chat(&mut store, chat_id, false);
        }
        rebuild_projection(&mut store, scope()).expect("initial projection");

        // Select the exact first bounded quantum from the durable ordering,
        // then make those chats unpublishable. This makes the regression
        // independent of opaque item-id encoding while still seeding the
        // lowest-sorted rows that historically livelocked every render tick.
        let blocked = {
            let read = store.read_txn().expect("read worklist");
            let worklist = read
                .dirty_render_items(MAX_RENDER_WORKLIST_ITEMS_PER_TICK)
                .expect("dirty worklist");
            assert_eq!(
                worklist.len(),
                MAX_RENDER_WORKLIST_ITEMS_PER_TICK as usize,
                "fixture needs a full bounded head"
            );
            worklist
        };
        let txn = store.write_txn().expect("make head protected");
        for item in &blocked {
            let ItemKey::Appearance(AppearanceKey {
                item: CanonicalKey::GeneratedDoc(document),
                ..
            }) = item.key()
            else {
                panic!("render state must belong to a generated document");
            };
            let mut record = txn
                .read()
                .chat(&document.chat)
                .expect("chat read")
                .expect("chat row");
            record.is_protected = true;
            txn.upsert_chat(&record).expect("protect chat");
        }
        txn.commit().expect("commit protected head");

        // Startup/relaunch projection must durably exclude every protected
        // generated document before the bounded renderer chooses a quantum.
        // Before the repair, this left the same lowest-sorted four rows dirty,
        // forcing a policy-only tick before any eligible document could run.
        rebuild_projection(&mut store, scope()).expect("reconcile protected projection");
        // Add a fresh eligible row after the protected lowest tier has been
        // durably excluded. This models a preserved profile that receives an
        // ordinary publishable chat while old protected rows still exist.
        add_main_chat(&mut store, 106, false);
        rebuild_projection(&mut store, scope()).expect("project eligible chat");

        let eligible = {
            let read = store.read_txn().expect("read eligible worklist");
            let eligible = read
                .dirty_render_items(u32::MAX)
                .expect("remaining worklist");
            assert!(
                !eligible.is_empty(),
                "fixture needs an eligible row behind the excluded head"
            );
            eligible
        };

        let cache_root = std::env::temp_dir().join(format!(
            "gramdrive-render-policy-skip-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&cache_root);
        assert!(
            render_pending_months(&mut store, &cache_root, 5_000)
                .expect("publish behind durably skipped head"),
            "policy-excluded rows cannot consume the first bounded render tick"
        );

        {
            let read = store.read_txn().expect("read skipped head");
            for item in &blocked {
                let state = read.render_state(item).expect("render state").expect("row");
                assert!(!state.dirty);
                assert_eq!(
                    state.skip_reason,
                    Some(gramdrive_state::repo::RenderSkipReason::PolicyExcluded)
                );
                assert!(state.skipped_at_ms.is_some());
                assert_eq!(
                    read.item(item)
                        .expect("item read")
                        .expect("item row")
                        .availability,
                    ItemAvailability::Restricted
                );
            }
        }
        let read = store.read_txn().expect("read publication");
        assert_eq!(
            read.dirty_render_items(u32::MAX)
                .expect("worklist after publication")
                .len(),
            0,
            "the first post-reconciliation tick publishes every remaining eligible row"
        );
        for item in eligible {
            let state = read
                .render_state(&item)
                .expect("render state")
                .expect("row");
            assert_eq!(state.rendered_at_ms, Some(5_000));
            assert_eq!(state.skip_reason, None);
        }
        drop(read);
        let _ = std::fs::remove_dir_all(cache_root);
    }

    #[test]
    fn attachment_projection_is_faithful_and_collision_names_ignore_discovery_order() {
        const SENT_AT_SECS: i64 = 1_700_000_000;
        let first = image_document_at(100, 10, SENT_AT_SECS, 510);
        let second = image_document_at(100, 20, SENT_AT_SECS, 520);

        let forward = colliding_attachment_names(vec![first.clone(), second.clone()]);
        let reverse = colliding_attachment_names(vec![second, first]);

        assert_eq!(forward, reverse);
        assert_ne!(forward.get(&10), forward.get(&20));
    }

    #[test]
    fn incremental_attachment_collision_reversions_existing_name_and_is_relaunch_stable() {
        const SENT_AT_SECS: i64 = 1_700_000_000;
        let mut store = store();
        let txn = store.write_txn().expect("write metadata");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");

        let first = image_document_at(100, 10, SENT_AT_SECS, 510);
        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![first],
                window: Some(CrawlWindow {
                    oldest_message_id: 10,
                    newest_message_id: 10,
                }),
                history_complete: false,
                skipped_malformed: 0,
            },
            1_000,
        )
        .expect("initial attachment");

        let first_key = AttachmentKey {
            message: MessageKey {
                chat: ChatKey {
                    scope: scope(),
                    chat_id: ChatId(100),
                },
                message_id: MessageId(10),
            },
            index: AttachmentIndex(0),
        };
        let first_canonical = ItemKey::Canonical(CanonicalKey::Attachment(first_key)).id();
        let initial = store
            .read_txn()
            .expect("read initial projection")
            .appearances_of(&first_canonical)
            .expect("initial appearance")
            .remove(0);

        let second = image_document_at(100, 20, SENT_AT_SECS, 520);
        apply_live_commit(
            &mut store,
            scope(),
            &LiveCommit {
                chat_id: 100,
                changes: vec![LiveChange::Observed(Box::new(second))],
                advance_newest: None,
                skipped_malformed: 0,
                refreshes_rejected: 0,
            },
            2_000,
        )
        .expect("colliding attachment");

        let collision_resolved = store
            .read_txn()
            .expect("read collision projection")
            .appearances_of(&first_canonical)
            .expect("existing appearance")
            .remove(0);
        assert_eq!(
            collision_resolved.id, initial.id,
            "identity must remain stable"
        );
        assert_ne!(collision_resolved.safe_name, initial.safe_name);
        assert_ne!(
            collision_resolved.metadata_version, initial.metadata_version,
            "Finder-visible rename must advance the metadata version"
        );

        let stable_id = collision_resolved.id.clone();
        let stable_name = collision_resolved.safe_name.clone();
        let stable_version = collision_resolved.metadata_version.clone();
        rebuild_projection(&mut store, scope()).expect("relaunch projection rebuild");
        let relaunched = store
            .read_txn()
            .expect("read relaunched projection")
            .appearances_of(&first_canonical)
            .expect("relaunched appearance")
            .remove(0);
        assert_eq!(relaunched.id, stable_id);
        assert_eq!(relaunched.safe_name, stable_name);
        assert_eq!(relaunched.metadata_version, stable_version);
    }

    #[test]
    fn attachment_availability_transitions_reversion_in_place_and_journal_an_update() {
        let mut store = store();
        let txn = store.write_txn().expect("write metadata");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");

        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![image_document_at(100, 40, 1_700_000_000, 600)],
                window: Some(CrawlWindow {
                    oldest_message_id: 40,
                    newest_message_id: 40,
                }),
                history_complete: true,
                skipped_malformed: 0,
            },
            1_000,
        )
        .expect("initial fetchable attachment");

        let key = AttachmentKey {
            message: MessageKey {
                chat: ChatKey {
                    scope: scope(),
                    chat_id: ChatId(100),
                },
                message_id: MessageId(40),
            },
            index: AttachmentIndex(0),
        };
        let canonical = ItemKey::Canonical(CanonicalKey::Attachment(key)).id();
        let initial = store
            .read_txn()
            .expect("read initial attachment")
            .appearances_of(&canonical)
            .expect("initial appearance")
            .remove(0);
        assert_eq!(initial.availability, ItemAvailability::Fetchable);

        let mut prior = initial;
        for (availability, can_be_saved) in [
            (StateAttachmentAvailability::Restricted, false),
            (StateAttachmentAvailability::Unavailable, false),
            (StateAttachmentAvailability::Fetchable, true),
        ] {
            let (mut facts, anchor) = {
                let read = store.read_txn().expect("read transition source");
                (
                    read.attachment(&key)
                        .expect("attachment")
                        .expect("attachment row")
                        .facts,
                    read.change_journal_state()
                        .expect("journal anchor")
                        .latest_sequence,
                )
            };
            facts.availability = availability;
            facts.can_be_saved = can_be_saved;
            let txn = store.write_txn().expect("write availability transition");
            txn.upsert_attachment(&facts)
                .expect("update attachment availability");
            txn.commit().expect("commit availability transition");
            rebuild_projection(&mut store, scope()).expect("reproject transition");

            let read = store.read_txn().expect("read projected transition");
            let updated = read
                .appearances_of(&canonical)
                .expect("updated appearance")
                .remove(0);
            assert_eq!(
                updated.id, prior.id,
                "availability must not replace identity"
            );
            assert_eq!(
                updated.safe_name, prior.safe_name,
                "availability must not rename the item"
            );
            let projected_availability = match availability {
                StateAttachmentAvailability::Fetchable => ItemAvailability::Fetchable,
                StateAttachmentAvailability::Restricted => ItemAvailability::Restricted,
                StateAttachmentAvailability::Unavailable
                | StateAttachmentAvailability::ViewOnce => ItemAvailability::Unavailable,
            };
            assert_eq!(updated.availability, projected_availability);
            assert_ne!(
                updated.metadata_version, prior.metadata_version,
                "Finder must receive a new metadata version for {availability:?}"
            );
            assert!(
                read.item_changes_since(scope().account, anchor, u32::MAX)
                    .expect("provider updates")
                    .iter()
                    .any(|change| change.item.id == updated.id),
                "the availability transition must be replayed to the provider"
            );
            prior = updated;
        }
    }

    #[test]
    fn attachment_content_version_ignores_locator_refresh_and_changes_with_content_identity() {
        let mut store = store();
        let txn = store.write_txn().expect("write metadata");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");

        let initial =
            image_document_revision(600, "remote-before", Some("stable-content"), 700, 1, None);
        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![initial],
                window: Some(CrawlWindow {
                    oldest_message_id: 40,
                    newest_message_id: 40,
                }),
                history_complete: true,
                skipped_malformed: 0,
            },
            1_000,
        )
        .expect("initial attachment");

        let key = AttachmentKey {
            message: MessageKey {
                chat: ChatKey {
                    scope: scope(),
                    chat_id: ChatId(100),
                },
                message_id: MessageId(40),
            },
            index: AttachmentIndex(0),
        };
        let canonical = ItemKey::Canonical(CanonicalKey::Attachment(key)).id();
        let initial_version = {
            let read = store.read_txn().expect("read initial attachment");
            let attachment = read.attachment(&key).expect("attachment").expect("row");
            let item = read
                .appearances_of(&canonical)
                .expect("initial appearance")
                .remove(0);
            assert_eq!(
                item.content
                    .as_ref()
                    .and_then(|content| content.content_version.as_ref()),
                Some(&attachment.facts.content_version)
            );
            attachment.facts.content_version
        };
        let blob_hash = ContentHash::Sha256([0x42; 32]);
        let txn = store.write_txn().expect("link verified bytes");
        txn.record_blob(scope().account, &blob_hash, 4096, 1_100)
            .expect("record blob");
        txn.link_attachment_blob(&key, &blob_hash, 1_100)
            .expect("link blob");
        txn.commit().expect("commit blob link");

        let locator_refresh = image_document_revision(
            601,
            "remote-after",
            Some("stable-content"),
            701,
            2,
            Some(1_700_000_010),
        );
        apply_live_commit(
            &mut store,
            scope(),
            &LiveCommit {
                chat_id: 100,
                changes: vec![LiveChange::Observed(Box::new(locator_refresh))],
                advance_newest: None,
                skipped_malformed: 0,
                refreshes_rejected: 0,
            },
            2_000,
        )
        .expect("locator refresh");
        {
            let read = store.read_txn().expect("read locator refresh");
            let attachment = read.attachment(&key).expect("attachment").expect("row");
            assert_eq!(attachment.facts.content_version, initial_version);
            assert_eq!(attachment.facts.telegram_local_file_id, Some(601));
            assert_eq!(
                attachment.facts.telegram_file_id.as_deref(),
                Some("remote-after")
            );
            assert_eq!(attachment.blob_hash, Some(blob_hash));
            assert_eq!(attachment.last_verified_at_ms, Some(1_100));
            let item = read
                .appearances_of(&canonical)
                .expect("refreshed appearance")
                .remove(0);
            assert_eq!(
                item.content.and_then(|content| content.content_version),
                Some(initial_version.clone())
            );
        }

        let replacement = image_document_revision(
            602,
            "remote-replacement",
            Some("replacement-content"),
            702,
            3,
            Some(1_700_000_020),
        );
        apply_live_commit(
            &mut store,
            scope(),
            &LiveCommit {
                chat_id: 100,
                changes: vec![LiveChange::Observed(Box::new(replacement))],
                advance_newest: None,
                skipped_malformed: 0,
                refreshes_rejected: 0,
            },
            3_000,
        )
        .expect("content replacement");
        let read = store.read_txn().expect("read content replacement");
        let attachment = read.attachment(&key).expect("attachment").expect("row");
        assert_ne!(attachment.facts.content_version, initial_version);
        assert_eq!(
            attachment.blob_hash, None,
            "verified bytes from the prior content version must not be reused"
        );
        assert_eq!(attachment.last_verified_at_ms, None);
        let item = read
            .appearances_of(&canonical)
            .expect("replacement appearance")
            .remove(0);
        assert_eq!(
            item.content.and_then(|content| content.content_version),
            Some(attachment.facts.content_version)
        );
    }

    #[test]
    fn attachment_without_stable_identity_invalidates_verified_bytes_on_main_generation_change() {
        let mut store = store();
        let txn = store.write_txn().expect("write metadata");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");

        let initial = image_document_revision(610, "remote-a", None, 710, 10, None);
        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![initial],
                window: Some(CrawlWindow {
                    oldest_message_id: 40,
                    newest_message_id: 40,
                }),
                history_complete: true,
                skipped_malformed: 0,
            },
            1_000,
        )
        .expect("initial attachment");

        let key = AttachmentKey {
            message: MessageKey {
                chat: ChatKey {
                    scope: scope(),
                    chat_id: ChatId(100),
                },
                message_id: MessageId(40),
            },
            index: AttachmentIndex(0),
        };
        let canonical = ItemKey::Canonical(CanonicalKey::Attachment(key)).id();
        let initial_version = store
            .read_txn()
            .expect("read initial attachment")
            .attachment(&key)
            .expect("attachment")
            .expect("row")
            .facts
            .content_version;
        let blob_hash = ContentHash::Sha256([0x43; 32]);
        let txn = store.write_txn().expect("link verified bytes");
        txn.record_blob(scope().account, &blob_hash, 4096, 1_100)
            .expect("record blob");
        txn.link_attachment_blob(&key, &blob_hash, 1_100)
            .expect("link blob");
        txn.commit().expect("commit blob link");

        let preview_only_refresh =
            image_document_revision(610, "remote-a", None, 711, 11, Some(1_700_000_010));
        apply_live_commit(
            &mut store,
            scope(),
            &LiveCommit {
                chat_id: 100,
                changes: vec![LiveChange::Observed(Box::new(preview_only_refresh))],
                advance_newest: None,
                skipped_malformed: 0,
                refreshes_rejected: 0,
            },
            2_000,
        )
        .expect("preview-only refresh");
        {
            let read = store.read_txn().expect("read preview-only refresh");
            let attachment = read.attachment(&key).expect("attachment").expect("row");
            assert_eq!(attachment.facts.content_version, initial_version);
            assert_eq!(attachment.blob_hash, Some(blob_hash));
            assert_eq!(attachment.last_verified_at_ms, Some(1_100));
        }

        let possible_replacement =
            image_document_revision(611, "remote-b", None, 712, 12, Some(1_700_000_020));
        apply_live_commit(
            &mut store,
            scope(),
            &LiveCommit {
                chat_id: 100,
                changes: vec![LiveChange::Observed(Box::new(possible_replacement))],
                advance_newest: None,
                skipped_malformed: 0,
                refreshes_rejected: 0,
            },
            3_000,
        )
        .expect("possible main-content replacement");

        let read = store.read_txn().expect("read possible replacement");
        let attachment = read.attachment(&key).expect("attachment").expect("row");
        assert_ne!(attachment.facts.content_version, initial_version);
        assert_eq!(attachment.facts.telegram_unique_id, None);
        assert_eq!(attachment.facts.telegram_local_file_id, Some(611));
        assert_eq!(
            attachment.facts.telegram_file_id.as_deref(),
            Some("remote-b")
        );
        assert_eq!(
            attachment.blob_hash, None,
            "a possible replacement without stable identity must not retain verified bytes"
        );
        assert_eq!(attachment.last_verified_at_ms, None);
        let item = read
            .appearances_of(&canonical)
            .expect("replacement appearance")
            .remove(0);
        assert_eq!(
            item.content.and_then(|content| content.content_version),
            Some(attachment.facts.content_version)
        );
    }

    #[test]
    fn processed_and_nonfetchable_media_persist_truthful_projection_facts() {
        let td_file = |id: i32, size: u64| {
            json!({
                "@type": "file",
                "id": id,
                "size": size,
                "remote": {
                    "id": format!("remote-{id}"),
                    "unique_id": format!("unique-{id}")
                }
            })
        };
        let records = vec![
            attachment_message_at(
                30,
                true,
                None,
                json!({
                    "@type": "messageVideo",
                    "video": {
                        "file_name": "sender-transport.mov",
                        "mime_type": "video/quicktime",
                        "video": td_file(530, 5_000)
                    }
                }),
            ),
            attachment_message_at(
                31,
                false,
                None,
                json!({
                    "@type": "messageDocument",
                    "document": {
                        "file_name": "protected.pdf",
                        "mime_type": "application/pdf",
                        "document": td_file(531, 6_000)
                    }
                }),
            ),
            attachment_message_at(
                32,
                true,
                Some(json!({"@type": "messageSelfDestructTypeImmediately"})),
                json!({
                    "@type": "messagePhoto",
                    "photo": {"sizes": [{
                        "type": "x", "width": 800, "height": 600,
                        "photo": td_file(532, 7_000)
                    }]}
                }),
            ),
            attachment_message_at(33, true, None, json!({"@type": "messageExpiredPhoto"})),
            attachment_message_at(
                34,
                true,
                None,
                json!({"@type": "messagePhoto", "photo": {"sizes": []}}),
            ),
        ];

        let mut store = store();
        let txn = store.write_txn().expect("write metadata");
        txn.set_display_timezone(scope().account, "Asia/Tbilisi", 10)
            .expect("display timezone");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");
        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records,
                window: Some(CrawlWindow {
                    oldest_message_id: 30,
                    newest_message_id: 34,
                }),
                history_complete: true,
                skipped_malformed: 0,
            },
            2_000,
        )
        .expect("attachment history");

        let read = store.read_txn().expect("read attachments");
        let attachment = |message_id| {
            read.attachments_of_message(&MessageKey {
                chat: ChatKey {
                    scope: scope(),
                    chat_id: ChatId(100),
                },
                message_id: MessageId(message_id),
            })
            .expect("attachment rows")
            .into_iter()
            .next()
            .expect("one attachment")
        };

        let video = attachment(30);
        assert_eq!(video.facts.logical_kind, StateAttachmentLogicalKind::Video);
        assert_eq!(
            video.facts.telegram_representation,
            StateTelegramRepresentation::Video
        );
        assert_eq!(
            video.facts.fidelity,
            StateAttachmentFidelity::TelegramVariant
        );
        assert_eq!(video.facts.source_name, None);
        assert_eq!(video.facts.mime_type.as_deref(), Some("video/quicktime"));
        let video_item = read
            .appearances_of(&ItemKey::Canonical(CanonicalKey::Attachment(video.facts.key)).id())
            .expect("video appearance")
            .remove(0);
        assert!(video_item.safe_name.ends_with(" video.mov"));

        let restricted = attachment(31);
        assert_eq!(
            restricted.facts.availability,
            StateAttachmentAvailability::Restricted
        );
        assert_eq!(
            restricted.facts.fidelity,
            StateAttachmentFidelity::MetadataOnly
        );
        assert!(!restricted.facts.can_be_saved);
        let restricted_item = read
            .appearances_of(
                &ItemKey::Canonical(CanonicalKey::Attachment(restricted.facts.key)).id(),
            )
            .expect("restricted appearance")
            .remove(0);
        assert_eq!(restricted_item.availability, ItemAvailability::Restricted);
        let view_once = attachment(32);
        assert_eq!(
            view_once.facts.availability,
            StateAttachmentAvailability::ViewOnce
        );
        assert_eq!(
            view_once.facts.fidelity,
            StateAttachmentFidelity::MetadataOnly
        );
        let view_once_item = read
            .appearances_of(&ItemKey::Canonical(CanonicalKey::Attachment(view_once.facts.key)).id())
            .expect("view-once appearance")
            .remove(0);
        assert_eq!(view_once_item.availability, ItemAvailability::Unavailable);
        for message_id in [33, 34] {
            let unavailable = attachment(message_id);
            assert_eq!(
                unavailable.facts.availability,
                StateAttachmentAvailability::Unavailable
            );
            assert_eq!(
                unavailable.facts.fidelity,
                StateAttachmentFidelity::MetadataOnly
            );
            assert_eq!(unavailable.facts.telegram_local_file_id, None);
            let item = read
                .appearances_of(
                    &ItemKey::Canonical(CanonicalKey::Attachment(unavailable.facts.key)).id(),
                )
                .expect("unavailable appearance")
                .remove(0);
            assert_eq!(item.availability, ItemAvailability::Unavailable);
        }
    }

    #[test]
    fn per_message_save_restriction_purges_audit_history_and_persists_only_placeholder() {
        const JULY_15_SECS: i64 = 1_784_116_800;

        for retention in [RetentionMode::Mirror, RetentionMode::Audit] {
            let mut store = store();
            add_main_chat(&mut store, 100, false);
            if retention == RetentionMode::Audit {
                let txn = store.write_txn().expect("Audit policy");
                txn.set_retention_mode(scope().account, retention, None, 10)
                    .expect("set Audit");
                txn.commit().expect("commit Audit");
            }
            apply_history_commit(
                &mut store,
                scope(),
                &HistoryCommit {
                    chat_id: 100,
                    records: vec![message_at(
                        100,
                        1,
                        "allowed-revision-that-must-not-survive",
                        JULY_15_SECS,
                    )],
                    window: Some(CrawlWindow {
                        oldest_message_id: 1,
                        newest_message_id: 1,
                    }),
                    history_complete: false,
                    skipped_malformed: 0,
                },
                1_000,
            )
            .expect("allowed revision");

            let restricted = normalize_message(&json!({
                "@type": "message",
                "id": 1,
                "chat_id": 100,
                "date": JULY_15_SECS,
                "edit_date": JULY_15_SECS + 1,
                "sender_id": {"@type": "messageSenderUser", "user_id": 77},
                "can_be_saved": false,
                "reply_to": {
                    "@type": "messageReplyToMessage",
                    "message_id": 99,
                    "quote": {
                        "@type": "textQuote",
                        "text": {
                            "@type": "formattedText",
                            "text": "restricted-quote",
                            "entities": []
                        }
                    }
                },
                "interaction_info": {
                    "reactions": {
                        "@type": "messageReactions",
                        "reactions": [{
                            "@type": "messageReaction",
                            "type": {
                                "@type": "reactionTypeEmoji",
                                "emoji": "restricted-reaction"
                            },
                            "total_count": 1,
                            "is_chosen": true
                        }]
                    }
                },
                "content": {
                    "@type": "messageText",
                    "text": {
                        "@type": "formattedText",
                        "text": "restricted-current-plaintext",
                        "entities": [{
                            "@type": "textEntity",
                            "offset": 0,
                            "length": 10,
                            "type": {
                                "@type": "textEntityTypeTextUrl",
                                "url": "https://restricted.example"
                            }
                        }]
                    }
                }
            }))
            .expect("restricted message");
            apply_live_commit(
                &mut store,
                scope(),
                &LiveCommit {
                    chat_id: 100,
                    changes: vec![LiveChange::Observed(Box::new(restricted))],
                    advance_newest: None,
                    skipped_malformed: 0,
                    refreshes_rejected: 0,
                },
                2_000,
            )
            .expect("restricted revision");

            let chat_key = ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            };
            let read = store.read_txn().expect("read restricted events");
            let events = read.events_after(&chat_key, 0, 10).expect("events");
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].payload, None);
            let current = events[1].payload.as_ref().expect("placeholder payload");
            let persisted: MessageRecord =
                serde_json::from_slice(&current.bytes).expect("decode placeholder");
            assert_eq!(persisted.reply, None);
            assert_eq!(persisted.topic, None);
            assert_eq!(persisted.album_id, None);
            assert!(persisted.reactions.is_empty());
            assert!(!persisted.can_be_saved);
            assert!(matches!(
                persisted.content,
                MessageContent::Restricted {
                    reason: ContentRestriction::SaveForbidden
                }
            ));
            let persisted_bytes = &current.bytes;
            for secret in [
                "allowed-revision-that-must-not-survive",
                "restricted-quote",
                "restricted-reaction",
                "restricted-current-plaintext",
                "https://restricted.example",
            ] {
                assert!(
                    !persisted_bytes
                        .windows(secret.len())
                        .any(|window| window == secret.as_bytes()),
                    "persisted restricted payload leaked {secret}"
                );
            }
        }
    }

    #[test]
    fn owned_commit_path_publishes_only_affected_months_across_every_live_view() {
        const JULY_15_SECS: i64 = 1_784_116_800;
        const AUGUST_3_SECS: i64 = 1_785_758_400;

        let mut store = store();
        let txn = store.write_txn().expect("write");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");
        rebuild_projection(&mut store, scope()).expect("initial projection");

        let chat_key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let cache_root = std::env::temp_dir().join(format!(
            "gramdrive-owned-monthly-render-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&cache_root);

        let july_history = HistoryCommit {
            chat_id: 100,
            records: vec![message_at(100, 1, "july", JULY_15_SECS)],
            window: Some(CrawlWindow {
                oldest_message_id: 1,
                newest_message_id: 1,
            }),
            history_complete: false,
            skipped_malformed: 0,
        };
        apply_history_commit(&mut store, scope(), &july_history, 1_000).expect("july commit");
        render_pending_months(&mut store, &cache_root, 1_100).expect("july render");
        assert_eq!(render_facts(&mut store, chat_key, 2026, 7).len(), 2);

        let txn = store.write_txn().expect("add archive view");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Archive,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("archive membership");
        txn.commit().expect("commit archive membership");
        rebuild_projection(&mut store, scope()).expect("project archive view");
        render_pending_months(&mut store, &cache_root, 1_150).expect("publish archive view");

        let july = render_facts(&mut store, chat_key, 2026, 7);
        assert_eq!(july.len(), 4, "two formats in both live views");
        for view in [ChatListKind::Main, ChatListKind::Archive] {
            let chat_item = ItemKey::Appearance(AppearanceKey {
                view,
                item: CanonicalKey::Chat(chat_key),
            })
            .id();
            let names: BTreeSet<_> = store
                .read_txn()
                .expect("read children")
                .stored_children(&chat_item)
                .expect("chat children")
                .into_iter()
                .map(|item| item.display_name)
                .collect();
            assert_eq!(
                names,
                BTreeSet::from([".chat.json".to_owned(), "2026-07".to_owned(),])
            );
        }

        let replay_anchor = store
            .read_txn()
            .expect("read journal")
            .change_journal_state()
            .expect("journal")
            .latest_sequence;
        apply_history_commit(&mut store, scope(), &july_history, 1_200).expect("history replay");
        render_pending_months(&mut store, &cache_root, 1_300).expect("render replay");
        assert_eq!(render_facts(&mut store, chat_key, 2026, 7), july);
        assert!(
            store
                .read_txn()
                .expect("read changes")
                .item_changes_since(scope().account, replay_anchor, 100)
                .expect("replay changes")
                .is_empty(),
            "byte-identical replay must be provider-journal quiet"
        );

        let august_insert = LiveCommit {
            chat_id: 100,
            changes: vec![LiveChange::Observed(Box::new(message_at(
                100,
                2,
                "august",
                AUGUST_3_SECS,
            )))],
            advance_newest: Some(2),
            skipped_malformed: 0,
            refreshes_rejected: 0,
        };
        apply_live_commit(&mut store, scope(), &august_insert, 2_000).expect("august insert");
        render_pending_months(&mut store, &cache_root, 2_100).expect("august render");
        assert_eq!(render_facts(&mut store, chat_key, 2026, 7), july);
        let august = render_facts(&mut store, chat_key, 2026, 8);
        assert_eq!(august.len(), 4);

        let edit_anchor = store
            .read_txn()
            .expect("read journal")
            .change_journal_state()
            .expect("journal")
            .latest_sequence;
        let mut edited = message_at(100, 1, "july edited", JULY_15_SECS);
        edited.edited_at_ms = Some(JULY_15_SECS.saturating_mul(1_000) + 500);
        let july_edit = LiveCommit {
            chat_id: 100,
            changes: vec![LiveChange::Observed(Box::new(edited))],
            advance_newest: None,
            skipped_malformed: 0,
            refreshes_rejected: 0,
        };
        apply_live_commit(&mut store, scope(), &july_edit, 3_000).expect("july edit");
        render_pending_months(&mut store, &cache_root, 3_100).expect("edited render");
        let july_edited = render_facts(&mut store, chat_key, 2026, 7);
        assert_ne!(july_edited, july);
        assert_eq!(render_facts(&mut store, chat_key, 2026, 8), august);
        let edit_changes = store
            .read_txn()
            .expect("read changes")
            .item_changes_since(scope().account, edit_anchor, 100)
            .expect("edit changes");
        // Both formats in both views, plus the July directory and the chat
        // directory in each view: re-rendering July changed the documents'
        // logical sizes, and a folder whose contents changed size is a
        // folder whose published size changed too (BUG-260728-2qfzbd).
        // August is untouched in every view.
        assert_eq!(
            edit_changes.len(),
            8,
            "both formats, their July directory, and the chat directory, in both views"
        );
        assert!(edit_changes.iter().all(|change| {
            match change.item.id.key() {
                ItemKey::Appearance(AppearanceKey {
                    item: CanonicalKey::GeneratedDoc(document),
                    ..
                }) => {
                    document.partition
                        == (DocPartition::Month {
                            year: 2026,
                            month: 7,
                        })
                }
                ItemKey::Appearance(AppearanceKey {
                    item: CanonicalKey::MonthDir(dir),
                    ..
                }) => (dir.year, dir.month) == (2026, 7),
                ItemKey::Appearance(AppearanceKey {
                    item: CanonicalKey::Chat(_),
                    ..
                }) => true,
                _ => false,
            }
        }));
        let july_watermarks: BTreeSet<_> = {
            let read = store.read_txn().expect("read render states");
            read.month_render_catalog(chat_key, 2026, 7)
                .expect("catalog")
                .into_iter()
                .map(|entry| {
                    read.render_state(&entry.item)
                        .expect("render state")
                        .expect("state")
                        .input_watermark_seq
                })
                .collect()
        };
        assert_eq!(july_watermarks.len(), 1, "all appearances advance together");

        let delete_anchor = store
            .read_txn()
            .expect("read journal")
            .change_journal_state()
            .expect("journal")
            .latest_sequence;
        let july_delete = LiveCommit {
            chat_id: 100,
            changes: vec![LiveChange::Deleted { message_id: 1 }],
            advance_newest: None,
            skipped_malformed: 0,
            refreshes_rejected: 0,
        };
        apply_live_commit(&mut store, scope(), &july_delete, 4_000).expect("july delete");
        render_pending_months(&mut store, &cache_root, 4_100).expect("deleted render");
        assert_eq!(render_facts(&mut store, chat_key, 2026, 8), august);
        let delete_changes = store
            .read_txn()
            .expect("read changes")
            .item_changes_since(scope().account, delete_anchor, 100)
            .expect("delete changes");
        assert_eq!(delete_changes.len(), 8);
        assert!(delete_changes.iter().all(|change| {
            match change.item.id.key() {
                ItemKey::Appearance(AppearanceKey {
                    item: CanonicalKey::GeneratedDoc(document),
                    ..
                }) => {
                    document.partition
                        == (DocPartition::Month {
                            year: 2026,
                            month: 7,
                        })
                }
                ItemKey::Appearance(AppearanceKey {
                    item: CanonicalKey::MonthDir(dir),
                    ..
                }) => (dir.year, dir.month) == (2026, 7),
                ItemKey::Appearance(AppearanceKey {
                    item: CanonicalKey::Chat(_),
                    ..
                }) => true,
                _ => false,
            }
        }));

        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn owned_policy_path_republishes_retention_changes_at_one_message_watermark() {
        const JULY_15_SECS: i64 = 1_784_116_800;

        let mut store = store();
        let txn = store.write_txn().expect("write");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");
        rebuild_projection(&mut store, scope()).expect("initial projection");

        let chat_key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let cache_root = std::env::temp_dir().join(format!(
            "gramdrive-retention-republication-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&cache_root);
        let history = HistoryCommit {
            chat_id: 100,
            records: vec![message_at(100, 1, "original", JULY_15_SECS)],
            window: Some(CrawlWindow {
                oldest_message_id: 1,
                newest_message_id: 1,
            }),
            history_complete: false,
            skipped_malformed: 0,
        };
        apply_history_commit(&mut store, scope(), &history, 1_000).expect("history");
        render_pending_months(&mut store, &cache_root, 1_100).expect("mirror render");
        let mirror_initial = render_facts(&mut store, chat_key, 2026, 7);
        let initial_watermark = store
            .read_txn()
            .expect("read watermark")
            .latest_event_seq(&chat_key)
            .expect("watermark");

        let audit_change = {
            let txn = store.write_txn().expect("audit transition");
            let change = txn
                .set_retention_mode(scope().account, RetentionMode::Audit, None, 2_000)
                .expect("set audit");
            txn.commit().expect("commit audit");
            change
        };
        assert!(audit_change.changed());
        assert_eq!(audit_change.purged_events, 0);
        render_pending_months(&mut store, &cache_root, 2_100)
            .expect("same-watermark audit publication");
        let audit_initial = render_facts(&mut store, chat_key, 2026, 7);
        assert_ne!(audit_initial, mirror_initial);
        assert!(audit_initial.iter().all(|fact| {
            fact.content_version.contains("/g1/retention-audit/") && fact.path.contains("-g1-w")
        }));
        assert_eq!(
            store
                .read_txn()
                .expect("read watermark")
                .latest_event_seq(&chat_key)
                .expect("watermark"),
            initial_watermark,
            "a policy-only publication does not fabricate a message event"
        );

        let mut edited = message_at(100, 1, "edited in audit", JULY_15_SECS);
        edited.edited_at_ms = Some(JULY_15_SECS.saturating_mul(1_000) + 500);
        apply_live_commit(
            &mut store,
            scope(),
            &LiveCommit {
                chat_id: 100,
                changes: vec![LiveChange::Observed(Box::new(edited))],
                advance_newest: None,
                skipped_malformed: 0,
                refreshes_rejected: 0,
            },
            3_000,
        )
        .expect("audit edit");
        render_pending_months(&mut store, &cache_root, 3_100).expect("audit edit render");
        let audit_watermark = store
            .read_txn()
            .expect("read watermark")
            .latest_event_seq(&chat_key)
            .expect("watermark");

        let mirror_change = {
            let txn = store.write_txn().expect("mirror transition");
            let change = txn
                .set_retention_mode(
                    scope().account,
                    RetentionMode::Mirror,
                    Some(
                        gramdrive_state::repo::AuditToMirrorConfirmation::parse(
                            scope().account,
                            &gramdrive_state::repo::AuditToMirrorConfirmation::expected_phrase(
                                scope().account,
                            ),
                        )
                        .expect("confirmation"),
                    ),
                    4_000,
                )
                .expect("set mirror");
            txn.commit().expect("commit mirror");
            change
        };
        assert!(mirror_change.changed());
        assert_eq!(mirror_change.purged_events, 1, "superseded payload purged");
        render_pending_months(&mut store, &cache_root, 4_100)
            .expect("same-watermark mirror publication");
        let mirror_again = render_facts(&mut store, chat_key, 2026, 7);
        assert!(mirror_again.iter().all(|fact| {
            fact.content_version.contains("/g2/retention-mirror/") && fact.path.contains("-g2-w")
        }));
        assert_eq!(
            store
                .read_txn()
                .expect("read watermark")
                .latest_event_seq(&chat_key)
                .expect("watermark"),
            audit_watermark
        );
        let events = store
            .read_txn()
            .expect("read events")
            .events_after(&chat_key, 0, 10)
            .expect("events");
        assert_eq!(events.len(), 2);
        assert!(events[0].payload.is_none(), "superseded content was purged");
        assert!(events[1].payload.is_some(), "current revision remains");
        let read = store.read_txn().expect("read states");
        assert!(
            read.month_render_catalog(chat_key, 2026, 7)
                .expect("catalog")
                .into_iter()
                .all(|entry| !read
                    .render_state(&entry.item)
                    .expect("state")
                    .expect("render state")
                    .dirty)
        );
        drop(read);
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn chat_protection_transaction_redacts_live_state_in_every_policy_combination() {
        const JULY_15_SECS: i64 = 1_784_116_800;

        for retention in [RetentionMode::Mirror, RetentionMode::Audit] {
            for archive_mode in [false, true] {
                let mut store = store();
                let txn = store.write_txn().expect("write policy fixture");
                txn.upsert_chat(
                    &snapshot_chat_record(scope(), &chat(100, "Chat")).expect("chat record"),
                )
                .expect("chat");
                txn.replace_chat_list(
                    &ChatListKey {
                        scope: scope(),
                        kind: ChatListKind::Main,
                    },
                    &[ChatListEntry {
                        chat_id: ChatId(100),
                        sort_order: 10,
                        pinned: false,
                    }],
                )
                .expect("main membership");
                if retention == RetentionMode::Audit {
                    txn.set_retention_mode(scope().account, retention, None, 10)
                        .expect("Audit mode");
                }
                txn.set_archive_mode(scope().account, archive_mode, 11)
                    .expect("Archive mode");
                txn.commit().expect("commit policy fixture");
                rebuild_projection(&mut store, scope()).expect("initial projection");

                let history = HistoryCommit {
                    chat_id: 100,
                    records: vec![
                        message_at(100, 1, "plaintext-that-must-be-purged", JULY_15_SECS),
                        image_document_at(100, 2, JULY_15_SECS + 1, 702),
                    ],
                    window: Some(CrawlWindow {
                        oldest_message_id: 1,
                        newest_message_id: 2,
                    }),
                    history_complete: false,
                    skipped_malformed: 0,
                };
                apply_history_commit(&mut store, scope(), &history, 1_000)
                    .expect("allowed history");
                let chat_key = ChatKey {
                    scope: scope(),
                    chat_id: ChatId(100),
                };
                let attachment_key = AttachmentKey {
                    message: MessageKey {
                        chat: chat_key,
                        message_id: MessageId(2),
                    },
                    index: AttachmentIndex(0),
                };
                let attachment_canonical =
                    ItemKey::Canonical(CanonicalKey::Attachment(attachment_key)).id();
                let attachment_appearance = store
                    .read_txn()
                    .expect("read attachment appearance")
                    .appearances_of(&attachment_canonical)
                    .expect("attachment appearances")
                    .remove(0)
                    .id;
                assert_eq!(
                    store
                        .read_txn()
                        .expect("read initial pin")
                        .pin(&attachment_appearance)
                        .expect("pin")
                        .is_some(),
                    archive_mode
                );

                let cache_root = std::env::temp_dir().join(format!(
                    "gramdrive-protected-message-render-{}-{:?}-{retention:?}-{archive_mode}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                let _ = std::fs::remove_dir_all(&cache_root);
                render_pending_months(&mut store, &cache_root, 1_100)
                    .expect("pre-protection render");
                let rendered = render_facts(&mut store, chat_key, 2026, 7);
                assert_eq!(rendered.len(), 2);
                let secret = b"plaintext-that-must-be-purged";
                assert!(rendered.iter().any(|fact| {
                    fact.bytes
                        .windows(secret.len())
                        .any(|window| window == secret)
                }));
                let mut old_paths: BTreeSet<_> =
                    rendered.iter().map(|fact| fact.path.clone()).collect();
                let read = store.read_txn().expect("read chat metadata cache");
                for item_id in read
                    .generated_document_items_of_chat(&chat_key)
                    .expect("generated documents")
                {
                    let ItemKey::Appearance(AppearanceKey {
                        item: CanonicalKey::GeneratedDoc(document),
                        ..
                    }) = item_id.key()
                    else {
                        continue;
                    };
                    if document.partition == DocPartition::Chat {
                        old_paths.insert(
                            read.cache_entry(&item_id)
                                .expect("chat metadata cache")
                                .expect("chat metadata materialization")
                                .materialization_ref
                                .expect("chat metadata cache path"),
                        );
                    }
                }
                assert_eq!(
                    old_paths.len(),
                    3,
                    "all generated documents have bytes to purge"
                );
                drop(read);

                let protected_batch = UpdateBatch {
                    chats: vec![ChatMetadata {
                        chat_id: 100,
                        kind: SnapshotChatKind::Private,
                        title: "Chat".to_owned(),
                        username: None,
                        is_protected: true,
                        photo: None,
                    }],
                    memberships: Vec::new(),
                    ..UpdateBatch::default()
                };
                apply_update_batch(&mut store, scope(), &protected_batch).expect("protect chat");
                // Replaying the authoritative update is the crash-recovery
                // boundary: it must converge without new purge ownership.
                apply_update_batch(&mut store, scope(), &protected_batch)
                    .expect("replay protected chat");

                let read = store.read_txn().expect("read protected state");
                let events = read.events_after(&chat_key, 0, 10).expect("events");
                assert_eq!(events.len(), 2);
                assert!(events.iter().all(|event| event.payload.is_none()));
                let attachment = read
                    .attachment(&attachment_key)
                    .expect("attachment")
                    .expect("restricted placeholder");
                assert_eq!(attachment.facts.source_name, None);
                assert_eq!(attachment.facts.mime_type, None);
                assert_eq!(attachment.facts.exact_size, None);
                assert_eq!(attachment.facts.telegram_unique_id, None);
                assert_eq!(attachment.facts.telegram_local_file_id, None);
                assert_eq!(attachment.facts.telegram_file_id, None);
                assert_eq!(attachment.facts.file_reference, None);
                assert_eq!(
                    attachment.facts.availability,
                    StateAttachmentAvailability::Restricted
                );
                assert!(!attachment.facts.can_be_saved);
                assert_eq!(attachment.blob_hash, None);
                assert!(
                    read.pin(&attachment_appearance)
                        .expect("restricted pin")
                        .is_none()
                );

                let attachment_item = read
                    .appearances_of(&attachment_canonical)
                    .expect("attachment appearances")
                    .remove(0);
                assert_eq!(attachment_item.availability, ItemAvailability::Restricted);
                assert!(!attachment_item.safe_name.contains("scan"));
                let attachment_content = attachment_item.content.expect("restricted content facts");
                assert_eq!(attachment_content.mime_type, None);
                assert_eq!(attachment_content.logical_size, None);

                for item_id in read
                    .generated_document_items_of_chat(&chat_key)
                    .expect("render catalog")
                {
                    let item = read.item(&item_id).expect("doc item").expect("doc");
                    assert_eq!(item.availability, ItemAvailability::Restricted);
                    assert_eq!(item.content.and_then(|facts| facts.content_version), None);
                    assert!(read.cache_entry(&item_id).expect("doc cache").is_none());
                    let state = read
                        .render_state(&item_id)
                        .expect("render state")
                        .expect("state");
                    assert!(!state.dirty);
                    assert_eq!(state.content_version, None);
                    assert_eq!(state.content_hash, None);
                    assert_eq!(
                        state.skip_reason,
                        Some(gramdrive_state::repo::RenderSkipReason::PolicyExcluded)
                    );
                }
                let queued_paths: BTreeSet<_> = read
                    .retention_purge_queue(scope().account, 10)
                    .expect("purge queue")
                    .into_iter()
                    .map(|entry| entry.materialization_ref)
                    .collect();
                assert_eq!(queued_paths, old_paths);
                drop(read);

                let mut released_batch = protected_batch;
                released_batch.chats[0].is_protected = false;
                apply_update_batch(&mut store, scope(), &released_batch)
                    .expect("release protection");
                let read = store.read_txn().expect("read released state");
                assert!(
                    read.events_after(&chat_key, 0, 10)
                        .expect("events after release")
                        .iter()
                        .all(|event| event.payload.is_none()),
                    "protection removal must not resurrect plaintext"
                );
                let attachment = read
                    .attachment(&attachment_key)
                    .expect("attachment after release")
                    .expect("placeholder after release");
                assert_eq!(
                    attachment.facts.availability,
                    StateAttachmentAvailability::Restricted
                );
                assert_eq!(attachment.facts.telegram_local_file_id, None);
                assert_eq!(attachment.facts.source_name, None);
                assert!(
                    read.month_render_catalog(chat_key, 2026, 7)
                        .expect("released catalog")
                        .iter()
                        .all(|entry| read
                            .render_state(&entry.item)
                            .expect("render state")
                            .expect("state")
                            .dirty)
                );
                drop(read);
                let _ = std::fs::remove_dir_all(cache_root);
            }
        }
    }

    #[test]
    fn timezone_transition_atomically_remaps_boundary_month_and_republishes() {
        // 2026-06-30 21:30 UTC is 2026-07-01 01:30 in Asia/Tbilisi.
        const BOUNDARY_SECS: i64 = 1_782_855_000;

        let mut store = store();
        let txn = store.write_txn().expect("write");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");
        rebuild_projection(&mut store, scope()).expect("initial projection");
        let chat_key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let cache_root = std::env::temp_dir().join(format!(
            "gramdrive-timezone-repartition-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&cache_root);
        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![message_at(100, 1, "boundary", BOUNDARY_SECS)],
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 1,
                }),
                history_complete: false,
                skipped_malformed: 0,
            },
            1_000,
        )
        .expect("history");
        drain_render_jobs(&mut store, &cache_root, 1_100);
        assert_eq!(render_facts(&mut store, chat_key, 2026, 6).len(), 2);
        let watermark = store
            .read_txn()
            .expect("read watermark")
            .latest_event_seq(&chat_key)
            .expect("watermark");

        let change = {
            DisplayTimeZone::named("Asia/Tbilisi").expect("valid policy timezone");
            let txn = store.write_txn().expect("timezone transition");
            let change = txn
                .set_display_timezone(scope().account, "Asia/Tbilisi", 2_000)
                .expect("set timezone");
            reconcile_projection_txn(&txn, scope()).expect("repartition in transition");
            txn.commit().expect("atomic timezone transition");
            change
        };
        assert!(change.changed());
        assert_eq!(change.previous, "UTC");
        assert_eq!(change.current, "Asia/Tbilisi");
        assert!(change.invalidated_docs >= 2);
        assert!(
            store
                .read_txn()
                .expect("read old catalog")
                .month_render_catalog(chat_key, 2026, 6)
                .expect("old catalog")
                .is_empty(),
            "old UTC month appearances are tombstoned in the policy commit"
        );

        drain_render_jobs(&mut store, &cache_root, 2_100);
        let july = render_facts(&mut store, chat_key, 2026, 7);
        assert_eq!(july.len(), 2);
        assert!(july.iter().all(|fact| {
            fact.content_version
                .contains("/g1/retention-mirror/tz-Asia/Tbilisi")
                && fact.path.contains("-g1-w")
        }));
        assert!(july.iter().any(|fact| {
            String::from_utf8_lossy(&fact.bytes).contains("\"date_ms\":1782855000000")
        }));
        assert_eq!(
            store
                .read_txn()
                .expect("read watermark")
                .latest_event_seq(&chat_key)
                .expect("watermark"),
            watermark,
            "source timestamp and message watermark remain absolute/stable"
        );
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn live_commit_merges_newest_with_history_and_replays_edits_deletes_once() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        let history = HistoryCommit {
            chat_id: 100,
            records: vec![message(100, 1, "one"), message(100, 2, "two")],
            window: Some(CrawlWindow {
                oldest_message_id: 1,
                newest_message_id: 2,
            }),
            history_complete: true,
            skipped_malformed: 0,
        };
        apply_history_commit(&mut store, scope(), &history, 1_000).expect("history");

        let mut edited = message(100, 2, "two edited");
        edited.edited_at_ms = Some(1_700_000_500_000);
        let live = LiveCommit {
            chat_id: 100,
            changes: vec![
                LiveChange::Observed(Box::new(message(100, 3, "three"))),
                LiveChange::Observed(Box::new(edited)),
                LiveChange::Deleted { message_id: 1 },
            ],
            advance_newest: Some(3),
            skipped_malformed: 0,
            refreshes_rejected: 0,
        };
        apply_live_commit(&mut store, scope(), &live, 2_000).expect("live");
        apply_live_commit(&mut store, scope(), &live, 3_000).expect("live replay");

        let key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let read = store.read_txn().expect("read");
        assert_eq!(
            read.chat_sync_state(&key).expect("sync").expect("row"),
            ChatSyncRecord {
                window: Some(SyncWindow {
                    oldest: MessageId(1),
                    newest: MessageId(3),
                }),
                history_complete: true,
                last_sync_at_ms: Some(3_000),
            }
        );
        let events = read.events_after(&key, 0, 100).expect("events");
        assert_eq!(events.len(), 5, "observe 1/2, observe 3, edit 2, delete 1");
    }

    #[test]
    fn relaunch_tracks_durable_chats_before_readiness_and_commits_deletes() {
        let mut store = store();
        add_chat(&mut store, 100, false);
        apply_history_commit(
            &mut store,
            scope(),
            &HistoryCommit {
                chat_id: 100,
                records: vec![message(100, 1, "one")],
                window: Some(CrawlWindow {
                    oldest_message_id: 1,
                    newest_message_id: 1,
                }),
                history_complete: false,
                skipped_malformed: 0,
            },
            1_000,
        )
        .expect("history");
        let mut coordinator =
            ContentCoordinator::new(&mut store, scope()).expect("relaunch coordinator");
        coordinator.live.on_update(&json!({
            "@type": "updateDeleteMessages",
            "chat_id": 100,
            "message_ids": [1],
            "is_permanent": true,
            "from_cache": false,
        }));
        assert_eq!(coordinator.live.buffer_usage().chats, 0);
        drain_pre_ready_live(&mut store, scope(), &mut coordinator.live).expect("pre-ready commit");

        let read = store.read_txn().expect("read");
        let events = read
            .events_after(
                &ChatKey {
                    scope: scope(),
                    chat_id: ChatId(100),
                },
                0,
                10,
            )
            .expect("events");
        assert_eq!(events.len(), 2);
        assert_eq!(
            events.last().expect("delete").kind,
            MessageEventKind::Deleted
        );
    }

    #[test]
    fn visible_and_requested_demand_preempt_only_at_scheduler_boundaries() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        add_main_chat(&mut store, 200, false);
        initialize_content_progress(&mut store, scope()).expect("progress");
        let demand = Mutex::new(ContentDemandState::default());
        {
            let mut held = demand.lock().expect("demand");
            held.set(100, ChatHistoryPriority::Requested);
            held.set(200, ChatHistoryPriority::Visible);
        }
        let scheduler = BackfillScheduler::with_defaults();
        assert_eq!(
            open_next_history_turn(&mut store, scope(), &scheduler, &demand, 1_000)
                .expect("plan")
                .expect("visible work"),
            (ChatId(200), BackfillPriority::Visible)
        );

        // The visible chat has now had the turn its hint bought, so releasing
        // it really does hand the next boundary to the requested chat.
        demand
            .lock()
            .expect("demand")
            .set(200, ChatHistoryPriority::Background);
        assert_eq!(
            open_next_history_turn(&mut store, scope(), &scheduler, &demand, 2_000)
                .expect("plan")
                .expect("requested work"),
            (ChatId(100), BackfillPriority::Requested)
        );
    }

    /// An ordinary Finder open signals `visible` and, once the enumerator is
    /// invalidated, `background` again — often within the same millisecond and
    /// always while the namespace worker is still inside another chat's crawl.
    /// The hint must still buy the chat a history turn (BUG-260728-2qfzbd).
    ///
    /// Before this, the release erased the demand entry outright, the worker's
    /// next snapshot saw nothing, and an opened chat never advanced at all: on
    /// the installed profile the same chat only moved when a `visible` hint was
    /// injected by hand over the control socket and never released.
    #[test]
    fn a_visible_hint_released_before_the_scheduler_snapshots_still_gets_its_turn() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        add_main_chat(&mut store, 200, false);
        initialize_content_progress(&mut store, scope()).expect("progress");
        let scheduler = BackfillScheduler::with_defaults();
        let demand = Mutex::new(ContentDemandState::default());

        // Chat 100 is the head of the background rotation, so anything that
        // loses the foreground edge picks it instead of the opened chat.
        assert_eq!(
            open_next_history_turn(&mut store, scope(), &scheduler, &demand, 1_000)
                .expect("plan")
                .expect("background work")
                .0,
            ChatId(100)
        );

        // The whole Finder demand lifecycle lands between two scheduler
        // boundaries: nothing observes the queue while it is visible.
        {
            let mut held = demand.lock().expect("demand");
            held.set(200, ChatHistoryPriority::Visible);
            held.set(200, ChatHistoryPriority::Background);
            assert!(
                held.live_snapshot().0.is_empty(),
                "the live view must follow the release exactly"
            );
        }

        assert_eq!(
            open_next_history_turn(&mut store, scope(), &scheduler, &demand, 2_000)
                .expect("plan")
                .expect("the admitted chat"),
            (ChatId(200), BackfillPriority::Visible),
            "an admitted hint owes the chat one turn even after its release"
        );
        assert!(
            demand.lock().expect("demand").unspent.is_empty(),
            "the admission is spent by the turn it bought"
        );

        // And it is exactly one turn: the ledger does not pin the chat to the
        // front of the rotation forever.
        assert_eq!(
            open_next_history_turn(&mut store, scope(), &scheduler, &demand, 3_000)
                .expect("plan")
                .expect("background work")
                .0,
            ChatId(100),
            "a spent admission must return the queue to ordinary rotation"
        );
    }

    /// Reading a file inside a chat is the drive's reliable "the user is in
    /// this chat" signal, because an ordinary folder open is answered by macOS
    /// out of its own replica and never reaches the extension at all. The
    /// provider raises `requested` for the enclosing chat while the read runs
    /// and releases it when the read settles — a small local render can settle
    /// well inside one scheduler boundary, so the release routinely wins the
    /// race, exactly like the enumerator's pair (BUG-260728-2qfzbd).
    #[test]
    fn a_content_read_hint_released_before_the_scheduler_snapshots_still_gets_its_turn() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        add_main_chat(&mut store, 200, false);
        initialize_content_progress(&mut store, scope()).expect("progress");
        let scheduler = BackfillScheduler::with_defaults();
        let demand = Mutex::new(ContentDemandState::default());

        // Chat 100 heads the background rotation, so a lost edge is visible as
        // the scheduler picking it instead of the chat that was read.
        assert_eq!(
            open_next_history_turn(&mut store, scope(), &scheduler, &demand, 1_000)
                .expect("plan")
                .expect("background work")
                .0,
            ChatId(100)
        );

        {
            let mut held = demand.lock().expect("demand");
            held.set(200, ChatHistoryPriority::Requested);
            held.set(200, ChatHistoryPriority::Background);
            assert!(
                held.live_snapshot().1.is_empty(),
                "the live view must follow the release exactly"
            );
        }

        assert_eq!(
            open_next_history_turn(&mut store, scope(), &scheduler, &demand, 2_000)
                .expect("plan")
                .expect("the chat that was read"),
            (ChatId(200), BackfillPriority::Requested),
            "a settled content read still owes its chat one turn"
        );
        assert_eq!(
            open_next_history_turn(&mut store, scope(), &scheduler, &demand, 3_000)
                .expect("plan")
                .expect("background work")
                .0,
            ChatId(100),
            "and exactly one turn: the read must not pin the chat to the front"
        );
    }

    /// A hint that arrives while the account is paced or paused is not lost —
    /// those plans never read the demand lists, so they cannot have honored it.
    #[test]
    fn an_admission_survives_a_plan_that_never_reached_the_demand_lists() {
        let held = [
            BackfillStep::Paused,
            BackfillStep::Wait {
                until_ms: 5_000,
                reason: WaitReason::FloodWait,
            },
            BackfillStep::Idle {
                reason: IdleReason::Offline,
            },
        ];
        for step in held {
            let mut demand = ContentDemandState::default();
            demand.set(200, ChatHistoryPriority::Visible);
            demand.set(200, ChatHistoryPriority::Background);
            let watermark = demand.scheduling_snapshot().watermark;
            settle_admissions(&mut demand, step, watermark);
            assert_eq!(
                demand.scheduling_snapshot().visible,
                vec![ChatId(200)],
                "{step:?} never looked at demand, so it cannot spend an admission"
            );
        }
    }

    /// `plan_next` runs outside the demand lock, so a Finder open can land
    /// while it is deciding. That hint was never offered to it and must not be
    /// retired by its outcome.
    #[test]
    fn a_hint_that_arrives_while_a_plan_runs_is_not_spent_by_it() {
        let mut demand = ContentDemandState::default();
        demand.set(100, ChatHistoryPriority::Visible);
        demand.set(100, ChatHistoryPriority::Background);
        let watermark = demand.scheduling_snapshot().watermark;

        // The provider signals mid-plan; the plan then reports it walked both
        // foreground tiers, which is true of the queue it was actually handed.
        demand.set(200, ChatHistoryPriority::Visible);
        demand.set(200, ChatHistoryPriority::Background);
        settle_admissions(
            &mut demand,
            BackfillStep::AdvanceHistory {
                chat_id: ChatId(900),
                priority: BackfillPriority::Background,
            },
            watermark,
        );

        assert_eq!(
            demand.scheduling_snapshot().visible,
            vec![ChatId(200)],
            "only the admissions the plan was handed may be retired by it"
        );
    }

    /// The converse: a plan that reached past a demand tier proves it walked
    /// that whole tier and found nothing there still needing history, so those
    /// admissions are spent rather than left to be re-offered forever.
    #[test]
    fn reaching_past_a_demand_tier_spends_the_admissions_in_it() {
        let ledger = || {
            let mut demand = ContentDemandState::default();
            demand.set(200, ChatHistoryPriority::Visible);
            demand.set(300, ChatHistoryPriority::Requested);
            demand.set(200, ChatHistoryPriority::Background);
            demand.set(300, ChatHistoryPriority::Background);
            let watermark = demand.scheduling_snapshot().watermark;
            (demand, watermark)
        };
        let settled = |demand: &ContentDemandState| {
            let plan = demand.scheduling_snapshot();
            (plan.visible, plan.requested)
        };

        let (mut demand, watermark) = ledger();
        settle_admissions(
            &mut demand,
            BackfillStep::AdvanceHistory {
                chat_id: ChatId(300),
                priority: BackfillPriority::Requested,
            },
            watermark,
        );
        assert_eq!(
            settled(&demand),
            (Vec::new(), Vec::new()),
            "requested work is only reached after the whole visible tier"
        );

        let (mut demand, watermark) = ledger();
        settle_admissions(
            &mut demand,
            BackfillStep::AdvanceHistory {
                chat_id: ChatId(900),
                priority: BackfillPriority::Background,
            },
            watermark,
        );
        assert_eq!(
            settled(&demand),
            (Vec::new(), Vec::new()),
            "background work is only reached after both foreground tiers"
        );

        let (mut demand, watermark) = ledger();
        settle_admissions(
            &mut demand,
            BackfillStep::AdvanceHistory {
                chat_id: ChatId(200),
                priority: BackfillPriority::Visible,
            },
            watermark,
        );
        assert_eq!(
            settled(&demand),
            (Vec::new(), vec![ChatId(300)]),
            "the visible walk stops at its first hit, so deeper tiers are unspent"
        );
    }

    /// The ledger is a bounded queue, not a growing set: a client that signals
    /// without ever letting the scheduler run cannot make it grow.
    #[test]
    fn the_admission_ledger_is_bounded_and_drops_its_oldest_first() {
        let mut demand = ContentDemandState::default();
        let total = i64::try_from(MAX_UNSPENT_ADMISSIONS).expect("bound") + 10;
        for chat_id in 1..=total {
            demand.set(chat_id, ChatHistoryPriority::Visible);
            demand.set(chat_id, ChatHistoryPriority::Background);
        }
        let visible = demand.scheduling_snapshot().visible;
        assert_eq!(visible.len(), MAX_UNSPENT_ADMISSIONS);
        assert_eq!(
            visible.first().copied(),
            Some(ChatId(
                total - i64::try_from(MAX_UNSPENT_ADMISSIONS).expect("bound") + 1
            )),
            "the bound evicts the oldest admission, keeping the newest gestures"
        );
        assert_eq!(visible.last().copied(), Some(ChatId(total)));
    }

    /// A chat that finishes, fails, or turns out to be unreachable has its
    /// admission dropped with it — the crawl paths call `remove` and it must
    /// clear both halves of the ledger, or a completed chat would be re-offered
    /// as foreground work on every plan.
    #[test]
    fn removing_a_chat_clears_its_unspent_admission_too() {
        let mut demand = ContentDemandState::default();
        demand.set(200, ChatHistoryPriority::Visible);
        demand.set(200, ChatHistoryPriority::Background);
        demand.remove(200);
        let plan = demand.scheduling_snapshot();
        assert_eq!((plan.visible, plan.requested), (Vec::new(), Vec::new()));
    }

    /// Opening a chat's turn is what rotates the backlog, so the next plan
    /// offers a different chat even if the first one is the loudest
    /// (BUG-260728-2qfzbd).
    ///
    /// Without this, the scheduler re-offered whichever chat the ordering
    /// key put first, and the key it used — `last_sync_at_ms` — was stamped
    /// by live delivery rather than by history work.
    #[test]
    fn taking_a_history_turn_rotates_the_backlog_to_the_next_chat() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        add_main_chat(&mut store, 200, false);
        initialize_content_progress(&mut store, scope()).expect("progress");
        let scheduler = BackfillScheduler::with_defaults();
        let demand = Mutex::new(ContentDemandState::default());
        let open = |store: &mut StateStore, at_ms: i64| -> ChatId {
            open_next_history_turn(store, scope(), &scheduler, &demand, at_ms)
                .expect("plan")
                .expect("background work is available")
                .0
        };

        let first = open(&mut store, 1_000);
        let second = open(&mut store, 2_000);
        assert_ne!(
            first, second,
            "a chat that has just been given a turn must not be offered again \
             while another chat is still waiting for its first"
        );
        assert_eq!(
            open(&mut store, 3_000),
            first,
            "with every chat turned once, the rotation comes back round to \
             the one whose turn was longest ago"
        );
    }

    /// The installed build-127 profile carried 6,610 runnable chats while a
    /// sequential burst of twenty generated reads failed to buy even one turn
    /// for the selected crawling chat. Keep the production scheduler boundary
    /// honest at that cardinality: a read that settles (or is cancelled by its
    /// caller timeout) before the worker snapshots demand still owns one turn,
    /// live reads stay requested, and background work resumes immediately
    /// after the bounded burst rather than being disabled.
    #[test]
    fn twenty_foreground_turns_preempt_6609_backfills_then_background_resumes() {
        const ACTIVE_BACKFILLS: i64 = 6_609;
        const FOREGROUND_READS: i64 = 20;

        let mut store = store();
        let txn = store.write_txn().expect("saturated fixture transaction");
        let main = ChatListKey {
            scope: scope(),
            kind: ChatListKind::Main,
        };
        for offset in 0..ACTIVE_BACKFILLS {
            let chat_id = 10_000 + offset;
            txn.upsert_chat(
                &snapshot_chat_record(scope(), &chat(chat_id, "Synthetic backfill"))
                    .expect("synthetic chat record"),
            )
            .expect("saturated backfill chat");
            txn.upsert_chat_list_entry(
                &main,
                &ChatListEntry {
                    chat_id: ChatId(chat_id),
                    sort_order: -chat_id,
                    pinned: false,
                },
            )
            .expect("saturated backfill membership");
        }
        txn.commit().expect("commit saturated fixture");
        initialize_content_progress(&mut store, scope()).expect("saturated progress");

        let first_target = 10_000 + ACTIVE_BACKFILLS - FOREGROUND_READS;
        let targets = (first_target..first_target + FOREGROUND_READS).collect::<BTreeSet<_>>();
        let demand = Mutex::new(ContentDemandState::default());
        {
            let mut held = demand.lock().expect("foreground demand");
            for (index, target) in targets.iter().copied().enumerate() {
                held.set(target, ChatHistoryPriority::Requested);
                if index % 2 == 0 {
                    // Models the release/cancellation racing ahead of the
                    // namespace snapshot after a bounded caller timeout.
                    held.set(target, ChatHistoryPriority::Background);
                }
            }
        }

        let scheduler = BackfillScheduler::with_defaults();
        let mut served = BTreeSet::new();
        for call in 0..FOREGROUND_READS {
            let turn =
                open_next_history_turn(&mut store, scope(), &scheduler, &demand, 1_000 + call)
                    .expect("foreground plan")
                    .expect("foreground work");
            assert_eq!(turn.1, BackfillPriority::Requested);
            assert!(
                served.insert(turn.0.0),
                "one read must buy at most one turn"
            );
        }
        assert_eq!(served, targets, "all twenty selected chats get a turn");
        assert!(
            demand.lock().expect("spent demand").unspent.is_empty(),
            "twenty reads require exactly twenty foreground scheduler calls"
        );

        let background = open_next_history_turn(&mut store, scope(), &scheduler, &demand, 2_000)
            .expect("background plan")
            .expect("background remains runnable");
        assert_eq!(background.1, BackfillPriority::Background);
        assert!(
            !targets.contains(&background.0.0),
            "least-served background work must progress after the foreground burst"
        );
    }

    #[test]
    fn history_slice_crosses_pages_but_yields_to_fairness_and_foreground() {
        let mut crawl = CrawlMachine::new(CrawlPlan {
            chats: vec![ChatCrawl::new(100)],
            page_size: 2,
        })
        .expect("crawl");
        let demand = Mutex::new(ContentDemandState::default());

        for page_index in 0..MAX_BACKGROUND_HISTORY_PAGES_PER_SLICE {
            let CrawlStep::Submit(request) = crawl.next_step().expect("request") else {
                panic!("slice page must submit")
            };
            let from = request["from_message_id"].as_i64().expect("cursor");
            let newest = if from == 0 { 100 } else { from };
            crawl
                .on_response(Ok(json!({
                    "@type": "messages",
                    "messages": [
                        td_history_message_at(100, newest, 1_784_937_600),
                        td_history_message_at(100, newest - 1, 1_784_937_600),
                    ],
                })))
                .expect("page");
            assert!(matches!(
                crawl.next_step().expect("commit"),
                CrawlStep::Commit(_)
            ));
            assert_eq!(
                should_continue_history_slice(&mut crawl, 100, &demand),
                page_index + 1 < MAX_BACKGROUND_HISTORY_PAGES_PER_SLICE,
                "background slice must yield exactly at its bound"
            );
        }

        demand
            .lock()
            .expect("demand")
            .set(100, ChatHistoryPriority::Requested);
        assert!(
            should_continue_history_slice(&mut crawl, 100, &demand),
            "an explicitly opened chat continues past the background quantum"
        );
        for _ in MAX_BACKGROUND_HISTORY_PAGES_PER_SLICE..MAX_FOREGROUND_HISTORY_PAGES_PER_SLICE {
            let CrawlStep::Submit(request) = crawl.next_step().expect("foreground request") else {
                panic!("foreground slice page must submit")
            };
            let from = request["from_message_id"].as_i64().expect("cursor");
            crawl
                .on_response(Ok(json!({
                    "@type": "messages",
                    "messages": [
                        td_history_message_at(100, from, 1_784_937_600),
                        td_history_message_at(100, from - 1, 1_784_937_600),
                    ],
                })))
                .expect("foreground page");
            assert!(matches!(
                crawl.next_step().expect("foreground commit"),
                CrawlStep::Commit(_)
            ));
        }
        assert!(
            !should_continue_history_slice(&mut crawl, 100, &demand),
            "requested work must publish and yield at the foreground bound"
        );
        demand
            .lock()
            .expect("demand")
            .set(200, ChatHistoryPriority::Visible);
        assert!(
            !should_continue_history_slice(&mut crawl, 100, &demand),
            "new visible work preempts requested work at the page boundary"
        );
    }

    /// A turn granted as foreground keeps its foreground quantum for the whole
    /// turn, even though the hint that bought it was released long before the
    /// crawl started (BUG-260728-2qfzbd). Otherwise the guaranteed turn would
    /// quietly shrink to the background size and an opened chat would barely
    /// move.
    ///
    /// The ledger is deliberately not consulted here: an unspent admission for
    /// some *other* chat means a chat that is owed a turn, not a chat on
    /// screen, and letting it preempt would cut every background slice to one
    /// page while it waited.
    #[test]
    fn a_granted_foreground_turn_keeps_its_quantum_after_the_hint_is_released() {
        let mut store = store();
        add_main_chat(&mut store, 100, false);
        initialize_content_progress(&mut store, scope()).expect("progress");
        let mut crawl = crawl_for_chat(&mut store, scope(), ChatId(100), BackfillPriority::Visible)
            .expect("crawl");
        // The release already happened; the live view is empty.
        let demand = Mutex::new(ContentDemandState::default());
        demand
            .lock()
            .expect("demand")
            .set(900, ChatHistoryPriority::Visible);
        demand
            .lock()
            .expect("demand")
            .set(900, ChatHistoryPriority::Background);

        assert!(
            should_continue_history_slice(&mut crawl, 100, &demand),
            "a turn granted as visible must not run at the background quantum"
        );
        assert_eq!(
            crawl
                .progress()
                .into_iter()
                .find(|progress| progress.chat_id == 100)
                .map(|progress| progress.priority),
            Some(CrawlPriority::Visible)
        );
    }

    #[test]
    fn protected_and_cancelled_chats_have_truthful_progress() {
        let mut store = store();
        add_main_chat(&mut store, 100, true);
        add_main_chat(&mut store, 200, false);
        initialize_content_progress(&mut store, scope()).expect("progress");
        let protected = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let read = store.read_txn().expect("read");
        let progress = read
            .chat_content_progress(&protected)
            .expect("progress")
            .expect("row");
        assert_eq!(progress.phase, ChatContentPhase::Protected);
        assert_eq!(
            progress.failure_category.as_deref(),
            Some("protected-content")
        );
        assert!(!progress.retryable);
        drop(read);

        let mut coordinator = ContentCoordinator::new(&mut store, scope()).expect("coordinator");
        coordinator.active_chat = Some(200);
        coordinator.crawl = Some(
            crawl_for_chat(
                &mut store,
                scope(),
                ChatId(200),
                BackfillPriority::Background,
            )
            .expect("crawl"),
        );
        coordinator
            .mark_cancelled(&mut store, scope())
            .expect("cancel");
        let read = store.read_txn().expect("read");
        assert_eq!(
            read.chat_content_progress(&ChatKey {
                scope: scope(),
                chat_id: ChatId(200),
            })
            .expect("progress")
            .expect("row")
            .phase,
            ChatContentPhase::Cancelled
        );
    }

    // --- Correspondence dates and the folder size rollup ------------------
    //
    // BUG-260728-2qfzbd: every directory in the namespace used to publish a
    // null creation and modification date, which Finder renders as 1 Jan
    // 1970, and no size at all — so a chat folder could neither be dated nor
    // measured without opening it. Both facts are already in the index.

    /// Seeds one listed chat with July and August text plus one 4096-byte
    /// August document, projects it, and renders its months.
    fn seed_dated_chat(store: &mut StateStore, cache_root: &Path) -> ChatKey {
        const JULY_15_SECS: i64 = 1_784_116_800;
        const JULY_20_SECS: i64 = 1_784_548_800;
        const AUGUST_3_SECS: i64 = 1_785_758_400;

        let txn = store.write_txn().expect("write");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");
        rebuild_projection(store, scope()).expect("initial projection");

        let history = HistoryCommit {
            chat_id: 100,
            records: vec![
                message_at(100, 1, "july first", JULY_15_SECS),
                message_at(100, 2, "july last", JULY_20_SECS),
                image_document_at(100, 3, AUGUST_3_SECS, 900),
            ],
            window: Some(CrawlWindow {
                oldest_message_id: 1,
                newest_message_id: 3,
            }),
            history_complete: true,
            skipped_malformed: 0,
        };
        apply_history_commit(store, scope(), &history, 1_000).expect("history commit");
        render_pending_months(store, cache_root, 1_100).expect("render months");
        ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        }
    }

    fn main_appearance(item: CanonicalKey) -> ItemId {
        ItemKey::Appearance(AppearanceKey {
            view: ChatListKind::Main,
            item,
        })
        .id()
    }

    fn temp_cache_root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "gramdrive-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn directories_are_dated_from_correspondence_not_from_the_projection_clock() {
        const JULY_15_MS: i64 = 1_784_116_800_000;
        const JULY_20_MS: i64 = 1_784_548_800_000;
        const AUGUST_3_MS: i64 = 1_785_758_400_000;

        let mut store = store();
        let cache_root = temp_cache_root("dated-directories");
        let chat_key = seed_dated_chat(&mut store, &cache_root);

        let read = store.read_txn().expect("read projection");
        let chat_item = read
            .item(&main_appearance(CanonicalKey::Chat(chat_key)))
            .expect("chat item")
            .expect("chat appearance");
        assert_eq!(
            (chat_item.created_at_ms, chat_item.modified_at_ms),
            (Some(JULY_15_MS), Some(AUGUST_3_MS)),
            "a chat folder spans its first and last indexed message"
        );

        let july = read
            .item(&main_appearance(CanonicalKey::MonthDir(MonthDirKey {
                chat: chat_key,
                year: 2026,
                month: 7,
            })))
            .expect("july item")
            .expect("july appearance");
        assert_eq!(
            (july.created_at_ms, july.modified_at_ms),
            (Some(JULY_15_MS), Some(JULY_20_MS)),
            "a month folder spans only the correspondence inside it"
        );

        let august = read
            .item(&main_appearance(CanonicalKey::MonthDir(MonthDirKey {
                chat: chat_key,
                year: 2026,
                month: 8,
            })))
            .expect("august item")
            .expect("august appearance");
        assert_eq!(
            (august.created_at_ms, august.modified_at_ms),
            (Some(AUGUST_3_MS), Some(AUGUST_3_MS)),
            "a single-message month is dated by that message, not by the epoch"
        );

        let attachment = read
            .children_page(&august.id, None, 16)
            .expect("august children")
            .into_iter()
            .find(|item| {
                item.content
                    .as_ref()
                    .is_some_and(|c| c.logical_size == Some(4096))
            })
            .expect("the projected document");
        assert_eq!(
            (attachment.created_at_ms, attachment.modified_at_ms),
            (Some(AUGUST_3_MS), Some(AUGUST_3_MS)),
            "an attachment is immutable once observed, so both its dates are \
             the instant it was sent — never a null that renders as 1970"
        );
        drop(read);
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn a_chat_with_no_indexed_correspondence_is_dated_from_its_telegram_metadata() {
        // A chat the crawler has not reached yet holds no message instants,
        // but Finder still has to date its folder. Falling back to the last
        // time Telegram said anything about the chat keeps that truthful
        // instead of showing 1 Jan 1970 (BUG-260728-2qfzbd).
        let mut store = store();
        let txn = store.write_txn().expect("write");
        let mut record = snapshot_chat_record(scope(), &chat(100, "Untouched")).expect("record");
        record.last_update_at_ms = Some(1_700_000_000_000);
        txn.upsert_chat(&record).expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");
        rebuild_projection(&mut store, scope()).expect("projection");

        let chat_key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let stored = store
            .read_txn()
            .expect("read projection")
            .item(&main_appearance(CanonicalKey::Chat(chat_key)))
            .expect("chat item")
            .expect("chat appearance");
        assert_eq!(
            (stored.created_at_ms, stored.modified_at_ms),
            (Some(1_700_000_000_000), Some(1_700_000_000_000))
        );
        assert!(
            stored.created_at_ms.is_some_and(|ms| ms > 0),
            "a chat folder is never left at the epoch"
        );
        assert_eq!(
            stored.aggregate_size,
            Some(0),
            "an empty chat publishes zero bytes, which is a measurement, not an unknown"
        );
    }

    #[test]
    fn a_chat_with_no_time_at_all_falls_back_to_the_namespace_creation_time() {
        // Neither correspondence nor a Telegram metadata stamp. The folder
        // still exists and Finder still dates it, so the last truthful
        // answer available is when this namespace came into existence — the
        // same rule the fixed root children already use.
        let mut store = store();
        let txn = store.write_txn().expect("write");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Silent")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");
        rebuild_projection(&mut store, scope()).expect("projection");

        let stored = store
            .read_txn()
            .expect("read projection")
            .item(&main_appearance(CanonicalKey::Chat(ChatKey {
                scope: scope(),
                chat_id: ChatId(100),
            })))
            .expect("chat item")
            .expect("chat appearance");
        assert!(
            stored.created_at_ms.is_some_and(|ms| ms > 0)
                && stored.modified_at_ms.is_some_and(|ms| ms > 0),
            "never the epoch, even with nothing indexed: {stored:?}"
        );
    }

    #[test]
    fn a_chat_folder_publishes_the_exact_size_of_its_indexed_descendants() {
        let mut store = store();
        let cache_root = temp_cache_root("folder-rollup");
        let chat_key = seed_dated_chat(&mut store, &cache_root);

        let read = store.read_txn().expect("read projection");
        let descendant_total = |parent: &ItemId| -> u64 {
            read.children_page(parent, None, 256)
                .expect("children")
                .into_iter()
                .map(|item| match item.aggregate_size {
                    Some(size) => size,
                    None => item
                        .content
                        .as_ref()
                        .and_then(|content| content.logical_size)
                        .unwrap_or_default(),
                })
                .sum()
        };

        let chat_item = main_appearance(CanonicalKey::Chat(chat_key));
        let chat = read
            .item(&chat_item)
            .expect("chat item")
            .expect("chat appearance");
        let rollup = chat.aggregate_size.expect("a chat folder publishes a size");
        assert_eq!(
            rollup,
            descendant_total(&chat_item),
            "the published size equals what the index actually holds below it"
        );
        assert!(
            rollup >= 4096,
            "the rollup includes the 4096-byte document, so it is a real \
             measurement and not a placeholder: got {rollup}"
        );

        for month in [7u8, 8] {
            let month_item = main_appearance(CanonicalKey::MonthDir(MonthDirKey {
                chat: chat_key,
                year: 2026,
                month,
            }));
            let stored = read
                .item(&month_item)
                .expect("month item")
                .expect("month appearance");
            assert_eq!(
                stored.aggregate_size,
                Some(descendant_total(&month_item)),
                "month {month} sums its own children exactly"
            );
        }
        drop(read);
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn a_directory_that_owns_no_rollup_publishes_none_rather_than_a_false_zero() {
        // A chat list, a folder catalog and the account root hold chats, not
        // correspondence: no product surface asks for their size, and the
        // v16 backfill deliberately leaves their column NULL. The projection
        // has to agree — publishing `Some(0)` there claims "this subtree is
        // empty" for a subtree holding every byte in the account, and makes
        // the two owners of the column disagree about the same row
        // (BUG-260728-2qfzbd).
        let mut store = store();
        let cache_root = temp_cache_root("no-rollup-kinds");
        let chat_key = seed_dated_chat(&mut store, &cache_root);
        // A *custom folder* chat list is the node that actually reaches the
        // projection's no-facts fallback: the fixed root children are
        // written by account setup, while a folder view is reconciled under
        // the catalog like any other directory. Without one, this test would
        // pass whatever the fallback publishes.
        let txn = store.write_txn().expect("write");
        txn.replace_folders(
            scope(),
            &[FolderRecord {
                scope: scope(),
                folder_id: gramdrive_model::identity::FolderId(90),
                title: "Work".to_owned(),
                position: 0,
            }],
        )
        .expect("folders");
        txn.commit().expect("commit folders");
        rebuild_projection(&mut store, scope()).expect("reproject with a folder view");

        let read = store.read_txn().expect("read projection");
        let chat = read
            .item(&main_appearance(CanonicalKey::Chat(chat_key)))
            .expect("chat item")
            .expect("chat appearance");
        let chat_rollup = chat
            .aggregate_size
            .expect("a chat folder still publishes its rollup");
        assert!(
            chat_rollup > 0,
            "the fixture indexes real bytes, so the contrast below is \
             meaningful: got {chat_rollup}"
        );

        // The folder view is reconciled by the projection, so it is the one
        // whose dates this pass owns; the fixed root children and the
        // account root are written by account setup, and only their rollup
        // is this test's business.
        let folder_view = ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
            scope: scope(),
            kind: ChatListKind::Folder(gramdrive_model::identity::FolderId(90)),
        }))
        .id();
        for (label, id, projection_dated) in [
            ("a custom folder view", folder_view.clone(), true),
            (
                "the account root",
                ItemKey::Canonical(CanonicalKey::Account(scope().account)).id(),
                false,
            ),
            (
                "the Main chat list",
                ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                    scope: scope(),
                    kind: ChatListKind::Main,
                }))
                .id(),
                false,
            ),
            (
                "the folder catalog",
                ItemKey::Canonical(CanonicalKey::FolderCatalog(
                    gramdrive_model::identity::FolderCatalogKey { scope: scope() },
                ))
                .id(),
                false,
            ),
        ] {
            let stored = read
                .item(&id)
                .expect("directory item")
                .unwrap_or_else(|| panic!("{label} is projected"));
            assert!(
                stored.content.is_none(),
                "{label} is a directory, so it carries no file content facts: \
                 {stored:?}"
            );
            assert_eq!(
                stored.aggregate_size, None,
                "{label} owns no rollup and must claim none — a published \
                 0 would read as an empty subtree"
            );
            // Dates are a separate fact and stay required: the whole point
            // of this bug is that no live directory reports the epoch.
            // Dropping the rollup must not have dropped them too.
            if projection_dated {
                assert!(
                    stored.created_at_ms.is_some_and(|ms| ms > 0)
                        && stored.modified_at_ms.is_some_and(|ms| ms > 0),
                    "{label} is still dated from the namespace's own \
                     creation: {stored:?}"
                );
            }
        }
        drop(read);
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn publishing_a_document_updates_its_folder_sizes_without_reprojecting_the_chat() {
        // A full chat reconciliation re-reads every message instant and
        // every attachment projection in the chat. Running one after each
        // publication measured over 80% sustained agent CPU on a real
        // profile, so the rollup refresh is targeted instead: bounded by
        // tree depth and sibling count (BUG-260728-2qfzbd).
        const JULY_15_SECS: i64 = 1_784_116_800;

        let mut store = store();
        let cache_root = temp_cache_root("publication-rollup");
        let txn = store.write_txn().expect("write");
        txn.upsert_chat(&snapshot_chat_record(scope(), &chat(100, "Chat")).expect("record"))
            .expect("chat");
        txn.replace_chat_list(
            &ChatListKey {
                scope: scope(),
                kind: ChatListKind::Main,
            },
            &[ChatListEntry {
                chat_id: ChatId(100),
                sort_order: 10,
                pinned: false,
            }],
        )
        .expect("main membership");
        txn.commit().expect("commit metadata");
        rebuild_projection(&mut store, scope()).expect("initial projection");

        let chat_key = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        let history = HistoryCommit {
            chat_id: 100,
            records: vec![message_at(100, 1, "july", JULY_15_SECS)],
            window: Some(CrawlWindow {
                oldest_message_id: 1,
                newest_message_id: 1,
            }),
            history_complete: true,
            skipped_malformed: 0,
        };
        apply_history_commit(&mut store, scope(), &history, 1_000).expect("history commit");
        // Project the month, but do not render it: the two generated
        // documents exist with no bytes and therefore no size.
        rebuild_projection(&mut store, scope()).expect("month projection");

        let month_item = main_appearance(CanonicalKey::MonthDir(MonthDirKey {
            chat: chat_key,
            year: 2026,
            month: 7,
        }));
        let rollup = |store: &mut StateStore, item: &ItemId| -> Option<u64> {
            store
                .read_txn()
                .expect("read rollup")
                .item(item)
                .expect("item")
                .expect("directory")
                .aggregate_size
        };
        assert_eq!(
            rollup(&mut store, &month_item),
            Some(0),
            "an unrendered month holds no bytes yet"
        );

        render_pending_months(&mut store, &cache_root, 1_100).expect("render months");

        let published = rollup(&mut store, &month_item).expect("month rollup");
        assert!(
            published > 0,
            "publication alone refreshes the month's published size"
        );
        let chat_item = main_appearance(CanonicalKey::Chat(chat_key));
        let chat_rollup = rollup(&mut store, &chat_item).expect("chat rollup");
        assert!(
            chat_rollup >= published,
            "the chat's size includes its months: {chat_rollup} < {published}"
        );

        // And the version the targeted refresh stamped is exactly the one a
        // full reconciliation would compute, so neither owner undoes the
        // other.
        let before = store
            .read_txn()
            .expect("read version")
            .item(&month_item)
            .expect("item")
            .expect("month")
            .metadata_version;
        rebuild_projection(&mut store, scope()).expect("reconciliation after publication");
        assert_eq!(
            store
                .read_txn()
                .expect("read version")
                .item(&month_item)
                .expect("item")
                .expect("month")
                .metadata_version,
            before,
            "one derivation, two owners: the tokens must be identical"
        );
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn reprojecting_the_same_source_state_changes_no_date_size_or_identity() {
        let mut store = store();
        let cache_root = temp_cache_root("rollup-idempotence");
        let chat_key = seed_dated_chat(&mut store, &cache_root);

        /// One durable row's provider-visible facts: id, created, modified,
        /// rollup, metadata version.
        type ProjectedFacts = (String, Option<i64>, Option<i64>, Option<u64>, String);
        let snapshot = |store: &mut StateStore| -> Vec<ProjectedFacts> {
            let read = store.read_txn().expect("read snapshot");
            let mut rows = Vec::new();
            let mut frontier = vec![main_appearance(CanonicalKey::Chat(chat_key))];
            while let Some(parent) = frontier.pop() {
                for item in read.children_page(&parent, None, 256).expect("children") {
                    frontier.push(item.id.clone());
                    rows.push((
                        item.id.text(),
                        item.created_at_ms,
                        item.modified_at_ms,
                        item.aggregate_size,
                        item.metadata_version.as_str().to_owned(),
                    ));
                }
            }
            rows.sort();
            rows
        };

        let before = snapshot(&mut store);
        let anchor = store
            .read_txn()
            .expect("read journal")
            .change_journal_state()
            .expect("journal")
            .latest_sequence;

        rebuild_projection(&mut store, scope()).expect("full reprojection");
        rebuild_chat_projection(&mut store, chat_key).expect("chat-scoped reprojection");
        render_pending_months(&mut store, &cache_root, 2_100).expect("idle render");

        assert_eq!(
            snapshot(&mut store),
            before,
            "reconciling unchanged source state twice is a no-op down to the \
             identifier, the dates, the rollup, and the metadata version"
        );
        assert!(
            store
                .read_txn()
                .expect("read changes")
                .item_changes_since(scope().account, anchor, 100)
                .expect("changes")
                .is_empty(),
            "an idempotent pass must not wake the provider"
        );
        let _ = std::fs::remove_dir_all(&cache_root);
    }

    #[test]
    fn a_namespace_projected_before_this_version_gains_dates_and_sizes_in_place() {
        let mut store = store();
        let cache_root = temp_cache_root("legacy-namespace-migration");
        let chat_key = seed_dated_chat(&mut store, &cache_root);

        // Rewind the durable rows to exactly the shape a namespace projected
        // by an older build has: directories with no dates and no rollup.
        // Identifiers, names, parents, and content stay untouched.
        let durable_names = |store: &StateStore| -> Vec<(String, String)> {
            store
                .connection()
                .prepare("SELECT hex(item_id), safe_name FROM items ORDER BY item_id")
                .expect("prepare")
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("query")
                .collect::<Result<_, _>>()
                .expect("rows")
        };
        store
            .connection()
            .execute(
                "UPDATE items
                 SET created_at_ms = NULL, modified_at_ms = NULL,
                     aggregate_size = NULL
                 WHERE is_directory = 1",
                [],
            )
            .expect("clear directory facts");
        let legacy = durable_names(&store);

        let chat_item = main_appearance(CanonicalKey::Chat(chat_key));
        assert!(
            store
                .read_txn()
                .expect("read legacy")
                .item(&chat_item)
                .expect("chat item")
                .expect("chat appearance")
                .created_at_ms
                .is_none(),
            "precondition: the legacy shape really has no date"
        );

        rebuild_projection(&mut store, scope()).expect("migrating reconciliation");

        let read = store.read_txn().expect("read migrated");
        let chat = read
            .item(&chat_item)
            .expect("chat item")
            .expect("chat appearance");
        assert!(
            chat.created_at_ms.is_some_and(|ms| ms > 0)
                && chat.modified_at_ms.is_some_and(|ms| ms > 0),
            "one ordinary reconciliation pass is the migration: the folder is \
             dated from correspondence that was already indexed"
        );
        assert!(
            chat.aggregate_size.is_some_and(|size| size >= 4096),
            "and it regains its exact descendant size"
        );

        drop(read);
        assert_eq!(
            durable_names(&store),
            legacy,
            "no item identifier and no on-disk name changes: an installed \
             domain keeps every path it already handed out"
        );
        let _ = std::fs::remove_dir_all(&cache_root);
    }
}
