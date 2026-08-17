//! Shared durable state across host processes (TASK-260715-gnsa2s).
//!
//! On Apple platforms the app, the companion agent, and the File Provider
//! extension are separate processes coordinating through one SQLite
//! database in a shared container (`.spec/architecture.md`; PLAT-MAC-003).
//! This module is that coordination surface at the FFI boundary:
//!
//! - [`shared_state_layout`] fixes *where* the shared files live under a
//!   host-chosen data root, so every process derives identical paths from
//!   the same rule instead of repeating strings;
//! - [`SharedStateStore`] opens the database the one supported way (WAL,
//!   busy timeout, schema ensured) and exposes the narrow snapshot reads a
//!   provider extension needs — each call one short read transaction;
//! - [`SharedStateStore::data_version`] is the cross-process change probe
//!   that pairs with the host's change doorbell (on Apple, a Darwin
//!   notification posted by the writer's host process);
//! - [`quarantine_corrupt_state`] is corruption recovery, restricted to
//!   the [`StateRole::Coordinator`] by contract.
//!
//! # Roles
//!
//! Every process opens with a [`StateRole`]. Both roles may create a
//! missing database and migrate an older one forward — migrations are
//! short, resumable, and serialized by the database's own write lock, so
//! whichever process arrives first finishes the job (SYNC-072). The roles
//! differ in exactly one right: destroying files. Only the coordinator —
//! the process hosting the engine, one per container by product shape —
//! may quarantine a corrupt database. Two processes "recovering" the same
//! file concurrently could quarantine each other's fresh work, so the
//! provider reports [`DriveError::Storage`] and waits for the coordinator.
//!
//! # Writes
//!
//! Provider processes remain read-only. The one exported write is
//! [`SharedStateStore::ensure_root_structure`], restricted to the coordinator:
//! a bounded, idempotent startup repair for fixed virtual roots that contain
//! no source data. All Telegram-derived durable state is still written by the
//! engine inside its host process; the extension cannot mutate it (DEC-006
//! keeps the extension thin).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use gramdrive_model::identity::{
    AccountId, AccountKey, AccountScope, CanonicalKey, ChatListKey, ChatListKind, FolderCatalogKey,
    ItemId, ItemKey,
};
use gramdrive_model::version::MetadataVersion;
use gramdrive_state::repo::{ItemAvailability as StoredItemAvailability, ItemRecord, item_kind};
use gramdrive_state::{StateError, StateStore};

use crate::api::DriveError;

/// Hard provider-facing cap for one child page (NFR-021).
const MAX_CHILD_PAGE_SIZE: u32 = 256;

/// Which process is opening the shared state, in terms of the one right
/// that differs: recovery. See the module docs (§ Roles).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StateRole {
    /// The engine-hosting process (app or companion agent). May quarantine
    /// a corrupt database via [`quarantine_corrupt_state`]. One per shared
    /// container by product shape.
    Coordinator,
    /// A reading process (File Provider extension, UI surfaces). Never
    /// destroys shared files; on corruption it reports and waits for the
    /// coordinator.
    Provider,
}

/// Where the shared durable state lives under a host-chosen data root.
///
/// The host resolves the *root* (on Apple: a fixed subdirectory of the App
/// Group container — the Swift support package owns that rule); this
/// record fixes everything below it. All fields are absolute paths as
/// plain strings (boundary rule: no OS path types).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SharedStateLayout {
    /// The data root the layout was derived from, verbatim.
    pub data_root: String,
    /// Directory of the state database and its sidecars: `<root>/state`.
    pub state_dir: String,
    /// The SQLite database file: `<root>/state/gramdrive.sqlite3`. The
    /// `-wal`/`-shm` sidecars live beside it and belong to SQLite.
    pub database_file: String,
    /// Where corrupt databases are preserved by recovery:
    /// `<root>/state/quarantine`.
    pub quarantine_dir: String,
    /// Root of the managed content cache: `<root>/cache`. Owned by the
    /// engine's quota and eviction accounting — deliberately *not* an OS
    /// "Caches" location the system may purge behind the accounting's
    /// back.
    pub cache_dir: String,
}

/// Derives the canonical [`SharedStateLayout`] for a data root.
///
/// Pure path derivation: creates nothing, checks nothing on disk. Fails
/// with [`DriveError::InvalidArgument`] when `data_root` is empty.
#[uniffi::export]
pub fn shared_state_layout(data_root: String) -> Result<SharedStateLayout, DriveError> {
    if data_root.is_empty() {
        return Err(DriveError::InvalidArgument {
            detail: "data_root must be a non-empty directory path".to_owned(),
        });
    }
    let root = Path::new(&data_root);
    let state_dir = root.join("state");
    Ok(SharedStateLayout {
        database_file: path_string(state_dir.join("gramdrive.sqlite3"))?,
        quarantine_dir: path_string(state_dir.join("quarantine"))?,
        cache_dir: path_string(root.join("cache"))?,
        state_dir: path_string(state_dir)?,
        data_root,
    })
}

/// Which source implementation serves an account. Mirrors the state
/// store's account vocabulary (domain-model § Account).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SourceKind {
    /// A local TDLib session.
    LocalTdlib,
    /// A remote HTTP drive service.
    RemoteHttp,
}

/// One configured account, as a provider host needs it (domain-model
/// § Account): identity for a stable File Provider domain, display name
/// for the user-visible drive, and the account root's item identifier so
/// the host can start reading the tree without knowing the identifier
/// derivation scheme. Never carries secret material — the secure-storage
/// reference stays on the engine's side of the boundary.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccountInfo {
    /// The account's stable numeric identity (DOM-021).
    pub account_id: i64,
    /// Which source implementation serves it.
    pub source_kind: SourceKind,
    /// Display name of the account (its root directory's name).
    pub display_name: String,
    /// Source-defined authorization state text (never secret material).
    pub auth_state: String,
    /// Current identity-namespace epoch (DOM-021). Item identities change
    /// across a bump; the account identity — and thus a domain derived
    /// from it — does not.
    pub namespace_version: u32,
    /// Persisted IANA timezone used for display names and civil partitions.
    /// Telegram source timestamps remain absolute UTC milliseconds.
    pub display_timezone: String,
    /// The account root directory's item identifier (text form) — equals
    /// [`ItemMetadata::id`] of the root [`ItemKind::Account`] item.
    pub root_item_id: String,
}

/// What kind of drive node an item is. Mirrors the state store's provider
/// projection vocabulary (DOM-001); directories and files are disjoint by
/// construction — see [`ItemMetadata::is_directory`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ItemKind {
    /// The account root directory.
    Account,
    /// A chat-list view root (Main, Archive, or a custom folder).
    ChatList,
    /// The fixed directory grouping the custom-folder views.
    FolderCatalog,
    /// A chat directory.
    Chat,
    /// The fixed `Active Stories` directory.
    ActiveStories,
    /// A direct `YYYY-MM` timeline directory.
    MonthDir,
    /// A calendar-year directory of a chat's export.
    YearDir,
    /// The media directory of one chat-export year.
    MediaDir,
    /// A downloadable attachment file.
    Attachment,
    /// Canonical story bytes (normally hidden behind appearances).
    CanonicalStory,
    /// Active or persistent appearance of canonical story bytes.
    StoryAppearance,
    /// A generated NDJSON/Markdown/JSON document.
    GeneratedDoc,
    /// The `order.json` ordering-metadata document of a list root.
    OrderDoc,
}

/// Content availability of an item (POL-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ItemAvailability {
    /// Bytes can be fetched.
    Fetchable,
    /// The source restricts the content; bytes are never fetched.
    Restricted,
    /// The content is gone at the source.
    Unavailable,
}

/// Where an item's durable offline pin came from (POL-2). A read-only
/// provider host maps either variant to the same "keep this available
/// offline" intent — quota-exempt and never evicted by policy (SYNC-051);
/// the origin exists only because the two release independently (POL-2,
/// SYNC-062), which is the engine's concern, not a foreign reader's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PinOrigin {
    /// An explicit user "available offline" pin.
    User,
    /// Archive-Mode coverage — the per-scope pin-all opt-in (POL-2). The
    /// engine folds coverage onto items as it backfills the scope, so a
    /// provider reading these is reading exactly the paced state.
    ArchiveMode,
}

