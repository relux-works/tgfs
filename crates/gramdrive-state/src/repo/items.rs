//! The provider projection: every node a native provider can see, keyed by
//! its stable [`ItemId`] (DEC-008, DOM-001/002/022/024).
//!
//! The identity columns of an `items` row — kind, account, namespace,
//! canonical link, view — are all *derivable from the id itself*, and this
//! module derives them rather than trusting the caller to repeat them
//! consistently: an [`ItemRecord`] carries only what the id cannot say
//! (tree position, names, versions, content facts, timestamps). The one
//! exception is the account root, whose id deliberately excludes the
//! namespace epoch (DOM-021); its epoch column is read from the account row
//! at upsert time.

use gramdrive_model::identity::{
    AccountScope, AppearanceKey, CanonicalKey, ChatKey, ChatListKey, ChatListKind,
    FolderCatalogKey, ItemId, ItemKey, StoryAppearanceLocation,
};
use gramdrive_model::version::{ContentVersion, MetadataVersion, directory_metadata_version};
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{
    ReadTxn, WriteTxn, item_id_from_column, scope_columns, size_from_column, size_to_column,
};

/// What kind of provider node an item is (`items.kind`).
///
/// Mirrors the [`CanonicalKey`] vocabulary minus messages and blobs: v1
/// surfaces neither as a provider node (messages render into generated
/// docs; blobs back attachments).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// Canonical story bytes (not enumerated directly).
    CanonicalStory,
    /// Active or persistent appearance of canonical story bytes.
    StoryAppearance,
    /// A generated NDJSON/Markdown/JSON document.
    GeneratedDoc,
    /// The `order.json` ordering-metadata document of a list root.
    OrderDoc,
}

impl ItemKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::ChatList => "chat_list",
            Self::FolderCatalog => "folder_catalog",
            Self::Chat => "chat",
            Self::ActiveStories => "active_stories",
            Self::MonthDir => "month_dir",
            Self::YearDir => "year_dir",
            Self::MediaDir => "media_dir",
            Self::Attachment => "attachment",
            Self::CanonicalStory => "canonical_story",
            Self::StoryAppearance => "story_appearance",
            Self::GeneratedDoc => "generated_doc",
            Self::OrderDoc => "order_doc",
        }
    }

    /// Whether items of this kind are directories — a function of kind by
    /// schema CHECK, restated here for callers.
    pub fn is_directory(self) -> bool {
        matches!(
            self,
            Self::Account
                | Self::ChatList
                | Self::FolderCatalog
                | Self::Chat
                | Self::ActiveStories
                | Self::MonthDir
                | Self::YearDir
                | Self::MediaDir
        )
    }
}

/// Content availability of a provider item (`items.availability`, POL-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemAvailability {
    /// Bytes can be fetched.
    Fetchable,
    /// Telegram restricts the content; bytes are never fetched.
    Restricted,
    /// The content is gone at the source.
    Unavailable,
}

impl ItemAvailability {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fetchable => "fetchable",
            Self::Restricted => "restricted",
            Self::Unavailable => "unavailable",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "fetchable" => Ok(Self::Fetchable),
            "restricted" => Ok(Self::Restricted),
            "unavailable" => Ok(Self::Unavailable),
            other => Err(StateError::CorruptRow {
                table: "items",
                detail: format!("unknown availability '{other}'"),
            }),
        }
    }
}

/// Content facts of a file item. Directories never carry them (schema
/// CHECK); a file may carry none yet (a generated doc before its first
/// render).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileFacts {
    /// MIME type, if known.
    pub mime_type: Option<String>,
    /// Logical size in bytes, if known.
    pub logical_size: Option<u64>,
    /// Version of the bytes providers currently see (DOM-003).
    pub content_version: Option<ContentVersion>,
}

/// One provider-visible node (DOM-001).
///
/// Identity columns are derived from [`ItemRecord::id`] — see the module
/// docs. Reads return `content` as `Some` for every file kind and `None`
/// for every directory; writes accept `None` for a file as shorthand for
/// "no content facts yet".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemRecord {
    /// The node's stable provider identity.
    pub id: ItemId,
    /// The parent node; `None` exactly for the account root.
    pub parent: Option<ItemId>,
    /// Name for display.
    pub display_name: String,
    /// Filesystem-safe name, unique among live siblings (SYNC-012).
    pub safe_name: String,
    /// Version of the node's provider-visible metadata (DOM-003).
    pub metadata_version: MetadataVersion,
    /// Content facts; files only.
    pub content: Option<FileFacts>,
    /// Exact logical size of the node's indexed descendants; directories
    /// only, and `None` until a reconciliation pass has rolled it up
    /// (BUG-260728-2qfzbd).
    ///
    /// Deliberately separate from [`FileFacts::logical_size`]: that field
    /// stays "this file's own bytes", so a directory can never claim file
    /// content facts and a file can never claim a subtree rollup. The value
    /// is the sum of the *known* logical sizes below the node — a descendant
    /// whose size is not indexed yet contributes nothing rather than an
    /// estimate (SYNC-032 keeps exact size a source fact, never a guess).
    pub aggregate_size: Option<u64>,
    /// Content availability (POL-4).
    pub availability: ItemAvailability,
    /// Creation time, if known (ms since the Unix epoch).
    pub created_at_ms: Option<i64>,
    /// Last modification time, if known (ms since the Unix epoch).
    pub modified_at_ms: Option<i64>,
    /// POL-3 tombstone: when the node's deletion was observed.
    pub deleted_at_ms: Option<i64>,
}

