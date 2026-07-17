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

use gramdrive_model::identity::{AccountScope, CanonicalKey, ChatListKind, ItemId, ItemKey};
use gramdrive_model::version::{ContentVersion, MetadataVersion};
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

impl ItemKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::ChatList => "chat_list",
            Self::FolderCatalog => "folder_catalog",
            Self::Chat => "chat",
            Self::YearDir => "year_dir",
            Self::MediaDir => "media_dir",
            Self::Attachment => "attachment",
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
    /// Content availability (POL-4).
    pub availability: ItemAvailability,
    /// Creation time, if known (ms since the Unix epoch).
    pub created_at_ms: Option<i64>,
    /// Last modification time, if known (ms since the Unix epoch).
    pub modified_at_ms: Option<i64>,
    /// POL-3 tombstone: when the node's deletion was observed.
    pub deleted_at_ms: Option<i64>,
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
    }
}

fn canonical_kind(key: &CanonicalKey) -> Result<ItemKind, StateError> {
    match key {
        CanonicalKey::Account(_) => Ok(ItemKind::Account),
        CanonicalKey::ChatList(_) => Ok(ItemKind::ChatList),
        CanonicalKey::FolderCatalog(_) => Ok(ItemKind::FolderCatalog),
        CanonicalKey::Chat(_) => Ok(ItemKind::Chat),
        CanonicalKey::YearDir(_) => Ok(ItemKind::YearDir),
        CanonicalKey::MediaDir(_) => Ok(ItemKind::MediaDir),
        CanonicalKey::Attachment(_) => Ok(ItemKind::Attachment),
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
        CanonicalKey::YearDir(k) => Some(k.chat.scope),
        CanonicalKey::MediaDir(k) => Some(k.chat.scope),
        CanonicalKey::Message(k) => Some(k.chat.scope),
        CanonicalKey::Attachment(k) => Some(k.message.chat.scope),
        CanonicalKey::GeneratedDoc(k) => Some(k.chat.scope),
        CanonicalKey::OrderDoc(k) => Some(k.list.scope),
    }
}

/// The `(view_kind, view_folder_id)` column pair of an appearance view.
fn view_columns(view: ChatListKind) -> Result<(&'static str, Option<i64>), StateError> {
    match view {
        ChatListKind::Main => Ok(("main", None)),
        ChatListKind::Archive => Ok(("archive", None)),
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
    }
}

/// The columns every item read selects, in [`read_item`]'s order.
const ITEM_COLUMNS: &str = "item_id, parent_item_id, display_name, safe_name, metadata_version,
     mime_type, logical_size, content_version, availability,
     created_at_ms, modified_at_ms, deleted_at_ms";

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
    })
}

fn finish_item(raw: RawItem) -> Result<ItemRecord, StateError> {
    let id = item_id_from_column("items", &raw.item_id)?;
    let kind = item_kind(&id)?;
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
        availability: ItemAvailability::parse(&raw.availability)?,
        created_at_ms: raw.created_at_ms,
        modified_at_ms: raw.modified_at_ms,
        deleted_at_ms: raw.deleted_at_ms,
    })
}

impl ReadTxn<'_> {
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

impl WriteTxn<'_> {
    /// Inserts or fully replaces one provider node.
    ///
    /// Identity columns are derived from the id (module docs); the record
    /// must be structurally coherent — parent absent exactly for the
    /// account root, content facts only on files — and violations are typed
    /// errors, not CHECK failures.
    pub fn upsert_item(&self, record: &ItemRecord) -> Result<(), StateError> {
        let derived = derive_identity(&record.id)?;
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
        let (view_kind, view_folder_id) = match derived.view {
            Some((kind, folder)) => (Some(kind), folder),
            None => (None, None),
        };
        self.conn()
            .prepare_cached(
                "INSERT INTO items (item_id, account_id, namespace_version, kind,
                                    parent_item_id, canonical_item_id, view_kind,
                                    view_folder_id, display_name, safe_name, is_directory,
                                    mime_type, logical_size, metadata_version,
                                    content_version, availability, created_at_ms,
                                    modified_at_ms, deleted_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                         ?16, ?17, ?18, ?19)
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
                     deleted_at_ms = excluded.deleted_at_ms",
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
            ])?;
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
        let stored: Option<Option<String>> = self
            .conn()
            .prepare_cached("SELECT content_version FROM items WHERE item_id = ?1")?
            .query_row(params![id.as_bytes()], |row| row.get(0))
            .optional()?;
        let stored = stored.ok_or(StateError::RowNotFound { entity: "item" })?;
        if stored.as_deref() != expected.map(ContentVersion::as_str) {
            return Err(StateError::VersionConflict {
                entity: "item content",
                expected: expected.map(|version| version.as_str().to_owned()),
                found: stored,
            });
        }
        let logical_size = facts.logical_size.map(size_to_column).transpose()?;
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
        Ok(())
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
        let changed = self
            .conn()
            .prepare_cached(
                "UPDATE items SET deleted_at_ms = ?2, metadata_version = ?3
                 WHERE item_id = ?1 AND deleted_at_ms IS NULL",
            )?
            .execute(params![
                id.as_bytes(),
                deleted_at_ms,
                new_metadata_version.as_str(),
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
        }
        Ok(())
    }
}