/// One provider-visible node's metadata, as durably stored (DOM-001).
///
/// Identifiers are the stable text form of the item's [`ItemId`] (DOM-024)
/// — opaque to hosts, stable across processes and restarts. Version tokens
/// are opaque text compared for equality only (DOM-003).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ItemMetadata {
    /// Version of this provider metadata record vocabulary.
    pub contract_version: u16,
    /// Stable item identifier (text form).
    pub id: String,
    /// Parent identifier; `None` exactly for the account root.
    pub parent: Option<String>,
    /// What kind of node this is.
    pub kind: ItemKind,
    /// Whether the node is a directory — a function of `kind`, restated so
    /// hosts need no kind table.
    pub is_directory: bool,
    /// Name for display.
    pub display_name: String,
    /// Filesystem-safe name, unique among live siblings (SYNC-012).
    pub safe_name: String,
    /// Opaque token versioning provider-visible metadata (DOM-003).
    pub metadata_version: String,
    /// MIME type, when known. Files only.
    pub mime_type: Option<String>,
    /// Logical size in bytes, when known. Files only.
    pub logical_size: Option<u64>,
    /// Exact logical size of the item's indexed descendants, in bytes.
    /// Directories only, and `None` until a reconciliation pass has rolled
    /// it up (BUG-260728-2qfzbd).
    ///
    /// A host publishes this as the folder's size so the system can answer
    /// "how big is this chat?" from durable metadata alone — before the
    /// folder is enumerated and without fetching a single content byte. It
    /// sums the *known* descendant sizes: an item whose exact size the
    /// source has not reported yet contributes nothing rather than an
    /// estimate (SYNC-032). Defaulted so adding it stays an additive
    /// contract change for foreign construction sites.
    #[uniffi(default = None)]
    pub aggregate_size: Option<u64>,
    /// Attachment logical kind, independent of Telegram representation.
    pub attachment_logical_kind: Option<String>,
    /// Telegram representation token (`original_document`, `message_photo`, ...).
    pub attachment_representation: Option<String>,
    /// Fidelity token (`original`, `telegram_variant`, ...).
    pub attachment_fidelity: Option<String>,
    /// Sender-provided source name, only when the representation exposes one.
    pub attachment_source_name: Option<String>,
    /// Exact source-reported attachment size.
    pub attachment_exact_size: Option<u64>,
    /// Opaque token versioning the bytes (DOM-003). Files only, and absent
    /// before first content is known.
    pub content_version: Option<String>,
    /// Content availability (POL-4).
    pub availability: ItemAvailability,
    /// Creation time in ms since the Unix epoch, when known.
    pub created_at_ms: Option<i64>,
    /// Last modification time in ms since the Unix epoch, when known.
    pub modified_at_ms: Option<i64>,
    /// POL-3 tombstone: when the node's deletion was observed. A `Some`
    /// here means the node is no longer live.
    pub deleted_at_ms: Option<i64>,
    /// Durable offline-pin state (POL-2), or `None` when no pin covers the
    /// item. A provider host maps `Some(_)` to "keep available offline"
    /// (eager, never evicted) and `None` to the evictable placeholder
    /// default (SYNC-052); the origin only says who may release it. Read
    /// from the durable `pins` table, so it survives restarts and precedes
    /// materialization — a pin set before hydration still protects the
    /// eventual bytes. Defaulted so adding it stays an additive contract
    /// change for foreign construction sites.
    #[uniffi(default = None)]
    pub pin: Option<PinOrigin>,
    /// Telegram chat owning this node, when the node is a chat appearance or
    /// lives below one. Hosts use this identity only for best-effort
    /// scheduling hints; the opaque item id remains the filesystem identity.
    #[uniffi(default = None)]
    pub chat_id: Option<i64>,
}

/// One provider-visible item change: the item's current state under the
/// journal sequence of its latest change (TASK-260715-rhcnhc). A change
/// whose `metadata` carries a POL-3 tombstone (`deleted_at_ms` set) is a
/// deletion; everything else is a create-or-update — change enumeration
/// replays current state, not history.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ItemChange {
    /// The change's journal sequence — strictly increasing across one
    /// journal life, never reused. Anchors page by it.
    pub sequence: i64,
    /// The item's current durable metadata, tombstone included.
    pub metadata: ItemMetadata,
}

/// One bounded page of live children from shared durable state.
///
/// `next_after` is an opaque item identifier to pass back to
/// [`SharedStateStore::children_page`] for the same `parent`. It is present
/// exactly when another page may exist. The store validates that a supplied
/// anchor belongs to that parent (tombstones remain valid anchors), so a
/// cursor replayed against another container fails instead of silently
/// skipping children.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ItemPage {
    /// Live children in stable identifier order.
    pub items: Vec<ItemMetadata>,
    /// Last delivered identifier when another page may exist.
    pub next_after: Option<String>,
}

/// The change journal's identity and high-water mark, read in one snapshot
/// — what a provider host mints durable sync anchors from.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChangeJournalState {
    /// Names this journal's sequence space. A persisted anchor carrying a
    /// different value is from another database life (corruption recovery
    /// starts sequences over) and must be treated as expired, never
    /// compared.
    pub instance_id: String,
    /// The highest sequence the journal has ever issued; 0 before the
    /// first recorded change. Monotonic across coalescing and cascades.
    pub latest_sequence: i64,
}

/// Readiness of the fixed, local-only first page for authorized accounts.
///
/// The four nodes per account (Chats, Archive, Stories, Folders) are virtual
/// structure, not Telegram history. Creating them is therefore bounded and
/// gives File Provider a useful first page before any source pagination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct RootStructureReadiness {
    /// Authorized accounts found in durable state.
    pub authorized_account_count: u32,
    /// Authorized accounts whose fixed first page is present.
    pub ready_account_count: u32,
    /// Fixed first-page items present across those accounts (four each).
    pub top_level_item_count: u32,
}

/// One identity-free provider callback result sent through the agent control
/// boundary. Item tokens are log-only and deliberately never cross into the
/// durable state.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ProviderFetchHealthObservation {
    /// The callback returned verified content.
    pub succeeded: bool,
    /// Hydration engine or transport failed.
    pub engine_failure: bool,
    /// An error was mapped onto the File Provider surface.
    pub provider_mapping: bool,
    /// The mapping asserted `noSuchItem`.
    pub no_such_item: bool,
    /// The system may retry this result.
    pub retryable: bool,
    /// Observation time for aggregate freshness only.
    pub observed_at_ms: i64,
}

/// Durable aggregate health for File Provider fetch callbacks.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ProviderFetchHealthCounters {
    /// Total callbacks recorded.
    pub callback_count: u64,
    /// Verified content returns.
    pub success_count: u64,
    /// Hydration engine or transport failures.
    pub engine_failure_count: u64,
    /// Provider error mappings.
    pub provider_mapping_count: u64,
    /// Provider mappings that asserted `noSuchItem`.
    pub no_such_item_count: u64,
    /// Retryable callback results.
    pub retryable_count: u64,
}

/// One process's handle on the shared durable state (module docs).
///
/// Every read runs as its own short WAL snapshot transaction: consistent
/// for the duration of the call, never blocking the writing process, never
/// holding locks between calls. Calls are synchronous and touch the disk —
/// hosts call from a background queue, not a UI thread. The handle is
/// internally locked; sharing one across threads serializes its calls, and
/// opening several handles over the same container is equally valid.
#[derive(Debug, uniffi::Object)]
pub struct SharedStateStore {
    store: Mutex<StateStore>,
    role: StateRole,
    layout: SharedStateLayout,
}

#[uniffi::export]
impl SharedStateStore {
    /// Opens (creating if absent) the shared state under `data_root`, per
    /// the canonical [`shared_state_layout`].
    ///
    /// Creates the layout directories, opens the database in WAL mode, and
    /// ensures the schema — creating it, or migrating an older file
    /// forward (either role; module docs § Roles). Fails with
    /// [`DriveError::Storage`] when the file refuses WAL, carries a newer
    /// schema than this build, or is corrupt — for the corrupt case the
    /// coordinator's recovery path is [`quarantine_corrupt_state`].
    #[uniffi::constructor]
    pub fn open(data_root: String, role: StateRole) -> Result<Arc<Self>, DriveError> {
        let layout = shared_state_layout(data_root)?;
        for dir in [&layout.state_dir, &layout.cache_dir] {
            std::fs::create_dir_all(dir).map_err(|error| DriveError::Storage {
                detail: format!("cannot create shared state directory '{dir}': {error}"),
            })?;
        }
        let store = StateStore::open(&layout.database_file).map_err(map_state_error)?;
        Ok(Arc::new(Self {
            store: Mutex::new(store),
            role,
            layout,
        }))
    }

    /// The layout this store was opened under.
    pub fn layout(&self) -> SharedStateLayout {
        self.layout.clone()
    }

    /// The role this store was opened with.
    pub fn role(&self) -> StateRole {
        self.role
    }

    /// One item's durable metadata by its stable identifier, or `None` if
    /// no such item exists (a normal answer, not an error — the item may
    /// belong to a snapshot this process has not observed yet).
    pub fn item(&self, id: String) -> Result<Option<ItemMetadata>, DriveError> {
        let id = parse_item_id(&id)?;
        let mut store = self.store()?;
        let txn = store.read_txn().map_err(map_state_error)?;
        let Some(record) = txn.item(&id).map_err(map_state_error)? else {
            return Ok(None);
        };
        let pin = read_pin(&txn, &record.id)?;
        Ok(Some(item_metadata(&txn, record, pin)?))
    }