/// The durable policy pass that first observed an item tombstone.
///
/// This is a fixed vocabulary, not diagnostic text: it cannot carry a name,
/// identifier, source error, or any other user-derived detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstoneProvenance {
    /// Projection reconciliation removed a stale branch.
    Reconcile,
    /// A retention-mode transition removed local availability.
    Retention,
    /// A policy decision invalidated the item.
    Policy,
}

impl TombstoneProvenance {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reconcile => "reconcile",
            Self::Retention => "retention",
            Self::Policy => "policy",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "reconcile" => Ok(Self::Reconcile),
            "retention" => Ok(Self::Retention),
            "policy" => Ok(Self::Policy),
            _ => Err(StateError::CorruptRow {
                table: "items",
                detail: "unknown tombstone provenance".to_owned(),
            }),
        }
    }
}

/// The provider-node kind encoded in an item id.
///
/// Fails for ids that are not provider nodes — messages and blobs
/// (`InvalidArgument`) — and for an appearance wrapping something that
/// cannot appear under a view (the account root, another non-node).
pub fn item_kind(id: &ItemId) -> Result<ItemKind, StateError> {
    match id.key() {
        ItemKey::Canonical(key) => canonical_kind(&key),
        ItemKey::Appearance(appearance) => {
            let kind = canonical_kind(&appearance.item)?;
            if kind == ItemKind::Account {
                return Err(StateError::InvalidArgument {
                    what: "the account root has no per-view appearance",
                });
            }
            Ok(kind)
        }
        ItemKey::StoryAppearance(_) => Ok(ItemKind::StoryAppearance),
    }
}

fn canonical_kind(key: &CanonicalKey) -> Result<ItemKind, StateError> {
    match key {
        CanonicalKey::Account(_) => Ok(ItemKind::Account),
        CanonicalKey::ChatList(_) => Ok(ItemKind::ChatList),
        CanonicalKey::FolderCatalog(_) => Ok(ItemKind::FolderCatalog),
        CanonicalKey::Chat(_) => Ok(ItemKind::Chat),
        CanonicalKey::ActiveStories(_) => Ok(ItemKind::ActiveStories),
        CanonicalKey::MonthDir(_) => Ok(ItemKind::MonthDir),
        CanonicalKey::YearDir(_) => Ok(ItemKind::YearDir),
        CanonicalKey::MediaDir(_) => Ok(ItemKind::MediaDir),
        CanonicalKey::Attachment(_) => Ok(ItemKind::Attachment),
        CanonicalKey::Story(_) => Ok(ItemKind::CanonicalStory),
        CanonicalKey::GeneratedDoc(_) => Ok(ItemKind::GeneratedDoc),
        CanonicalKey::OrderDoc(_) => Ok(ItemKind::OrderDoc),
        CanonicalKey::Message(_) => Err(StateError::InvalidArgument {
            what: "messages are not provider nodes in v1",
        }),
        CanonicalKey::Blob(_) => Err(StateError::InvalidArgument {
            what: "blobs are not provider nodes",
        }),
    }
}

/// The scope a canonical key carries, when it carries one — everything but
/// the account root, whose id deliberately excludes the epoch (DOM-021).
fn canonical_scope(key: &CanonicalKey) -> Option<AccountScope> {
    match key {
        CanonicalKey::Account(_) | CanonicalKey::Blob(_) => None,
        CanonicalKey::ChatList(k) => Some(k.scope),
        CanonicalKey::FolderCatalog(k) => Some(k.scope),
        CanonicalKey::Chat(k) => Some(k.scope),
        CanonicalKey::ActiveStories(k) => Some(k.chat.scope),
        CanonicalKey::MonthDir(k) => Some(k.chat.scope),
        CanonicalKey::YearDir(k) => Some(k.chat.scope),
        CanonicalKey::MediaDir(k) => Some(k.chat.scope),
        CanonicalKey::Message(k) => Some(k.chat.scope),
        CanonicalKey::Attachment(k) => Some(k.message.chat.scope),
        CanonicalKey::Story(k) => Some(k.poster.scope),
        CanonicalKey::GeneratedDoc(k) => Some(k.chat.scope),
        CanonicalKey::OrderDoc(k) => Some(k.list.scope),
    }
}

/// The `(view_kind, view_folder_id)` column pair of an appearance view.
fn view_columns(view: ChatListKind) -> Result<(&'static str, Option<i64>), StateError> {
    match view {
        ChatListKind::Main => Ok(("main", None)),
        ChatListKind::Archive => Ok(("archive", None)),
        ChatListKind::Stories => Ok(("stories", None)),
        ChatListKind::Folder(folder) => {
            if folder.0 == 0 {
                Err(StateError::InvalidArgument {
                    what: "folder id 0 is the built-in-list sentinel, not a real folder",
                })
            } else {
                Ok(("folder", Some(i64::from(folder.0))))
            }
        }
    }
}

