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
//! There are none here, deliberately. Durable state is written by the
//! engine inside its host process (Rust, in-process); foreign processes
//! read. A write surface at this boundary would invite the extension to
//! mutate state the engine owns (DEC-006 keeps the extension thin).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use gramdrive_model::identity::ItemId;
use gramdrive_state::repo::item_kind;
use gramdrive_state::{StateError, StateStore};

use crate::api::DriveError;

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
    /// A calendar-year directory of a chat's export.
    YearDir,
    /// The media directory of one chat-export year.
    MediaDir,
    /// A downloadable attachment file.
    Attachment,
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

/// One provider-visible node's metadata, as durably stored (DOM-001).
///
/// Identifiers are the stable text form of the item's [`ItemId`] (DOM-024)
/// — opaque to hosts, stable across processes and restarts. Version tokens
/// are opaque text compared for equality only (DOM-003).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ItemMetadata {
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
        let record = txn.item(&id).map_err(map_state_error)?;
        record.map(item_metadata).transpose()
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
        records.into_iter().map(item_metadata).collect()
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
        let record = txn
            .child_by_name(&parent, &safe_name)
            .map_err(map_state_error)?;
        record.map(item_metadata).transpose()
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

fn item_metadata(record: gramdrive_state::repo::ItemRecord) -> Result<ItemMetadata, DriveError> {
    let kind = map_kind(item_kind(&record.id).map_err(map_state_error)?);
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
    Ok(ItemMetadata {
        id: record.id.text(),
        parent: record.parent.as_ref().map(ItemId::text),
        kind,
        is_directory: kind_is_directory(kind),
        display_name: record.display_name,
        safe_name: record.safe_name,
        metadata_version: record.metadata_version.as_str().to_owned(),
        mime_type,
        logical_size,
        content_version,
        availability: map_availability(record.availability),
        created_at_ms: record.created_at_ms,
        modified_at_ms: record.modified_at_ms,
        deleted_at_ms: record.deleted_at_ms,
    })
}

fn map_kind(kind: gramdrive_state::repo::ItemKind) -> ItemKind {
    use gramdrive_state::repo::ItemKind as Stored;
    match kind {
        Stored::Account => ItemKind::Account,
        Stored::ChatList => ItemKind::ChatList,
        Stored::FolderCatalog => ItemKind::FolderCatalog,
        Stored::Chat => ItemKind::Chat,
        Stored::YearDir => ItemKind::YearDir,
        Stored::MediaDir => ItemKind::MediaDir,
        Stored::Attachment => ItemKind::Attachment,
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
        AccountId, AccountKey, AccountScope, AttachmentIndex, AttachmentKey, CanonicalKey, ChatId,
        ChatKey, ItemKey, MessageId, MessageKey, NamespaceVersion,
    };
    use gramdrive_model::version::{ContentVersion, MetadataVersion};
    use gramdrive_state::repo::{AccountRecord, FileFacts, ItemRecord, RetentionMode, SourceKind};

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
        txn.upsert_item(&ItemRecord {
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