    /// Records one aggregate File Provider callback result. Only the agent
    /// coordinator may write it: extension callbacks send this through the
    /// bounded local control endpoint, preserving DEC-006's single-writer
    /// ownership of durable state.
    pub fn record_provider_fetch_health(
        &self,
        observation: ProviderFetchHealthObservation,
    ) -> Result<(), DriveError> {
        if self.role != StateRole::Coordinator {
            return Err(DriveError::InvalidArgument {
                detail: "only the coordinator may record provider health".to_owned(),
            });
        }
        let mut store = self.store()?;
        let txn = store.write_txn().map_err(map_state_error)?;
        txn.record_provider_fetch_health(gramdrive_state::repo::ProviderFetchHealthObservation {
            succeeded: observation.succeeded,
            engine_failure: observation.engine_failure,
            provider_mapping: observation.provider_mapping,
            no_such_item: observation.no_such_item,
            retryable: observation.retryable,
            observed_at_ms: observation.observed_at_ms,
        })
        .map_err(map_state_error)?;
        txn.commit().map_err(map_state_error)
    }

    /// Reads the aggregate provider callback counters without exposing any
    /// callback identity or diagnostic text.
    pub fn provider_fetch_health(&self) -> Result<ProviderFetchHealthCounters, DriveError> {
        let mut store = self.store()?;
        let txn = store.read_txn().map_err(map_state_error)?;
        let counters = txn.provider_fetch_health().map_err(map_state_error)?;
        Ok(ProviderFetchHealthCounters {
            callback_count: counters.callback_count,
            success_count: counters.success_count,
            engine_failure_count: counters.engine_failure_count,
            provider_mapping_count: counters.provider_mapping_count,
            no_such_item_count: counters.no_such_item_count,
            retryable_count: counters.retryable_count,
        })
    }

    /// One page of a directory's live children in stable identifier order,
    /// anchored after `after` (the last identifier of the previous page;
    /// `None` starts from the beginning). Pages from one unchanged
    /// database state compose into the exact child set; a concurrent
    /// writer commit between pages is visible via [`Self::data_version`]
    /// (SYNC-003 anchoring).
    pub fn children(
        &self,
        parent: String,
        after: Option<String>,
        limit: u32,
    ) -> Result<Vec<ItemMetadata>, DriveError> {
        let parent = parse_item_id(&parent)?;
        let after = after.as_deref().map(parse_item_id_str).transpose()?;
        let mut store = self.store()?;
        let txn = store.read_txn().map_err(map_state_error)?;
        let records = txn
            .children_page(&parent, after.as_ref(), limit)
            .map_err(map_state_error)?;
        records
            .into_iter()
            .map(|record| {
                let pin = read_pin(&txn, &record.id)?;
                item_metadata(&txn, record, pin)
            })
            .collect()
    }

    /// One capped page of a directory's live children plus its continuation
    /// anchor.
    ///
    /// Unlike the legacy [`Self::children`] primitive, this method owns the
    /// page boundary: it reads one extra row to distinguish a full final page
    /// from a page that really continues, caps every request at 256 records,
    /// and validates that `after` is an item of this exact `parent`. An anchor
    /// may be tombstoned after it was issued; its durable parent still makes
    /// it a valid keyset position. A missing or foreign anchor returns
    /// [`DriveError::NotFound`], which provider hosts map to their explicit
    /// page-expired recovery.
    pub fn children_page(
        &self,
        parent: String,
        after: Option<String>,
        limit: u32,
    ) -> Result<ItemPage, DriveError> {
        if limit == 0 {
            return Err(DriveError::InvalidArgument {
                detail: "child page limit must be greater than zero".to_owned(),
            });
        }
        let parent = parse_item_id(&parent)?;
        let after = after.as_deref().map(parse_item_id_str).transpose()?;
        let page_size = limit.min(MAX_CHILD_PAGE_SIZE);
        let query_size = page_size.saturating_add(1);
        let page_size = usize::try_from(page_size).map_err(|_| DriveError::Internal {
            detail: "child page size exceeds this platform's capacity".to_owned(),
        })?;
        let mut store = self.store()?;
        let txn = store.read_txn().map_err(map_state_error)?;
        if let Some(anchor) = &after {
            let valid = txn
                .item(anchor)
                .map_err(map_state_error)?
                .is_some_and(|record| record.parent.as_ref() == Some(&parent));
            if !valid {
                return Err(DriveError::NotFound {
                    detail: "child page anchor is missing or belongs to another container"
                        .to_owned(),
                });
            }
        }
        let mut records = txn
            .children_page(&parent, after.as_ref(), query_size)
            .map_err(map_state_error)?;
        let has_more = records.len() > page_size;
        if has_more {
            records.truncate(page_size);
        }
        let next_after = if has_more {
            records.last().map(|record| record.id.text())
        } else {
            None
        };
        let items = records
            .into_iter()
            .map(|record| {
                let pin = read_pin(&txn, &record.id)?;
                item_metadata(&txn, record, pin)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ItemPage { items, next_after })
    }

    /// The live child of `parent` named `safe_name`, for resolving a path
    /// one component at a time (DOM-005). `None` when no live child has
    /// that name.
    pub fn child_by_name(
        &self,
        parent: String,
        safe_name: String,
    ) -> Result<Option<ItemMetadata>, DriveError> {
        let parent = parse_item_id(&parent)?;
        let mut store = self.store()?;
        let txn = store.read_txn().map_err(map_state_error)?;
        let Some(record) = txn
            .child_by_name(&parent, &safe_name)
            .map_err(map_state_error)?
        else {
            return Ok(None);
        };
        let pin = read_pin(&txn, &record.id)?;
        Ok(Some(item_metadata(&txn, record, pin)?))
    }

    /// Every configured account in stable identity order — what a
    /// provider host maps File Provider domains from. An empty list is a
    /// normal answer (no account configured yet), not an error.
    pub fn accounts(&self) -> Result<Vec<AccountInfo>, DriveError> {
        let mut store = self.store()?;
        let txn = store.read_txn().map_err(map_state_error)?;
        let records = txn.accounts().map_err(map_state_error)?;
        Ok(records.into_iter().map(account_info).collect())
    }

    /// Ensures every authorized account has its bounded fixed first page.
    ///
    /// Coordinator-only: provider processes remain read-only. Repeated calls
    /// and relaunches are idempotent; the state repository suppresses change
    /// journal entries for byte-for-byte identical upserts.
    pub fn ensure_root_structure(&self) -> Result<RootStructureReadiness, DriveError> {
        if self.role != StateRole::Coordinator {
            return Err(DriveError::InvalidArgument {
                detail: "only the coordinator role may ensure root structure".to_owned(),
            });
        }

        let mut store = self.store()?;
        let accounts = {
            let txn = store.read_txn().map_err(map_state_error)?;
            txn.accounts().map_err(map_state_error)?
        };
        let authorized: Vec<_> = accounts
            .into_iter()
            .filter(|account| account.auth_state == "authorized")
            .collect();

        if !authorized.is_empty() {
            let txn = store.write_txn().map_err(map_state_error)?;
            for account in &authorized {
                let scope = account.scope();
                let root = ItemKey::Canonical(CanonicalKey::Account(scope.account)).id();
                upsert_fixed_root_structure(&txn, scope, root, account.created_at_ms)?;
            }
            txn.commit().map_err(map_state_error)?;
        }

        let account_count = u32::try_from(authorized.len()).map_err(|_| DriveError::Internal {
            detail: "authorized account count exceeds boundary capacity".to_owned(),
        })?;
        Ok(RootStructureReadiness {
            authorized_account_count: account_count,
            ready_account_count: account_count,
            top_level_item_count: account_count.saturating_mul(4),
        })
    }

    /// One configured account by its stable identity, or `None` if it is
    /// not configured — the point read a provider extension resolving its
    /// domain needs.
    pub fn account(&self, account_id: i64) -> Result<Option<AccountInfo>, DriveError> {
        let key = AccountKey {
            account_id: AccountId(account_id),
        };
        let mut store = self.store()?;
        let txn = store.read_txn().map_err(map_state_error)?;
        let record = txn.account(key).map_err(map_state_error)?;
        Ok(record.map(account_info))
    }

    /// The item change journal's identity and high-water mark
    /// (TASK-260715-rhcnhc): the durable half of change signaling. The
    /// doorbell plus [`Self::data_version`] answer *whether* to look;
    /// anchors minted from this state plus [`Self::item_changes_since`]
    /// answer *what changed* — and unlike `data_version`, journal
    /// sequences are stable across handles, processes, and restarts
    /// within one `instance_id`.
    pub fn change_journal_state(&self) -> Result<ChangeJournalState, DriveError> {
        let mut store = self.store()?;
        let txn = store.read_txn().map_err(map_state_error)?;
        let state = txn.change_journal_state().map_err(map_state_error)?;
        Ok(ChangeJournalState {
            instance_id: state.instance_id,
            latest_sequence: state.latest_sequence,
        })
    }

    /// One page of an account's item changes with journal sequence greater
    /// than `after_sequence`, in sequence order — each change the item's
    /// *current* state (a set `deleted_at_ms` is a deletion). A full page
    /// (`len == limit`) means more may follow, anchored after the last
    /// returned sequence; a short page is the journal's current end. The
    /// page is one read snapshot.
    pub fn item_changes_since(
        &self,
        account_id: i64,
        after_sequence: i64,
        limit: u32,
    ) -> Result<Vec<ItemChange>, DriveError> {
        let key = AccountKey {
            account_id: AccountId(account_id),
        };
        let mut store = self.store()?;
        let txn = store.read_txn().map_err(map_state_error)?;
        let records = txn
            .item_changes_since(key, after_sequence, limit)
            .map_err(map_state_error)?;
        records
            .into_iter()
            .map(|record| {
                let pin = read_pin(&txn, &record.item.id)?;
                Ok(ItemChange {
                    sequence: record.sequence,
                    metadata: item_metadata(&txn, record.item, pin)?,
                })
            })
            .collect()
    }

    /// The database's connection-relative change stamp.
    ///
    /// The value differs from one previously returned by *this handle*
    /// exactly when another connection — in this process or another — has
    /// committed in between. That makes it the cheap "anything new?" probe
    /// a host pairs with its change doorbell: on the doorbell (or a poll
    /// tick), compare against the last value seen and re-enumerate only on
    /// change. The value is meaningful only relative to earlier calls on
    /// the same handle — never persist it, never compare it across
    /// handles or processes.
    pub fn data_version(&self) -> Result<i64, DriveError> {
        self.store()?.data_version().map_err(map_state_error)
    }

    /// The schema version of the opened database, for diagnostics and
    /// health reporting (NFR-032).
    pub fn schema_version(&self) -> Result<i64, DriveError> {
        self.store()?.schema_version().map_err(map_state_error)
    }
}

/// Writes the fixed account-root children shared by fresh authorization and
/// startup repair. These nodes contain no source data and never trigger
/// message or media loading.
pub(crate) fn upsert_fixed_root_structure(
    txn: &gramdrive_state::WriteTxn<'_>,
    scope: AccountScope,
    root: ItemId,
    created_at_ms: i64,
) -> Result<(), DriveError> {
    let version =
        MetadataVersion::new("root-structure-v2").map_err(|error| DriveError::Internal {
            detail: format!("root structure metadata version: {error}"),
        })?;
    let definitions = [
        (
            ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                scope,
                kind: ChatListKind::Main,
            }))
            .id(),
            "Chats",
        ),
        (
            ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                scope,
                kind: ChatListKind::Archive,
            }))
            .id(),
            "Archive",
        ),
        (
            ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                scope,
                kind: ChatListKind::Stories,
            }))
            .id(),
            "Stories",
        ),
        (
            ItemKey::Canonical(CanonicalKey::FolderCatalog(FolderCatalogKey { scope })).id(),
            "Folders",
        ),
    ];
    for (id, name) in definitions {
        txn.upsert_item(&ItemRecord {
            id,
            parent: Some(root.clone()),
            display_name: name.to_owned(),
            safe_name: name.to_owned(),
            metadata_version: version.clone(),
            content: None,
            // The fixed root children are containers of containers; their
            // rollup is filled by the first reconciliation pass that has the
            // chat lists to sum.
            aggregate_size: None,
            availability: StoredItemAvailability::Fetchable,
            created_at_ms: Some(created_at_ms),
            modified_at_ms: Some(created_at_ms),
            deleted_at_ms: None,
        })
        .map_err(map_state_error)?;
    }
    Ok(())
}