/// Everything an upsert derives from the id: kind, scope (or the account
/// whose current epoch to read), and the appearance columns.
struct DerivedIdentity {
    kind: ItemKind,
    scope: ScopeSource,
    canonical_item_id: Option<Vec<u8>>,
    view: Option<(&'static str, Option<i64>)>,
}

enum ScopeSource {
    /// The key carries its scope.
    Carried(AccountScope),
    /// The account root: epoch comes from the account row.
    AccountRow(i64),
}

fn derive_identity(id: &ItemId) -> Result<DerivedIdentity, StateError> {
    let kind = item_kind(id)?;
    match id.key() {
        ItemKey::Canonical(key) => {
            let scope = match canonical_scope(&key) {
                Some(scope) => ScopeSource::Carried(scope),
                None => match key {
                    CanonicalKey::Account(account) => ScopeSource::AccountRow(account.account_id.0),
                    // canonical_kind already rejected blobs.
                    _ => {
                        return Err(StateError::InvalidArgument {
                            what: "item key carries no account scope",
                        });
                    }
                },
            };
            Ok(DerivedIdentity {
                kind,
                scope,
                canonical_item_id: None,
                view: None,
            })
        }
        ItemKey::Appearance(appearance) => {
            let scope = canonical_scope(&appearance.item).ok_or(StateError::InvalidArgument {
                what: "appearance wraps a key that carries no account scope",
            })?;
            Ok(DerivedIdentity {
                kind,
                scope: ScopeSource::Carried(scope),
                canonical_item_id: Some(
                    ItemKey::Canonical(appearance.item).id().as_bytes().to_vec(),
                ),
                view: Some(view_columns(appearance.view)?),
            })
        }
        ItemKey::StoryAppearance(appearance) => Ok(DerivedIdentity {
            kind: ItemKind::StoryAppearance,
            scope: ScopeSource::Carried(appearance.story.poster.scope),
            canonical_item_id: Some(
                ItemKey::Canonical(CanonicalKey::Story(appearance.story))
                    .id()
                    .as_bytes()
                    .to_vec(),
            ),
            view: Some(view_columns(appearance.view)?),
        }),
    }
}

/// The columns every item read selects, in [`read_item`]'s order.
const ITEM_COLUMNS: &str = "item_id, parent_item_id, display_name, safe_name, metadata_version,
     mime_type, logical_size, content_version, availability,
     created_at_ms, modified_at_ms, deleted_at_ms, aggregate_size";

struct RawItem {
    item_id: Vec<u8>,
    parent_item_id: Option<Vec<u8>>,
    display_name: String,
    safe_name: String,
    metadata_version: String,
    mime_type: Option<String>,
    logical_size: Option<i64>,
    content_version: Option<String>,
    availability: String,
    created_at_ms: Option<i64>,
    modified_at_ms: Option<i64>,
    deleted_at_ms: Option<i64>,
    aggregate_size: Option<i64>,
}

fn read_item(row: &Row<'_>) -> Result<RawItem, rusqlite::Error> {
    Ok(RawItem {
        item_id: row.get(0)?,
        parent_item_id: row.get(1)?,
        display_name: row.get(2)?,
        safe_name: row.get(3)?,
        metadata_version: row.get(4)?,
        mime_type: row.get(5)?,
        logical_size: row.get(6)?,
        content_version: row.get(7)?,
        availability: row.get(8)?,
        created_at_ms: row.get(9)?,
        modified_at_ms: row.get(10)?,
        deleted_at_ms: row.get(11)?,
        aggregate_size: row.get(12)?,
    })
}

fn finish_item(raw: RawItem) -> Result<ItemRecord, StateError> {
    let id = item_id_from_column("items", &raw.item_id)?;
    let kind = item_kind(&id)?;
    let aggregate_size = if kind.is_directory() {
        raw.aggregate_size
            .map(|size| size_from_column("items", size))
            .transpose()?
    } else {
        None
    };
    let content = if kind.is_directory() {
        None
    } else {
        Some(FileFacts {
            mime_type: raw.mime_type,
            logical_size: raw
                .logical_size
                .map(|size| size_from_column("items", size))
                .transpose()?,
            content_version: raw
                .content_version
                .map(|text| {
                    ContentVersion::new(text).map_err(|error| StateError::CorruptRow {
                        table: "items",
                        detail: format!("content_version does not parse: {error}"),
                    })
                })
                .transpose()?,
        })
    };
    Ok(ItemRecord {
        id,
        parent: raw
            .parent_item_id
            .map(|bytes| item_id_from_column("items", &bytes))
            .transpose()?,
        display_name: raw.display_name,
        safe_name: raw.safe_name,
        metadata_version: MetadataVersion::new(raw.metadata_version).map_err(|error| {
            StateError::CorruptRow {
                table: "items",
                detail: format!("metadata_version does not parse: {error}"),
            }
        })?,
        content,
        aggregate_size,
        availability: ItemAvailability::parse(&raw.availability)?,
        created_at_ms: raw.created_at_ms,
        modified_at_ms: raw.modified_at_ms,
        deleted_at_ms: raw.deleted_at_ms,
    })
}

impl ReadTxn<'_> {
    /// The immutable provenance of an item's first tombstone, when present.
    /// Schema v18 backfills every installed tombstone and enforces the
    /// deleted/provenance invariant for future writes.
    pub fn tombstone_provenance(
        &self,
        id: &ItemId,
    ) -> Result<Option<TombstoneProvenance>, StateError> {
        self.conn()
            .prepare_cached("SELECT tombstone_provenance FROM items WHERE item_id = ?1")?
            .query_row(params![id.as_bytes()], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten()
            .map(|value| TombstoneProvenance::parse(&value))
            .transpose()
    }

    /// Every currently live stored child in identity order, without applying
    /// source-specific presentation ordering. Projection reconciliation uses
    /// this to find stale rows that no longer have a folder or membership
    /// join and therefore cannot appear through `children_page`.
    pub fn stored_children(&self, parent: &ItemId) -> Result<Vec<ItemRecord>, StateError> {
        let mut statement = self.conn().prepare_cached(&format!(
            "SELECT {ITEM_COLUMNS} FROM items
             WHERE parent_item_id = ?1 AND deleted_at_ms IS NULL
             ORDER BY item_id"
        ))?;
        let rows = statement.query_map(params![parent.as_bytes()], read_item)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(finish_item(row?)?);
        }
        Ok(items)
    }

    /// One provider node by its stable identity (DOM-024).
    pub fn item(&self, id: &ItemId) -> Result<Option<ItemRecord>, StateError> {
        let raw = self
            .conn()
            .prepare_cached(&format!(
                "SELECT {ITEM_COLUMNS} FROM items WHERE item_id = ?1"
            ))?
            .query_row(params![id.as_bytes()], read_item)
            .optional()?;
        raw.map(finish_item).transpose()
    }

    /// One page of a directory's live children in stable id order,
    /// anchored after the last id of the previous page (SYNC-003).
    pub fn children_page(
        &self,
        parent: &ItemId,
        after: Option<&ItemId>,
        limit: u32,
    ) -> Result<Vec<ItemRecord>, StateError> {
        match parent.key() {
            ItemKey::Canonical(CanonicalKey::Account(account)) => {
                let account_record = self
                    .account(account)?
                    .ok_or(StateError::RowNotFound { entity: "account" })?;
                let scope = account_record.scope();
                let ids = [
                    ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                        scope,
                        kind: ChatListKind::Main,
                    }))
                    .id(),
                    ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                        scope,
                        kind: ChatListKind::Archive,
                    }))
                    .id(),
                    ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                        scope,
                        kind: ChatListKind::Stories,
                    }))
                    .id(),
                    ItemKey::Canonical(CanonicalKey::FolderCatalog(FolderCatalogKey { scope }))
                        .id(),
                ];
                // The exact Telegram root contract applies once the owned
                // lifecycle has installed all four fixed roots. Before that,
                // retain the generic store behavior used by migrations and
                // source-free callers that may have arbitrary account-root
                // children.
                let fixed_roots_ready = ids.iter().try_fold(true, |ready, id| {
                    Ok::<_, StateError>(
                        ready
                            && self
                                .item(id)?
                                .is_some_and(|item| item.deleted_at_ms.is_none()),
                    )
                })?;
                if fixed_roots_ready {
                    return self.item_id_page(&ids, after, limit);
                }
                return self.stored_children_page(parent, after, limit);
            }
            ItemKey::Canonical(CanonicalKey::FolderCatalog(catalog)) => {
                let ids: Vec<ItemId> = self
                    .folders(catalog.scope)?
                    .into_iter()
                    .map(|folder| {
                        ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                            scope: catalog.scope,
                            kind: ChatListKind::Folder(folder.folder_id),
                        }))
                        .id()
                    })
                    .collect();
                return self.item_id_page(&ids, after, limit);
            }
            ItemKey::Canonical(CanonicalKey::ChatList(list)) => {
                // Legacy/source-free callers may store arbitrary children
                // under a list root without normalized membership rows. Keep
                // the generic item-id contract for that shape; a composed
                // namespace always has membership rows and takes the ordered
                // path below.
                if self.chat_list(&list)?.is_empty() {
                    return self.stored_children_page(parent, after, limit);
                }
                let after_chat = match after.map(ItemId::key) {
                    None => None,
                    Some(ItemKey::Appearance(AppearanceKey {
                        view,
                        item: CanonicalKey::Chat(chat),
                    })) if view == list.kind && chat.scope == list.scope => Some(chat.chat_id),
                    Some(_) => {
                        return Err(StateError::InvalidArgument {
                            what: "chat-list page anchor is not a child appearance",
                        });
                    }
                };
                let entries = self.chat_list_page(&list, after_chat, limit)?;
                let mut items = Vec::with_capacity(entries.len());
                for entry in entries {
                    let id = ItemKey::Appearance(AppearanceKey {
                        view: list.kind,
                        item: CanonicalKey::Chat(ChatKey {
                            scope: list.scope,
                            chat_id: entry.chat_id,
                        }),
                    })
                    .id();
                    let item = self.item(&id)?.ok_or(StateError::RowNotFound {
                        entity: "chat appearance item",
                    })?;
                    if item.deleted_at_ms.is_none() {
                        items.push(item);
                    }
                }
                return Ok(items);
            }
            _ => {}
        }

        self.stored_children_page(parent, after, limit)
    }

    fn stored_children_page(
        &self,
        parent: &ItemId,
        after: Option<&ItemId>,
        limit: u32,
    ) -> Result<Vec<ItemRecord>, StateError> {
        // Every stored id is non-empty (schema CHECK), so the empty blob is
        // a universal "before everything" anchor.
        let anchor: &[u8] = after.map_or(&[], ItemId::as_bytes);
        let mut statement = self.conn().prepare_cached(&format!(
            "SELECT {ITEM_COLUMNS} FROM items
             WHERE parent_item_id = ?1 AND deleted_at_ms IS NULL AND item_id > ?2
             ORDER BY item_id LIMIT ?3"
        ))?;
        let rows = statement.query_map(
            params![parent.as_bytes(), anchor, i64::from(limit)],
            read_item,
        )?;
        let mut items = Vec::new();
        for row in rows {
            items.push(finish_item(row?)?);
        }
        Ok(items)
    }

    fn item_id_page(
        &self,
        ids: &[ItemId],
        after: Option<&ItemId>,
        limit: u32,
    ) -> Result<Vec<ItemRecord>, StateError> {
        let start = match after {
            None => 0,
            Some(anchor) => {
                ids.iter()
                    .position(|id| id == anchor)
                    .ok_or(StateError::RowNotFound {
                        entity: "item page anchor",
                    })?
                    + 1
            }
        };
        let mut items = Vec::new();
        for id in ids.iter().skip(start).take(limit as usize) {
            if let Some(item) = self.item(id)?
                && item.deleted_at_ms.is_none()
            {
                items.push(item);
            }
        }
        Ok(items)
    }

    /// The live child of `parent` named `safe_name`, for path resolution one
    /// component at a time (DOM-005).
    pub fn child_by_name(
        &self,
        parent: &ItemId,
        safe_name: &str,
    ) -> Result<Option<ItemRecord>, StateError> {
        let raw = self
            .conn()
            .prepare_cached(&format!(
                "SELECT {ITEM_COLUMNS} FROM items
                 WHERE parent_item_id = ?1 AND safe_name = ?2 AND deleted_at_ms IS NULL"
            ))?
            .query_row(params![parent.as_bytes(), safe_name], read_item)
            .optional()?;
        raw.map(finish_item).transpose()
    }

    /// Every appearance row of one canonical item, for propagating a
    /// canonical change to every view (SYNC-026).
    pub fn appearances_of(&self, canonical: &ItemId) -> Result<Vec<ItemRecord>, StateError> {
        let mut statement = self.conn().prepare_cached(&format!(
            "SELECT {ITEM_COLUMNS} FROM items WHERE canonical_item_id = ?1"
        ))?;
        let rows = statement.query_map(params![canonical.as_bytes()], read_item)?;
        let mut items = Vec::new();
        for row in rows {
            items.push(finish_item(row?)?);
        }
        Ok(items)
    }
}