impl SharedStateStore {
    fn store(&self) -> Result<MutexGuard<'_, StateStore>, DriveError> {
        self.store.lock().map_err(|_| DriveError::Internal {
            detail: "shared state handle poisoned by an earlier panic".to_owned(),
        })
    }
}

/// Quarantines the database under `data_root` if — and only if — SQLite
/// reports it corrupt, clearing the path for a fresh open. Returns the
/// quarantine directory when files were moved, `None` when the database is
/// healthy or absent (nothing touched).
///
/// **Coordinator only** (module docs § Roles): callers must pass the role
/// they opened with, and [`StateRole::Provider`] is refused with
/// [`DriveError::InvalidArgument`]. Detection is separate from
/// destruction: this function re-probes the file itself and declines to
/// touch a healthy database, so a misdiagnosed error cannot destroy state.
/// The damaged files are preserved (not deleted) for diagnostics; pruning
/// old quarantine directories is host policy.
#[uniffi::export]
pub fn quarantine_corrupt_state(
    data_root: String,
    role: StateRole,
) -> Result<Option<String>, DriveError> {
    if role != StateRole::Coordinator {
        return Err(DriveError::InvalidArgument {
            detail: "only the coordinator role may quarantine shared state".to_owned(),
        });
    }
    let layout = shared_state_layout(data_root)?;
    let report =
        gramdrive_state::quarantine_if_corrupt(&layout.database_file).map_err(map_state_error)?;
    match report {
        None => Ok(None),
        Some(report) => Ok(Some(path_string(report.quarantine_dir)?)),
    }
}

fn path_string(path: PathBuf) -> Result<String, DriveError> {
    path.into_os_string()
        .into_string()
        .map_err(|_| DriveError::Internal {
            detail: "derived path is not valid UTF-8".to_owned(),
        })
}

fn parse_item_id(text: &str) -> Result<ItemId, DriveError> {
    ItemId::parse_text(text).map_err(|error| DriveError::InvalidArgument {
        detail: format!("not an item identifier: {error}"),
    })
}

fn parse_item_id_str(text: &str) -> Result<ItemId, DriveError> {
    parse_item_id(text)
}

/// Reads one item's durable offline-pin state within an open snapshot, in
/// provider vocabulary. The read is a same-transaction indexed point lookup,
/// so folding it into each item projection stays a snapshot-consistent read
/// (the pin and the metadata cannot disagree across a concurrent commit).
fn read_pin(
    txn: &gramdrive_state::ReadTxn<'_>,
    id: &ItemId,
) -> Result<Option<PinOrigin>, DriveError> {
    Ok(txn
        .pin(id)
        .map_err(map_state_error)?
        .map(|record| map_pin_origin(record.origin)))
}

fn item_metadata(
    txn: &gramdrive_state::ReadTxn<'_>,
    record: ItemRecord,
    pin: Option<PinOrigin>,
) -> Result<ItemMetadata, DriveError> {
    let kind = map_kind(item_kind(&record.id).map_err(map_state_error)?);
    let chat_id = item_chat_id(&record.id);
    let (mime_type, logical_size, content_version) = match record.content {
        None => (None, None, None),
        Some(facts) => (
            facts.mime_type,
            facts.logical_size,
            facts
                .content_version
                .map(|version| version.as_str().to_owned()),
        ),
    };
    let attachment = match record.id.key() {
        ItemKey::Canonical(CanonicalKey::Attachment(key))
        | ItemKey::Appearance(gramdrive_model::identity::AppearanceKey {
            item: CanonicalKey::Attachment(key),
            ..
        }) => txn.attachment(&key).map_err(map_state_error)?,
        _ => None,
    };
    let attachment_fields = attachment.map(|state| {
        let logical_kind = match state.facts.logical_kind {
            gramdrive_state::repo::AttachmentLogicalKind::Photo => "photo".to_owned(),
            gramdrive_state::repo::AttachmentLogicalKind::Video => "video".to_owned(),
            gramdrive_state::repo::AttachmentLogicalKind::Animation => "animation".to_owned(),
            gramdrive_state::repo::AttachmentLogicalKind::Audio => "audio".to_owned(),
            gramdrive_state::repo::AttachmentLogicalKind::Voice => "voice".to_owned(),
            gramdrive_state::repo::AttachmentLogicalKind::VideoNote => "video_note".to_owned(),
            gramdrive_state::repo::AttachmentLogicalKind::Sticker => "sticker".to_owned(),
            gramdrive_state::repo::AttachmentLogicalKind::Document => "document".to_owned(),
            gramdrive_state::repo::AttachmentLogicalKind::OtherMedia => "other_media".to_owned(),
            gramdrive_state::repo::AttachmentLogicalKind::Unknown => "unknown".to_owned(),
            gramdrive_state::repo::AttachmentLogicalKind::Other(value) => value,
        };
        let representation = match state.facts.telegram_representation {
            gramdrive_state::repo::TelegramRepresentation::OriginalDocument => {
                "original_document".to_owned()
            }
            gramdrive_state::repo::TelegramRepresentation::Photo => "message_photo".to_owned(),
            gramdrive_state::repo::TelegramRepresentation::Video => "message_video".to_owned(),
            gramdrive_state::repo::TelegramRepresentation::Animation => {
                "message_animation".to_owned()
            }
            gramdrive_state::repo::TelegramRepresentation::Audio => "message_audio".to_owned(),
            gramdrive_state::repo::TelegramRepresentation::Voice => "message_voice".to_owned(),
            gramdrive_state::repo::TelegramRepresentation::VideoNote => {
                "message_video_note".to_owned()
            }
            gramdrive_state::repo::TelegramRepresentation::Sticker => "message_sticker".to_owned(),
            gramdrive_state::repo::TelegramRepresentation::UnknownLegacy => {
                "unknown_legacy".to_owned()
            }
            gramdrive_state::repo::TelegramRepresentation::Other(value) => value,
        };
        let fidelity = match state.facts.fidelity {
            gramdrive_state::repo::AttachmentFidelity::Original => "original".to_owned(),
            gramdrive_state::repo::AttachmentFidelity::TelegramVariant => {
                "telegram_variant".to_owned()
            }
            gramdrive_state::repo::AttachmentFidelity::MetadataOnly => "metadata_only".to_owned(),
            gramdrive_state::repo::AttachmentFidelity::UnknownLegacy => "unknown_legacy".to_owned(),
            gramdrive_state::repo::AttachmentFidelity::Other(value) => value,
        };
        (
            logical_kind,
            representation,
            fidelity,
            state.facts.source_name,
            state.facts.exact_size,
        )
    });
    let (
        attachment_logical_kind,
        attachment_representation,
        attachment_fidelity,
        attachment_source_name,
        attachment_exact_size,
    ) = match attachment_fields {
        Some((kind, representation, fidelity, name, size)) => {
            (Some(kind), Some(representation), Some(fidelity), name, size)
        }
        None => (None, None, None, None, None),
    };
    Ok(ItemMetadata {
        contract_version: 2,
        id: record.id.text(),
        parent: record.parent.as_ref().map(ItemId::text),
        kind,
        is_directory: kind_is_directory(kind),
        display_name: record.display_name,
        safe_name: record.safe_name,
        metadata_version: record.metadata_version.as_str().to_owned(),
        mime_type,
        logical_size,
        aggregate_size: record.aggregate_size,
        attachment_logical_kind,
        attachment_representation,
        attachment_fidelity,
        attachment_source_name,
        attachment_exact_size,
        content_version,
        availability: map_availability(record.availability),
        created_at_ms: record.created_at_ms,
        modified_at_ms: record.modified_at_ms,
        deleted_at_ms: record.deleted_at_ms,
        pin,
        chat_id,
    })
}