/// The provider-visible column tuple of one `items` row — exactly the
/// columns `upsert_item`'s ON CONFLICT clause updates, read for the change
/// detection that keeps the item change journal quiet on identical
/// re-pushes.
type ProviderVisibleRow = (
    Option<Vec<u8>>, // parent_item_id
    String,          // display_name
    String,          // safe_name
    Option<String>,  // mime_type
    Option<i64>,     // logical_size
    String,          // metadata_version
    Option<String>,  // content_version
    String,          // availability
    Option<i64>,     // created_at_ms
    Option<i64>,     // modified_at_ms
    Option<i64>,     // deleted_at_ms
    Option<i64>,     // aggregate_size
);

/// The columns `update_item_content` compares and rewrites, in SELECT
/// order: content_version, mime_type, logical_size, metadata_version,
/// modified_at_ms.
type StoredContentRow = (
    Option<String>, // content_version
    Option<String>, // mime_type
    Option<i64>,    // logical_size
    String,         // metadata_version
    Option<i64>,    // modified_at_ms
);

/// Archive lifecycle inputs joined from one item and its account.
type ArchivePinState = (
    bool,           // account archive_mode
    i64,            // account policy update time
    String,         // item availability
    Option<String>, // item content version
    Option<i64>,    // item deletion time
);