fn item_chat_id(id: &ItemId) -> Option<i64> {
    let canonical = match id.key() {
        ItemKey::Canonical(canonical) => canonical,
        ItemKey::Appearance(gramdrive_model::identity::AppearanceKey { item, .. }) => item,
        ItemKey::StoryAppearance(appearance) => {
            return Some(appearance.story.poster.chat_id.0);
        }
    };
    match canonical {
        CanonicalKey::Chat(chat) => Some(chat.chat_id.0),
        CanonicalKey::ActiveStories(key) => Some(key.chat.chat_id.0),
        CanonicalKey::MonthDir(key) => Some(key.chat.chat_id.0),
        CanonicalKey::Message(key) => Some(key.chat.chat_id.0),
        CanonicalKey::Attachment(key) => Some(key.message.chat.chat_id.0),
        CanonicalKey::Story(key) => Some(key.poster.chat_id.0),
        CanonicalKey::GeneratedDoc(key) => Some(key.chat.chat_id.0),
        // Legacy identities remain readable during migrations.
        CanonicalKey::YearDir(key) => Some(key.chat.chat_id.0),
        CanonicalKey::MediaDir(key) => Some(key.chat.chat_id.0),
        CanonicalKey::Account(_)
        | CanonicalKey::ChatList(_)
        | CanonicalKey::FolderCatalog(_)
        | CanonicalKey::OrderDoc(_)
        | CanonicalKey::Blob(_) => None,
    }
}

fn account_info(record: gramdrive_state::repo::AccountRecord) -> AccountInfo {
    AccountInfo {
        account_id: record.account.account_id.0,
        source_kind: map_source_kind(record.source_kind),
        root_item_id: ItemKey::Canonical(CanonicalKey::Account(record.account))
            .id()
            .text(),
        display_name: record.display_name,
        auth_state: record.auth_state,
        namespace_version: record.namespace_version.0,
        display_timezone: record.display_timezone,
    }
}

fn map_source_kind(kind: gramdrive_state::repo::SourceKind) -> SourceKind {
    use gramdrive_state::repo::SourceKind as Stored;
    match kind {
        Stored::LocalTdlib => SourceKind::LocalTdlib,
        Stored::RemoteHttp => SourceKind::RemoteHttp,
    }
}

fn map_kind(kind: gramdrive_state::repo::ItemKind) -> ItemKind {
    use gramdrive_state::repo::ItemKind as Stored;
    match kind {
        Stored::Account => ItemKind::Account,
        Stored::ChatList => ItemKind::ChatList,
        Stored::FolderCatalog => ItemKind::FolderCatalog,
        Stored::Chat => ItemKind::Chat,
        Stored::ActiveStories => ItemKind::ActiveStories,
        Stored::MonthDir => ItemKind::MonthDir,
        Stored::YearDir => ItemKind::YearDir,
        Stored::MediaDir => ItemKind::MediaDir,
        Stored::Attachment => ItemKind::Attachment,
        Stored::CanonicalStory => ItemKind::CanonicalStory,
        Stored::StoryAppearance => ItemKind::StoryAppearance,
        Stored::GeneratedDoc => ItemKind::GeneratedDoc,
        Stored::OrderDoc => ItemKind::OrderDoc,
    }
}

fn kind_is_directory(kind: ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::Account
            | ItemKind::ChatList
            | ItemKind::FolderCatalog
            | ItemKind::Chat
            | ItemKind::ActiveStories
            | ItemKind::MonthDir
            | ItemKind::YearDir
            | ItemKind::MediaDir
    )
}

fn map_availability(availability: gramdrive_state::repo::ItemAvailability) -> ItemAvailability {
    use gramdrive_state::repo::ItemAvailability as Stored;
    match availability {
        Stored::Fetchable => ItemAvailability::Fetchable,
        Stored::Restricted => ItemAvailability::Restricted,
        Stored::Unavailable => ItemAvailability::Unavailable,
    }
}

fn map_pin_origin(origin: gramdrive_state::repo::PinOrigin) -> PinOrigin {
    use gramdrive_state::repo::PinOrigin as Stored;
    match origin {
        Stored::User => PinOrigin::User,
        Stored::ArchiveMode => PinOrigin::ArchiveMode,
    }
}