fn provider_visible_row(row: &Row<'_>) -> rusqlite::Result<ProviderVisibleRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

impl WriteTxn<'_> {
    /// Inserts or fully replaces one provider node.
    ///
    /// Identity columns are derived from the id (module docs); the record
    /// must be structurally coherent — parent absent exactly for the
    /// account root, content facts only on files — and violations are typed
    /// errors, not CHECK failures.
    pub fn upsert_item(&self, record: &ItemRecord) -> Result<(), StateError> {
        let derived = derive_identity(&record.id)?;
        if matches!(derived.kind, ItemKind::YearDir | ItemKind::MediaDir)
            && record.deleted_at_ms.is_none()
        {
            return Err(StateError::InvalidArgument {
                what: "legacy year and media directory kinds are tombstones only",
            });
        }
        if record.safe_name.is_empty() {
            return Err(StateError::InvalidArgument {
                what: "item safe_name must not be empty",
            });
        }
        if (derived.kind == ItemKind::Account) != record.parent.is_none() {
            return Err(StateError::InvalidArgument {
                what: "the account root, and only the account root, has no parent",
            });
        }
        if derived.kind.is_directory() && record.content.is_some() {
            return Err(StateError::InvalidArgument {
                what: "directories carry no content facts",
            });
        }
        if !derived.kind.is_directory() && record.aggregate_size.is_some() {
            return Err(StateError::InvalidArgument {
                what: "only directories carry a descendant size rollup",
            });
        }
        let (account_id, namespace) = match derived.scope {
            ScopeSource::Carried(scope) => scope_columns(&scope),
            ScopeSource::AccountRow(account_id) => {
                let namespace: Option<i64> = self
                    .conn()
                    .prepare_cached("SELECT namespace_version FROM accounts WHERE account_id = ?1")?
                    .query_row(params![account_id], |row| row.get(0))
                    .optional()?;
                let namespace = namespace.ok_or(StateError::RowNotFound { entity: "account" })?;
                (account_id, namespace)
            }
        };
        let content = record.content.clone().unwrap_or_default();
        let logical_size = content.logical_size.map(size_to_column).transpose()?;
        let aggregate_size = record.aggregate_size.map(size_to_column).transpose()?;
        let (view_kind, view_folder_id) = match derived.view {
            Some((kind, folder)) => (Some(kind), folder),
            None => (None, None),
        };
        // Change detection for the item change journal: an upsert that
        // rewrites the row it would replace — the engine re-baselining after
        // a restart (SYNC-021 replay) — is provider-invisible and must not
        // advance the item's change sequence, or every restart would replay
        // the whole tree at the provider boundary. The compared columns are
        // exactly the ones the ON CONFLICT clause below updates.
        let stored: Option<ProviderVisibleRow> = self
            .conn()
            .prepare_cached(
                "SELECT parent_item_id, display_name, safe_name, mime_type, logical_size,
                        metadata_version, content_version, availability, created_at_ms,
                        modified_at_ms, deleted_at_ms, aggregate_size
                 FROM items WHERE item_id = ?1",
            )?
            .query_row(params![record.id.as_bytes()], provider_visible_row)
            .optional()?;
        let incoming: ProviderVisibleRow = (
            record.parent.as_ref().map(|id| id.as_bytes().to_vec()),
            record.display_name.clone(),
            record.safe_name.clone(),
            content.mime_type.clone(),
            logical_size,
            record.metadata_version.as_str().to_owned(),
            content
                .content_version
                .as_ref()
                .map(|version| version.as_str().to_owned()),
            record.availability.as_str().to_owned(),
            record.created_at_ms,
            record.modified_at_ms,
            record.deleted_at_ms,
            aggregate_size,
        );
        if stored.as_ref() == Some(&incoming) {
            self.sync_archive_pin(&record.id)?;
            return Ok(());
        }
        self.conn()
            .prepare_cached(
                "INSERT INTO items (item_id, account_id, namespace_version, kind,
                                    parent_item_id, canonical_item_id, view_kind,
                                    view_folder_id, display_name, safe_name, is_directory,
                                    mime_type, logical_size, metadata_version,
                                    content_version, availability, created_at_ms,
                                    modified_at_ms, deleted_at_ms, aggregate_size,
                                    tombstone_provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                         ?16, ?17, ?18, ?19, ?20, ?21)
                 ON CONFLICT (item_id) DO UPDATE SET
                     parent_item_id = excluded.parent_item_id,
                     display_name = excluded.display_name,
                     safe_name = excluded.safe_name,
                     mime_type = excluded.mime_type,
                     logical_size = excluded.logical_size,
                     metadata_version = excluded.metadata_version,
                     content_version = excluded.content_version,
                     availability = excluded.availability,
                     created_at_ms = excluded.created_at_ms,
                     modified_at_ms = excluded.modified_at_ms,
                     deleted_at_ms = excluded.deleted_at_ms,
                     aggregate_size = excluded.aggregate_size,
                     tombstone_provenance = excluded.tombstone_provenance",
            )?
            .execute(params![
                record.id.as_bytes(),
                account_id,
                namespace,
                derived.kind.as_str(),
                record.parent.as_ref().map(ItemId::as_bytes),
                derived.canonical_item_id,
                view_kind,
                view_folder_id,
                record.display_name,
                record.safe_name,
                derived.kind.is_directory(),
                content.mime_type,
                logical_size,
                record.metadata_version.as_str(),
                content.content_version.as_ref().map(ContentVersion::as_str),
                record.availability.as_str(),
                record.created_at_ms,
                record.modified_at_ms,
                record.deleted_at_ms,
                aggregate_size,
                record
                    .deleted_at_ms
                    .map(|_| TombstoneProvenance::Reconcile.as_str()),
            ])?;
        self.journal_item_change(&record.id)?;
        self.sync_archive_pin(&record.id)?;
        Ok(())
    }

    /// Replaces a file item's content facts if — and only if — its stored
    /// content version is exactly `expected` (DOM-003 compare-and-set;
    /// SYNC-042's "fetched for A, never published as B").
    ///
    /// `expected: None` means "never published": the first publication of a
    /// generated document. On conflict nothing changes and the caller
    /// re-reads.
    pub fn update_item_content(
        &self,
        id: &ItemId,
        expected: Option<&ContentVersion>,
        facts: &FileFacts,
        new_metadata_version: &MetadataVersion,
        modified_at_ms: i64,
    ) -> Result<(), StateError> {
        if item_kind(id)?.is_directory() {
            return Err(StateError::InvalidArgument {
                what: "directories carry no content facts",
            });
        }
        let stored: Option<StoredContentRow> = self
            .conn()
            .prepare_cached(
                "SELECT content_version, mime_type, logical_size, metadata_version,
                            modified_at_ms
                     FROM items WHERE item_id = ?1",
            )?
            .query_row(params![id.as_bytes()], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .optional()?;
        let (stored_version, stored_mime, stored_size, stored_metadata, stored_modified) =
            stored.ok_or(StateError::RowNotFound { entity: "item" })?;
        if stored_version.as_deref() != expected.map(ContentVersion::as_str) {
            return Err(StateError::VersionConflict {
                entity: "item content",
                expected: expected.map(|version| version.as_str().to_owned()),
                found: stored_version,
            });
        }
        let logical_size = facts.logical_size.map(size_to_column).transpose()?;
        // The journal's no-op discipline: republishing the identical facts
        // under the identical versions is provider-invisible.
        if stored_mime == facts.mime_type
            && stored_size == logical_size
            && stored_version.as_deref()
                == facts.content_version.as_ref().map(ContentVersion::as_str)
            && stored_metadata == new_metadata_version.as_str()
            && stored_modified == Some(modified_at_ms)
        {
            return Ok(());
        }
        self.conn()
            .prepare_cached(
                "UPDATE items
                 SET mime_type = ?2, logical_size = ?3, content_version = ?4,
                     metadata_version = ?5, modified_at_ms = ?6
                 WHERE item_id = ?1",
            )?
            .execute(params![
                id.as_bytes(),
                facts.mime_type,
                logical_size,
                facts.content_version.as_ref().map(ContentVersion::as_str),
                new_metadata_version.as_str(),
                modified_at_ms,
            ])?;
        self.journal_item_change(id)?;
        self.sync_archive_pin(id)?;
        Ok(())
    }

    /// Recomputes the descendant size rollup of every directory ancestor of
    /// `id`, up to and including the chat it lives under (BUG-260728-2qfzbd).
    ///
    /// Publishing a generated document changes its logical size, and that
    /// size is a term of its month's rollup and of its chat's. The namespace
    /// projection also computes these, but a full chat reconciliation
    /// re-reads every message instant and every attachment projection in the
    /// chat — far too much work to run after each publication, and measured
    /// at over 80% sustained agent CPU when it did. This walk is bounded by
    /// the tree's depth and by each ancestor's own child count.
    ///
    /// The metadata version is composed from the same inputs, through the
    /// same shared helper, that the projection uses. Both owners therefore
    /// write the identical token for identical state, so neither can undo
    /// the other and a quiet republication never reaches the provider.
    ///
    /// Ancestors above the chat (chat lists, the account root) are left
    /// alone: they are not correspondence, their child counts are unbounded,
    /// and no product surface asks for their size.
    pub fn refresh_ancestor_rollups(&self, id: &ItemId) -> Result<(), StateError> {
        let mut current = self.parent_of(id)?;
        while let Some(ancestor) = current {
            let kind = item_kind(&ancestor)?;
            if !matches!(
                kind,
                ItemKind::Chat | ItemKind::MonthDir | ItemKind::ActiveStories
            ) {
                break;
            }
            let reached_chat = kind == ItemKind::Chat;
            self.rewrite_rollup(&ancestor)?;
            if reached_chat {
                break;
            }
            current = self.parent_of(&ancestor)?;
        }
        Ok(())
    }

    fn parent_of(&self, id: &ItemId) -> Result<Option<ItemId>, StateError> {
        let raw: Option<Option<Vec<u8>>> = self
            .conn()
            .prepare_cached("SELECT parent_item_id FROM items WHERE item_id = ?1")?
            .query_row(params![id.as_bytes()], |row| row.get(0))
            .optional()?;
        match raw.flatten() {
            Some(bytes) => Ok(Some(item_id_from_column("items", &bytes)?)),
            None => Ok(None),
        }
    }

    /// Rewrites one directory's rollup and version, journalling only when
    /// something a provider can see actually moved.
    fn rewrite_rollup(&self, directory: &ItemId) -> Result<(), StateError> {
        type RollupRow = (String, String, Option<i64>, Option<i64>, Option<i64>);
        let stored: Option<RollupRow> = self
            .conn()
            .prepare_cached(
                "SELECT display_name, metadata_version, created_at_ms, modified_at_ms,
                        aggregate_size
                 FROM items WHERE item_id = ?1 AND deleted_at_ms IS NULL",
            )?
            .query_row(params![directory.as_bytes()], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .optional()?;
        let Some((display_name, stored_version, created_at_ms, modified_at_ms, stored_size)) =
            stored
        else {
            return Ok(());
        };
        let total: i64 = self
            .conn()
            .prepare_cached(
                "SELECT COALESCE(sum(COALESCE(aggregate_size, logical_size)), 0)
                 FROM items WHERE parent_item_id = ?1 AND deleted_at_ms IS NULL",
            )?
            .query_row(params![directory.as_bytes()], |row| row.get(0))?;
        let aggregate = u64::try_from(total).map_err(|_| StateError::CorruptRow {
            table: "items",
            detail: "descendant size rollup is negative".to_owned(),
        })?;
        // The chat directory's version base is the chat's own metadata
        // version, exactly as the projection composes it; every other
        // directory bases on its display name.
        let base = match directory.key() {
            ItemKey::Appearance(AppearanceKey {
                item: CanonicalKey::Chat(chat),
                ..
            }) => self.chat_metadata_version(&chat)?.unwrap_or(display_name),
            _ => display_name,
        };
        // This walk only ever reaches chat, month and `Active Stories`
        // directories (see `refresh_ancestor_rollups`), every one of which
        // owns a rollup — so the size is always present here, and the
        // absent case belongs to the kinds this walk stops before.
        let version =
            directory_metadata_version(&base, created_at_ms, modified_at_ms, Some(aggregate))
                .map_err(|error| StateError::CorruptRow {
                    table: "items",
                    detail: format!("directory metadata version: {error}"),
                })?;
        // Republishing identical facts under an identical version is
        // provider-invisible and must not advance the item's change sequence,
        // or every quiet publication tick would wake the provider.
        if stored_size == Some(total) && stored_version == version.as_str() {
            return Ok(());
        }
        self.conn()
            .prepare_cached(
                "UPDATE items SET aggregate_size = ?2, metadata_version = ?3
                 WHERE item_id = ?1",
            )?
            .execute(params![directory.as_bytes(), total, version.as_str()])?;
        self.journal_item_change(directory)
    }

    fn chat_metadata_version(&self, chat: &ChatKey) -> Result<Option<String>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        Ok(self
            .conn()
            .prepare_cached(
                "SELECT metadata_version FROM chats
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3",
            )?
            .query_row(params![account_id, namespace, chat.chat_id.0], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// Tombstones a node per POL-3: the row stays, keeps its name history,
    /// and stops counting as a live sibling (SYNC-012).
    ///
    /// Idempotent — tombstoning an already-tombstoned node keeps the
    /// original observation time.
    pub fn tombstone_item(
        &self,
        id: &ItemId,
        deleted_at_ms: i64,
        new_metadata_version: &MetadataVersion,
    ) -> Result<(), StateError> {
        self.tombstone_item_with_provenance(
            id,
            deleted_at_ms,
            new_metadata_version,
            TombstoneProvenance::Reconcile,
        )
    }

    /// Tombstones an item with the fixed policy pass that caused it. The
    /// first tombstone wins, matching `deleted_at_ms` idempotence.
    pub fn tombstone_item_with_provenance(
        &self,
        id: &ItemId,
        deleted_at_ms: i64,
        new_metadata_version: &MetadataVersion,
        provenance: TombstoneProvenance,
    ) -> Result<(), StateError> {
        let changed = self
            .conn()
            .prepare_cached(
                "UPDATE items SET deleted_at_ms = ?2, metadata_version = ?3,
                                  tombstone_provenance = ?4
                 WHERE item_id = ?1 AND deleted_at_ms IS NULL",
            )?
            .execute(params![
                id.as_bytes(),
                deleted_at_ms,
                new_metadata_version.as_str(),
                provenance.as_str(),
            ])?;
        if changed == 0 {
            let exists: Option<i64> = self
                .conn()
                .prepare_cached("SELECT 1 FROM items WHERE item_id = ?1")?
                .query_row(params![id.as_bytes()], |row| row.get(0))
                .optional()?;
            if exists.is_none() {
                return Err(StateError::RowNotFound { entity: "item" });
            }
            // Already tombstoned: the idempotent re-observation changed
            // nothing provider-visible, so the journal stays quiet.
            self.sync_archive_pin(id)?;
            return Ok(());
        }
        self.journal_item_change(id)?;
        self.sync_archive_pin(id)?;
        Ok(())
    }

    /// Keeps Archive-Mode byte intent coupled to the current provider
    /// projection. This runs on live item insert/update/tombstone so content
    /// observed after Archive Mode was enabled is covered without another
    /// toggle, while protection/unavailability immediately releases only the
    /// Archive-owned pin. Explicit user intent is never overwritten.
    fn sync_archive_pin(&self, id: &ItemId) -> Result<(), StateError> {
        let state: Option<ArchivePinState> = self
            .conn()
            .prepare_cached(
                "SELECT a.archive_mode, a.updated_at_ms, i.availability,
                        i.content_version, i.deleted_at_ms
                 FROM items i JOIN accounts a ON a.account_id = i.account_id
                 WHERE i.item_id = ?1",
            )?
            .query_row(params![id.as_bytes()], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .optional()?;
        let Some((archive_mode, policy_at_ms, availability, content_version, deleted_at_ms)) =
            state
        else {
            return Err(StateError::RowNotFound { entity: "item" });
        };
        let persistent_content = match id.key() {
            ItemKey::Canonical(CanonicalKey::Attachment(_)) => true,
            ItemKey::Appearance(AppearanceKey {
                item: CanonicalKey::Attachment(_),
                ..
            }) => true,
            ItemKey::StoryAppearance(appearance) => {
                appearance.location != StoryAppearanceLocation::Active
            }
            _ => false,
        };
        let eligible = archive_mode
            && persistent_content
            && availability == "fetchable"
            && content_version.is_some()
            && deleted_at_ms.is_none();
        if eligible {
            self.conn()
                .prepare_cached(
                    "INSERT INTO pins (item_id, origin, created_at_ms)
                     VALUES (?1, 'archive_mode', ?2)
                     ON CONFLICT (item_id) DO NOTHING",
                )?
                .execute(params![id.as_bytes(), policy_at_ms])?;
            self.conn()
                .prepare_cached(
                    "UPDATE cache_entries
                     SET pinned = 1,
                         pin_origin = CASE
                             WHEN pin_origin = 'user' THEN 'user' ELSE 'archive_mode' END
                     WHERE item_id = ?1",
                )?
                .execute(params![id.as_bytes()])?;
        } else {
            self.conn()
                .prepare_cached("DELETE FROM pins WHERE item_id = ?1 AND origin = 'archive_mode'")?
                .execute(params![id.as_bytes()])?;
            self.conn()
                .prepare_cached(
                    "UPDATE cache_entries SET pinned = 0, pin_origin = NULL
                     WHERE item_id = ?1 AND pin_origin = 'archive_mode'",
                )?
                .execute(params![id.as_bytes()])?;
        }
        Ok(())
    }
}