/// Maps the state store's failure vocabulary onto the boundary categories
/// (NFR-030). Everything is a local-persistence failure ([`DriveError::
/// Storage`]) except the conditions with their own categories: caller
/// mistakes and missing required rows.
fn map_state_error(error: StateError) -> DriveError {
    match &error {
        StateError::InvalidArgument { what } => DriveError::InvalidArgument {
            detail: (*what).to_owned(),
        },
        StateError::RowNotFound { entity } => DriveError::NotFound {
            detail: format!("{entity} not found"),
        },
        _ => DriveError::Storage {
            detail: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    use gramdrive_model::identity::{
        AccountId, AccountKey, AccountScope, ActiveStoriesKey, AttachmentIndex, AttachmentKey,
        CanonicalKey, ChatId, ChatKey, DocFormat, DocPartition, GeneratedDocKey, ItemKey,
        MessageId, MessageKey, MonthDirKey, NamespaceVersion, SchemaFamily,
    };
    use gramdrive_model::version::{ContentVersion, MetadataVersion};
    use gramdrive_state::repo::{
        AccountRecord, AttachmentAvailability, AttachmentFacts, AttachmentFidelity,
        AttachmentLogicalKind, ChatRecord, ChatType, FileFacts, ItemRecord, MessageChange,
        MessageRevision, PinOrigin as StoredPinOrigin, RetentionMode,
        SourceKind as StoredSourceKind, TelegramRepresentation,
    };

    /// A unique data root under the OS temp dir, removed on drop.
    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "gramdrive-ffi-shared-state-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }

        fn as_str(&self) -> &str {
            self.path.to_str().expect("temp path is UTF-8")
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
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

    fn chat_dir_id() -> ItemId {
        ItemKey::Canonical(CanonicalKey::Chat(ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        }))
        .id()
    }

    fn file_id() -> ItemId {
        ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
            message: MessageKey {
                chat: ChatKey {
                    scope: scope(),
                    chat_id: ChatId(100),
                },
                message_id: MessageId(1),
            },
            index: AttachmentIndex(0),
        }))
        .id()
    }

    /// Seeds a small provider tree the way the engine host would: directly
    /// through the state crate, in-process.
    fn seed(root: &TempRoot) {
        let layout = shared_state_layout(root.as_str().to_owned()).expect("layout");
        std::fs::create_dir_all(&layout.state_dir).expect("state dir");
        let mut store = StateStore::open(&layout.database_file).expect("open");
        let txn = store.write_txn().expect("write txn");
        txn.upsert_account(&AccountRecord {
            account: scope().account,
            source_kind: StoredSourceKind::LocalTdlib,
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
        txn.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: root_id(),
            parent: None,
            display_name: "Test Account".to_owned(),
            safe_name: "Test Account".to_owned(),
            metadata_version: MetadataVersion::new("m1").expect("version"),
            content: None,
            availability: gramdrive_state::repo::ItemAvailability::Fetchable,
            created_at_ms: Some(1_000),
            modified_at_ms: Some(1_000),
            deleted_at_ms: None,
        })
        .expect("root");
        txn.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: chat_dir_id(),
            parent: Some(root_id()),
            display_name: "Chat 100".to_owned(),
            safe_name: "Chat 100".to_owned(),
            metadata_version: MetadataVersion::new("m1").expect("version"),
            content: None,
            availability: gramdrive_state::repo::ItemAvailability::Fetchable,
            created_at_ms: Some(1_000),
            modified_at_ms: Some(1_000),
            deleted_at_ms: None,
        })
        .expect("chat dir");
        txn.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: file_id(),
            parent: Some(chat_dir_id()),
            display_name: "photo.jpg".to_owned(),
            safe_name: "photo.jpg".to_owned(),
            metadata_version: MetadataVersion::new("m1").expect("version"),
            content: Some(FileFacts {
                mime_type: Some("image/jpeg".to_owned()),
                logical_size: Some(2_048),
                content_version: Some(ContentVersion::new("c1").expect("version")),
            }),
            availability: gramdrive_state::repo::ItemAvailability::Fetchable,
            created_at_ms: Some(1_000),
            modified_at_ms: Some(1_500),
            deleted_at_ms: None,
        })
        .expect("file");
        let chat = ChatKey {
            scope: scope(),
            chat_id: ChatId(100),
        };
        txn.upsert_chat(&ChatRecord {
            key: chat,
            chat_type: ChatType::Private,
            title: "Chat 100".to_owned(),
            username: None,
            is_protected: false,
            archive_mode: false,
            metadata_version: MetadataVersion::new("chat-v1").expect("version"),
            left_at_ms: None,
            deleted_at_ms: None,
            last_update_at_ms: Some(1_000),
        })
        .expect("source chat");
        txn.apply_message_changes(
            &chat,
            &[MessageChange::Observed(MessageRevision {
                message_id: MessageId(1),
                sender_id: Some(42),
                sent_at_ms: 1_000,
                edited_at_ms: None,
                observed_at_ms: 1_005,
                payload_schema: SchemaFamily(1),
                payload: vec![1],
            })],
        )
        .expect("source message");
        txn.upsert_attachment(&AttachmentFacts {
            key: AttachmentKey {
                message: MessageKey {
                    chat,
                    message_id: MessageId(1),
                },
                index: AttachmentIndex(0),
            },
            logical_kind: AttachmentLogicalKind::Photo,
            telegram_representation: TelegramRepresentation::OriginalDocument,
            fidelity: AttachmentFidelity::Original,
            source_name: Some("sender-photo.jpg".to_owned()),
            mime_type: Some("image/jpeg".to_owned()),
            exact_size: Some(2_048),
            content_version: ContentVersion::new("c1").expect("version"),
            telegram_unique_id: Some("unique-photo".to_owned()),
            telegram_local_file_id: Some(517),
            telegram_file_id: Some("refreshable-photo".to_owned()),
            file_reference: None,
            availability: AttachmentAvailability::Fetchable,
            can_be_saved: true,
        })
        .expect("source attachment");
        txn.commit().expect("commit");
    }

    #[test]
    fn layout_is_a_pure_derivation_of_the_root() {
        let layout = shared_state_layout("/container/data".to_owned()).expect("layout");
        assert_eq!(layout.data_root, "/container/data");
        assert_eq!(layout.state_dir, "/container/data/state");
        assert_eq!(
            layout.database_file,
            "/container/data/state/gramdrive.sqlite3"
        );
        assert_eq!(layout.quarantine_dir, "/container/data/state/quarantine");
        assert_eq!(layout.cache_dir, "/container/data/cache");
    }

    #[test]
    fn layout_rejects_an_empty_root() {
        let err = shared_state_layout(String::new()).expect_err("empty root");
        assert!(matches!(err, DriveError::InvalidArgument { .. }));
    }

    #[test]
    fn open_creates_layout_directories_and_an_empty_database() {
        let root = TempRoot::new();
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");
        assert_eq!(store.role(), StateRole::Provider);
        let layout = store.layout();
        assert!(Path::new(&layout.state_dir).is_dir());
        assert!(Path::new(&layout.cache_dir).is_dir());
        assert!(Path::new(&layout.database_file).is_file());
        // An empty database answers reads with absence, not errors.
        assert_eq!(store.item(root_id().text()).expect("item"), None);
        assert!(store.schema_version().expect("schema") > 0);
    }

    #[test]
    fn reads_return_the_seeded_metadata_across_a_separate_handle() {
        let root = TempRoot::new();
        seed(&root);
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");

        let account = store
            .item(root_id().text())
            .expect("item")
            .expect("account root exists");
        assert_eq!(account.kind, ItemKind::Account);
        assert!(account.is_directory);
        assert_eq!(account.parent, None);

        let children = store
            .children(root_id().text(), None, 100)
            .expect("children");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, chat_dir_id().text());
        assert_eq!(children[0].kind, ItemKind::Chat);

        let file = store
            .child_by_name(chat_dir_id().text(), "photo.jpg".to_owned())
            .expect("child_by_name")
            .expect("file exists");
        assert_eq!(file.id, file_id().text());
        assert_eq!(file.kind, ItemKind::Attachment);
        assert!(!file.is_directory);
        assert_eq!(file.mime_type.as_deref(), Some("image/jpeg"));
        assert_eq!(file.logical_size, Some(2_048));
        assert_eq!(file.content_version.as_deref(), Some("c1"));
        assert_eq!(file.availability, ItemAvailability::Fetchable);
        assert_eq!(file.contract_version, 2);
        assert_eq!(file.chat_id, Some(100));
        assert_eq!(file.attachment_logical_kind.as_deref(), Some("photo"));
        assert_eq!(
            file.attachment_representation.as_deref(),
            Some("original_document")
        );
        assert_eq!(file.attachment_fidelity.as_deref(), Some("original"));
        assert_eq!(
            file.attachment_source_name.as_deref(),
            Some("sender-photo.jpg")
        );
        assert_eq!(file.attachment_exact_size, Some(2_048));
    }

    #[test]
    fn malformed_processed_attachment_cannot_cross_the_ffi_boundary() {
        let root = TempRoot::new();
        seed(&root);
        let layout = shared_state_layout(root.as_str().to_owned()).expect("layout");
        let writer = StateStore::open(&layout.database_file).expect("open writer");
        writer
            .connection()
            .pragma_update(None, "ignore_check_constraints", true)
            .expect("test-only constraint bypass");
        writer
            .connection()
            .execute(
                "UPDATE attachments
                 SET telegram_representation = 'message_photo', fidelity = 'original',
                     source_name = 'claimed-original.jpg'",
                [],
            )
            .expect("inject corrupt historical row");
        drop(writer);

        let store = SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider)
            .expect("open provider");
        let error = store
            .item(file_id().text())
            .expect_err("corrupt claim rejected");
        assert!(matches!(error, DriveError::Storage { .. }));
        assert!(error.to_string().contains("Telegram-processed media"));
    }

    #[test]
    fn coordinator_repairs_fixed_root_structure_once_across_relaunches() {
        let root = TempRoot::new();
        seed(&root);
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Coordinator).expect("open");
        let before = store
            .change_journal_state()
            .expect("journal before")
            .latest_sequence;
        assert_eq!(
            before, 3,
            "the fixture journals its account root, chat, and attachment exactly once"
        );

        let readiness = store.ensure_root_structure().expect("ensure structure");
        assert_eq!(readiness.authorized_account_count, 1);
        assert_eq!(readiness.ready_account_count, 1);
        assert_eq!(readiness.top_level_item_count, 4);
        let children = store
            .children(root_id().text(), None, 100)
            .expect("root children");
        let names: Vec<_> = children
            .iter()
            .map(|item| item.display_name.as_str())
            .collect();
        assert_eq!(names, ["Chats", "Archive", "Stories", "Folders"]);
        assert_eq!(
            children.len(),
            4,
            "the provider root exposes exactly the fixed Telegram roots"
        );
        let after_first = store
            .change_journal_state()
            .expect("journal after first")
            .latest_sequence;
        assert_eq!(after_first, before + 4);
        let fixed_root_changes = store
            .item_changes_since(7, before, 100)
            .expect("fixed root changes");
        assert_eq!(
            fixed_root_changes
                .iter()
                .map(|change| change.sequence)
                .collect::<Vec<_>>(),
            vec![before + 1, before + 2, before + 3, before + 4],
            "each fixed root owns one durable journal sequence"
        );
        assert_eq!(
            fixed_root_changes
                .iter()
                .map(|change| change.metadata.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                    scope: scope(),
                    kind: ChatListKind::Main,
                }))
                .id()
                .text(),
                ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                    scope: scope(),
                    kind: ChatListKind::Archive,
                }))
                .id()
                .text(),
                ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                    scope: scope(),
                    kind: ChatListKind::Stories,
                }))
                .id()
                .text(),
                ItemKey::Canonical(CanonicalKey::FolderCatalog(FolderCatalogKey {
                    scope: scope(),
                }))
                .id()
                .text(),
            ],
            "sequence 7 is the real Stories-or-Folders event, not relaunch drift"
        );

        drop(store);
        let relaunched = SharedStateStore::open(root.as_str().to_owned(), StateRole::Coordinator)
            .expect("reopen");
        assert_eq!(
            relaunched.ensure_root_structure().expect("re-ensure"),
            readiness
        );
        assert_eq!(
            relaunched
                .change_journal_state()
                .expect("journal after relaunch")
                .latest_sequence,
            after_first,
            "an identical repair is journal-quiet"
        );
        assert!(
            relaunched
                .item_changes_since(7, after_first, 100)
                .expect("changes after relaunch")
                .is_empty(),
            "relaunch emits no hidden durable event"
        );
        assert_eq!(
            relaunched
                .children(root_id().text(), None, 100)
                .expect("children after relaunch")
                .len(),
            4,
            "relaunch never duplicates root items"
        );
    }

    #[test]
    fn provider_role_cannot_write_root_structure() {
        let root = TempRoot::new();
        seed(&root);
        let provider =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");
        assert!(matches!(
            provider.ensure_root_structure(),
            Err(DriveError::InvalidArgument { .. })
        ));
    }

    #[test]
    fn durable_pins_surface_on_every_provider_read_and_survive_a_reopen() {
        let root = TempRoot::new();
        seed(&root);
        // The coordinator pins the file (explicit user intent) and the chat
        // directory (Archive-Mode coverage), through the state crate exactly
        // as the engine host would — in-process, off the read-only boundary.
        let layout = shared_state_layout(root.as_str().to_owned()).expect("layout");
        let mut writer = StateStore::open(&layout.database_file).expect("open writer");
        let txn = writer.write_txn().expect("write txn");
        txn.pin_item(&file_id(), StoredPinOrigin::User, 3_000)
            .expect("pin file");
        txn.pin_item(&chat_dir_id(), StoredPinOrigin::ArchiveMode, 3_000)
            .expect("pin chat dir");
        txn.commit().expect("commit");

        // A fresh Provider handle — the "after restart" read — sees the
        // durable pins: the projection reads them from the pins table, not
        // from any in-memory carry-over.
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");

        // item(): both origins map, and an unpinned item reads `None`.
        assert_eq!(
            store
                .item(file_id().text())
                .expect("item")
                .expect("file")
                .pin,
            Some(PinOrigin::User)
        );
        assert_eq!(
            store
                .item(chat_dir_id().text())
                .expect("item")
                .expect("chat dir")
                .pin,
            Some(PinOrigin::ArchiveMode)
        );
        assert_eq!(
            store
                .item(root_id().text())
                .expect("item")
                .expect("root")
                .pin,
            None,
            "an unpinned item projects the evictable default"
        );

        // children(): each child in the page carries its own pin.
        let root_children = store
            .children(root_id().text(), None, 100)
            .expect("root children");
        assert_eq!(root_children.len(), 1);
        assert_eq!(root_children[0].pin, Some(PinOrigin::ArchiveMode));
        let chat_children = store
            .children(chat_dir_id().text(), None, 100)
            .expect("chat children");
        assert_eq!(chat_children.len(), 1);
        assert_eq!(chat_children[0].pin, Some(PinOrigin::User));

        // child_by_name(): the name resolver projects the same pin.
        let by_name = store
            .child_by_name(chat_dir_id().text(), "photo.jpg".to_owned())
            .expect("child_by_name")
            .expect("file");
        assert_eq!(by_name.pin, Some(PinOrigin::User));

        // item_changes_since(): the change feed carries the current pin, so a
        // working-set re-read reflects offline intent, not just tree shape.
        let changes = store.item_changes_since(7, 0, 100).expect("changes");
        let file_change = changes
            .iter()
            .find(|change| change.metadata.id == file_id().text())
            .expect("file change");
        assert_eq!(file_change.metadata.pin, Some(PinOrigin::User));
        let root_change = changes
            .iter()
            .find(|change| change.metadata.id == root_id().text())
            .expect("root change");
        assert_eq!(root_change.metadata.pin, None);
    }

    #[test]
    fn unpinning_drops_the_projection_back_to_the_evictable_default() {
        let root = TempRoot::new();
        seed(&root);
        let layout = shared_state_layout(root.as_str().to_owned()).expect("layout");
        let mut writer = StateStore::open(&layout.database_file).expect("open writer");
        let txn = writer.write_txn().expect("write txn");
        txn.pin_item(&file_id(), StoredPinOrigin::User, 3_000)
            .expect("pin file");
        txn.commit().expect("commit");

        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");
        assert_eq!(
            store
                .item(file_id().text())
                .expect("item")
                .expect("file")
                .pin,
            Some(PinOrigin::User)
        );

        // A foreign unpin commit is visible to the same handle (each read is
        // its own WAL snapshot), and the projection drops to `None`.
        let mut writer = StateStore::open(&layout.database_file).expect("reopen writer");
        let txn = writer.write_txn().expect("write txn");
        assert!(txn.unpin_item(&file_id()).expect("unpin"));
        txn.commit().expect("commit");
        assert_eq!(
            store
                .item(file_id().text())
                .expect("item")
                .expect("file")
                .pin,
            None
        );
    }

    #[test]
    fn accounts_are_empty_on_a_fresh_database() {
        let root = TempRoot::new();
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");
        assert_eq!(store.accounts().expect("accounts"), Vec::new());
        assert_eq!(store.account(7).expect("account"), None);
    }

    #[test]
    fn accounts_return_the_seeded_record_with_its_root_item_id() {
        let root = TempRoot::new();
        seed(&root);
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");

        let accounts = store.accounts().expect("accounts");
        assert_eq!(accounts.len(), 1);
        let account = &accounts[0];
        assert_eq!(account.account_id, 7);
        assert_eq!(account.source_kind, SourceKind::LocalTdlib);
        assert_eq!(account.display_name, "Test Account");
        assert_eq!(account.auth_state, "authorized");
        assert_eq!(account.namespace_version, 1);
        assert_eq!(account.display_timezone, "UTC");
        assert_eq!(account.root_item_id, root_id().text());
        // The advertised root resolves through the item read to the
        // account root item — the two reads can never disagree.
        let item = store
            .item(account.root_item_id.clone())
            .expect("item")
            .expect("root item exists");
        assert_eq!(item.kind, ItemKind::Account);

        assert_eq!(
            store.account(7).expect("account").as_ref(),
            Some(account),
            "the point read answers with the same record"
        );
        assert_eq!(store.account(8).expect("account"), None);
    }

    #[test]
    fn accounts_list_in_stable_identity_order() {
        let root = TempRoot::new();
        seed(&root);
        // A second account, inserted after the first but with a smaller
        // identity — the list must order by identity, not insertion.
        let layout = shared_state_layout(root.as_str().to_owned()).expect("layout");
        let mut writer = StateStore::open(&layout.database_file).expect("open writer");
        let txn = writer.write_txn().expect("write txn");
        txn.upsert_account(&AccountRecord {
            account: AccountKey {
                account_id: AccountId(3),
            },
            source_kind: StoredSourceKind::RemoteHttp,
            display_name: "Second Account".to_owned(),
            auth_state: "authorized".to_owned(),
            namespace_version: NamespaceVersion(2),
            display_timezone: "Asia/Tbilisi".to_owned(),
            retention_mode: RetentionMode::Audit,
            archive_mode: true,
            secret_ref: None,
            created_at_ms: 2_000,
            updated_at_ms: 2_000,
        })
        .expect("second account");
        txn.commit().expect("commit");

        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");
        let accounts = store.accounts().expect("accounts");
        assert_eq!(
            accounts
                .iter()
                .map(|account| account.account_id)
                .collect::<Vec<_>>(),
            vec![3, 7]
        );
        assert_eq!(accounts[0].source_kind, SourceKind::RemoteHttp);
        assert_eq!(accounts[0].namespace_version, 2);
    }

    #[test]
    fn children_pages_anchor_after_the_previous_page() {
        let root = TempRoot::new();
        seed(&root);
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");
        let first = store
            .children(root_id().text(), None, 1)
            .expect("first page");
        assert_eq!(first.len(), 1);
        let second = store
            .children(root_id().text(), Some(first[0].id.clone()), 1)
            .expect("second page");
        assert!(
            second.is_empty(),
            "one child means the second page is empty"
        );
    }

    #[test]
    fn bounded_date_first_pages_are_container_bound_and_stable_across_relaunch() {
        let root = TempRoot::new();
        seed(&root);
        let layout = shared_state_layout(root.as_str().to_owned()).expect("layout");
        let mut writer = StateStore::open(&layout.database_file).expect("open writer");
        let chat = ChatKey {
            scope: scope(),
            chat_id: ChatId(101),
        };
        let chat_id = ItemKey::Canonical(CanonicalKey::Chat(chat)).id();
        let chat_json_id = ItemKey::Canonical(CanonicalKey::GeneratedDoc(GeneratedDocKey {
            chat,
            partition: DocPartition::Chat,
            format: DocFormat::Json,
            schema_family: SchemaFamily(1),
        }))
        .id();
        let active_id =
            ItemKey::Canonical(CanonicalKey::ActiveStories(ActiveStoriesKey { chat })).id();
        let month_ids = [
            ItemKey::Canonical(CanonicalKey::MonthDir(MonthDirKey {
                chat,
                year: 2026,
                month: 6,
            }))
            .id(),
            ItemKey::Canonical(CanonicalKey::MonthDir(MonthDirKey {
                chat,
                year: 2026,
                month: 7,
            }))
            .id(),
        ];
        let version = MetadataVersion::new("date-first-m1").expect("version");
        let txn = writer.write_txn().expect("write txn");
        txn.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: chat_id.clone(),
            parent: Some(root_id()),
            display_name: "Date First Chat".to_owned(),
            safe_name: "Date First Chat".to_owned(),
            metadata_version: version.clone(),
            content: None,
            availability: StoredItemAvailability::Fetchable,
            created_at_ms: Some(1_000),
            modified_at_ms: Some(1_000),
            deleted_at_ms: None,
        })
        .expect("chat");
        txn.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: chat_json_id.clone(),
            parent: Some(chat_id.clone()),
            display_name: ".chat.json".to_owned(),
            safe_name: ".chat.json".to_owned(),
            metadata_version: version.clone(),
            content: Some(FileFacts {
                mime_type: Some("application/json".to_owned()),
                logical_size: Some(128),
                content_version: Some(ContentVersion::new("chat-c1").expect("content version")),
            }),
            availability: StoredItemAvailability::Fetchable,
            created_at_ms: Some(1_000),
            modified_at_ms: Some(2_000),
            deleted_at_ms: None,
        })
        .expect("chat json");
        for (id, name) in [
            (&active_id, "Active Stories"),
            (&month_ids[0], "2026-06"),
            (&month_ids[1], "2026-07"),
        ] {
            txn.upsert_item(&ItemRecord {
                aggregate_size: None,
                id: id.clone(),
                parent: Some(chat_id.clone()),
                display_name: name.to_owned(),
                safe_name: name.to_owned(),
                metadata_version: version.clone(),
                content: None,
                availability: StoredItemAvailability::Fetchable,
                created_at_ms: Some(1_000),
                modified_at_ms: Some(1_000),
                deleted_at_ms: None,
            })
            .expect("fixed child");
        }
        txn.commit().expect("commit");
        drop(writer);

        let provider =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");
        let first_page = provider
            .children_page(chat_id.text(), None, 10_000)
            .expect("bounded first page");
        assert_eq!(first_page.items.len(), 4);
        assert_eq!(first_page.next_after, None);
        let names: std::collections::BTreeSet<_> = first_page
            .items
            .iter()
            .map(|item| item.safe_name.as_str())
            .collect();
        assert_eq!(
            names,
            std::collections::BTreeSet::from([
                "Active Stories",
                "2026-06",
                "2026-07",
                ".chat.json",
            ])
        );
        let chat_json = first_page
            .items
            .iter()
            .find(|item| item.id == chat_json_id.text())
            .expect("chat json metadata");
        assert_eq!(chat_json.logical_size, Some(128));
        assert_eq!(chat_json.content_version.as_deref(), Some("chat-c1"));
        assert_eq!(chat_json.created_at_ms, Some(1_000));
        assert_eq!(chat_json.modified_at_ms, Some(2_000));

        drop(provider);
        let relaunched =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("reopen");
        assert_eq!(
            relaunched
                .children_page(chat_id.text(), None, 256)
                .expect("relaunch page")
                .items,
            first_page.items,
            "direct-month and generated-document identities survive relaunch"
        );

        let page_one = relaunched
            .children_page(chat_id.text(), None, 2)
            .expect("page one");
        let anchor = page_one.next_after.clone().expect("continuation");
        let page_two = relaunched
            .children_page(chat_id.text(), Some(anchor.clone()), 2)
            .expect("page two");
        assert_eq!(page_two.items.len(), 2);
        assert_eq!(page_two.next_after, None);
        assert!(matches!(
            relaunched.children_page(chat_id.text(), Some(chat_dir_id().text()), 2),
            Err(DriveError::NotFound { .. })
        ));

        let mut writer = StateStore::open(&layout.database_file).expect("reopen writer");
        let txn = writer.write_txn().expect("write txn");
        txn.tombstone_item(
            &parse_item_id(&anchor).expect("anchor id"),
            3_000,
            &MetadataVersion::new("date-first-tombstone").expect("version"),
        )
        .expect("tombstone anchor");
        txn.commit().expect("commit");
        assert!(
            relaunched
                .children_page(chat_id.text(), Some(anchor), 2)
                .is_ok(),
            "a tombstoned item remains a valid durable keyset anchor"
        );
    }

    #[test]
    fn malformed_identifiers_are_invalid_arguments() {
        let root = TempRoot::new();
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");
        let err = store
            .item("not-an-item-id".to_owned())
            .expect_err("malformed id");
        assert!(matches!(err, DriveError::InvalidArgument { .. }));
    }

    #[test]
    fn data_version_moves_exactly_on_foreign_commits() {
        let root = TempRoot::new();
        seed(&root);
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");
        let before = store.data_version().expect("data_version");
        assert_eq!(
            store.data_version().expect("data_version"),
            before,
            "no foreign commit, no movement"
        );
        // A foreign commit: another connection tombstones the file item.
        let layout = store.layout();
        let mut writer = StateStore::open(&layout.database_file).expect("open writer");
        let txn = writer.write_txn().expect("write txn");
        txn.tombstone_item(
            &file_id(),
            2_000,
            &MetadataVersion::new("m2").expect("version"),
        )
        .expect("tombstone");
        txn.commit().expect("commit");
        assert_ne!(
            store.data_version().expect("data_version"),
            before,
            "a foreign commit must move the stamp"
        );
    }

    #[test]
    fn the_change_journal_reads_expose_durable_sequences_across_handles() {
        let root = TempRoot::new();
        seed(&root);
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider).expect("open");

        let state = store.change_journal_state().expect("journal state");
        assert_eq!(state.instance_id.len(), 32, "the journal names its life");
        assert!(state.latest_sequence > 0, "the seed writes were journaled");

        // The whole journal, paged one change at a time: strictly
        // increasing sequences composing into exactly the seeded tree.
        let mut anchor = 0;
        let mut walked = Vec::new();
        loop {
            let page = store
                .item_changes_since(7, anchor, 1)
                .expect("changes page");
            let Some(change) = page.first() else { break };
            assert_eq!(page.len(), 1);
            assert!(change.sequence > anchor);
            anchor = change.sequence;
            walked.push(change.metadata.id.clone());
        }
        assert_eq!(
            walked,
            vec![root_id().text(), chat_dir_id().text(), file_id().text()]
        );
        assert_eq!(anchor, state.latest_sequence);

        // A foreign commit — another connection tombstoning the file — is a
        // change this handle pages from its anchor, as a deletion.
        let layout = store.layout();
        let mut writer = StateStore::open(&layout.database_file).expect("open writer");
        let txn = writer.write_txn().expect("write txn");
        txn.tombstone_item(
            &file_id(),
            2_000,
            &MetadataVersion::new("m2").expect("version"),
        )
        .expect("tombstone");
        txn.commit().expect("commit");

        let changes = store
            .item_changes_since(7, anchor, 100)
            .expect("changes after anchor");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].metadata.id, file_id().text());
        assert_eq!(
            changes[0].metadata.deleted_at_ms,
            Some(2_000),
            "a tombstone pages as a deletion"
        );
        assert_eq!(
            store
                .change_journal_state()
                .expect("journal state")
                .latest_sequence,
            changes[0].sequence
        );
    }

    #[test]
    fn quarantine_requires_the_coordinator_role() {
        let root = TempRoot::new();
        let err = quarantine_corrupt_state(root.as_str().to_owned(), StateRole::Provider)
            .expect_err("provider must be refused");
        assert!(matches!(err, DriveError::InvalidArgument { .. }));
    }

    #[test]
    fn quarantine_declines_healthy_state_and_recovers_corrupt_state() {
        let root = TempRoot::new();
        seed(&root);
        assert_eq!(
            quarantine_corrupt_state(root.as_str().to_owned(), StateRole::Coordinator)
                .expect("healthy quarantine"),
            None,
            "healthy state must never be quarantined"
        );

        // Corrupt the database header out from under the layout.
        let layout = shared_state_layout(root.as_str().to_owned()).expect("layout");
        std::fs::write(&layout.database_file, b"garbage, not a database").expect("corrupt");

        let quarantine_dir =
            quarantine_corrupt_state(root.as_str().to_owned(), StateRole::Coordinator)
                .expect("quarantine")
                .expect("corrupt state must be moved");
        assert!(
            quarantine_dir.starts_with(&layout.quarantine_dir),
            "damaged files must land under the layout's quarantine dir"
        );
        assert!(!Path::new(&layout.database_file).exists());

        // The cleared path opens fresh, in either role.
        let store =
            SharedStateStore::open(root.as_str().to_owned(), StateRole::Coordinator).expect("open");
        assert_eq!(store.item(root_id().text()).expect("item"), None);
    }

    #[test]
    fn open_reports_corruption_as_storage() {
        let root = TempRoot::new();
        let layout = shared_state_layout(root.as_str().to_owned()).expect("layout");
        std::fs::create_dir_all(&layout.state_dir).expect("state dir");
        std::fs::write(&layout.database_file, b"garbage, not a database").expect("corrupt");
        let err = SharedStateStore::open(root.as_str().to_owned(), StateRole::Provider)
            .expect_err("corrupt open must fail");
        assert!(matches!(err, DriveError::Storage { .. }));
    }
}
