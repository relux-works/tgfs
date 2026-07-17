//! Virtual tree builder (TASK-260715-3tjduq; SYNC-010..012, PRD-010..013).
//!
//! [`TreeProjection`] projects normalized source records into the default
//! logical layout of `.spec/sync-and-filesystem-semantics.md`:
//!
//! ```text
//! Account/
//!   Main/
//!     Chat/
//!       chat.json
//!       messages.ndjson
//!       2026/
//!         07.md
//!         media/
//!   Archive/
//!   Telegram Folders/
//! ```
//!
//! # Appearances over shared canonical records (SYNC-010, PRD-013)
//!
//! The layout is virtual. A projection stores exactly one record per
//! canonical chat; the chat-list views hold only references to it. Every
//! node below a view root carries an appearance identity — the view wrapped
//! around the unchanged canonical key — so the same chat in Main and in a
//! custom folder is two [`ItemId`]s over one record, never two records.
//! Attachment nodes reference materialized bytes through a shared
//! [`BlobKey`]; the projection never duplicates content identity either.
//!
//! # Lazy, paged enumeration (SYNC-003, SYNC-040)
//!
//! Nodes are never materialized eagerly: construction indexes the input
//! records, and [`TreeProjection::children`] mints only the requested page
//! of the requested parent — the shape a File Provider enumerator needs.
//! Page boundaries are anchored by the last returned child's [`ItemId`];
//! within one projection (one snapshot), pages are repeatable and can
//! neither duplicate nor skip children. Enumeration exposes metadata only —
//! it can never hydrate content.
//!
//! # Determinism (SYNC-011, SYNC-012 context)
//!
//! Sibling order is derived from stable identity — fixed roots, then folder
//! IDs, chat IDs, years, months, and message/attachment ordinals — never
//! from input or discovery order, so shuffled input yields a byte-identical
//! tree. Display names are raw presentation strings in the POL-1 stable form
//! (`crate::naming::chat_folder_name`); sanitization and collision suffixing
//! are the naming policy's job (`crate::naming`, TASK-260715-1ffbkg), applied
//! by the consumer over a sibling set, and Telegram ordering metadata is the
//! ordering projection's (TASK-260715-1jmsdp).
//!
//! # Read-only capabilities (DEC-007, SYNC-060)
//!
//! Every node advertises [`Capabilities`] with all write operations off.
//! The fields exist so providers can map them mechanically; v1 constructors
//! cannot produce anything writable (SYNC-063 owns any future change).

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use crate::identity::{
    AccountId, AccountKey, AccountScope, AppearanceKey, AttachmentIndex, AttachmentKey, BlobKey,
    CanonicalKey, ChatId, ChatKey, ChatListKey, ChatListKind, ContentHash, DocFormat, DocPartition,
    FolderCatalogKey, FolderId, GeneratedDocKey, ItemId, ItemKey, MediaDirKey, MessageId,
    MessageKey, NamespaceVersion, SchemaFamily, YearDirKey,
};
use crate::naming::chat_folder_name;

/// Display name of the Main chat-list root.
const MAIN_NAME: &str = "Main";
/// Display name of the Archive chat-list root.
const ARCHIVE_NAME: &str = "Archive";
/// Display name of the custom-folder catalog.
const FOLDER_CATALOG_NAME: &str = "Telegram Folders";
/// Display name of the per-year media directory.
const MEDIA_DIR_NAME: &str = "media";
/// Display name of the chat metadata document.
const CHAT_JSON_NAME: &str = "chat.json";
/// Display name of the whole-chat NDJSON document.
const MESSAGES_NDJSON_NAME: &str = "messages.ndjson";

/// Calendar month stamp — the partition a message or attachment falls into.
///
/// `month` is 1–12; [`TreeProjection::new`] rejects anything else, which is
/// the semantic validation [`DocPartition`] deliberately leaves to the tree
/// builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MonthStamp {
    /// Calendar year.
    pub year: u16,
    /// Calendar month, 1–12.
    pub month: u8,
}

/// Normalized facts about the account a projection is built for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    /// Stable account identifier.
    pub account_id: AccountId,
    /// Current identity-namespace epoch of the account.
    pub namespace_version: NamespaceVersion,
    /// Display name of the account root directory.
    pub display_name: String,
}

/// One custom Telegram folder (chat filter) of the account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRecord {
    /// Telegram folder identifier.
    pub folder_id: FolderId,
    /// Raw folder title; sanitization is naming policy, not tree structure.
    pub title: String,
}

/// Normalized facts about one canonical chat.
///
/// Identifier-level on purpose: records carry bare Telegram IDs and the
/// projection derives every key from its own account scope, so a record
/// belonging to a different account is not representable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRecord {
    /// Telegram chat identifier.
    pub chat_id: ChatId,
    /// Raw chat title.
    pub title: String,
    /// Public username, when the chat has one (POL-1 name component).
    pub username: Option<String>,
    /// Chat-list views the chat currently appears in. Duplicates are
    /// harmless and collapse; order never matters.
    pub memberships: Vec<ChatListKind>,
    /// Months with at least one observed message — each becomes a `MM.md`
    /// partition under its year directory.
    pub message_months: Vec<MonthStamp>,
    /// Downloadable attachments of the chat's messages.
    pub attachments: Vec<AttachmentRecord>,
}

/// Normalized facts about one downloadable attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentRecord {
    /// Message the attachment belongs to.
    pub message_id: MessageId,
    /// Ordinal within that message's attachments.
    pub index: AttachmentIndex,
    /// Month of the owning message — places the file under `YYYY/media/`.
    pub month: MonthStamp,
    /// Raw file display name.
    pub display_name: String,
    /// Logical size in bytes, when the source reports one.
    pub size: Option<u64>,
    /// Content hash of the fully materialized bytes, when known. Two
    /// attachments with equal hashes share one blob identity.
    pub content: Option<ContentHash>,
}

/// Schema families of the generated documents the tree publishes (DOM-023).
///
/// Family numbers are assigned by the rendering layer
/// (STORY-260715-1oq9jg); the tree builder only stamps them into
/// generated-document identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocSchemas {
    /// Family of the `chat.json` metadata document.
    pub chat_json: SchemaFamily,
    /// Family of the whole-chat `messages.ndjson` document.
    pub messages_ndjson: SchemaFamily,
    /// Family of the monthly `MM.md` Markdown documents.
    pub month_markdown: SchemaFamily,
}

/// What a provider may advertise for one item (SYNC-060, SYNC-061).
///
/// V1 is read-only with respect to Telegram (DEC-007): the write-side
/// fields exist so native providers can map capabilities mechanically, but
/// both constructors pin them `false`. Advertising writes is a future,
/// separately specified product change (SYNC-063).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Capabilities {
    /// Children may be enumerated (directories).
    pub enumerate_children: bool,
    /// Content may be read/hydrated (files).
    pub read_content: bool,
    /// Content may be written. Always `false` in v1.
    pub write_content: bool,
    /// The item may be renamed. Always `false` in v1.
    pub rename: bool,
    /// The item may be moved to another parent. Always `false` in v1.
    pub relocate: bool,
    /// The item may be deleted. Always `false` in v1.
    pub delete: bool,
}

impl Capabilities {
    /// Read-only directory: enumerable, no content, no writes.
    pub const fn read_only_directory() -> Self {
        Self {
            enumerate_children: true,
            read_content: false,
            write_content: false,
            rename: false,
            relocate: false,
            delete: false,
        }
    }

    /// Read-only file: readable content, no children, no writes.
    pub const fn read_only_file() -> Self {
        Self {
            enumerate_children: false,
            read_content: true,
            write_content: false,
            rename: false,
            relocate: false,
            delete: false,
        }
    }
}

/// Structural kind of a tree node.
///
/// Maps onto the `Item.kind` vocabulary of `.spec/domain-model.md`: `Root` is
/// `root`; `ChatList` covers `list` and `folder_view`; `FolderCatalog` is the
/// fixed grouping directory between them; `Chat`, `Year`, `Media` are `chat`,
/// `year`, `media_dir`; `GeneratedDoc` is `generated_file`; `Attachment` is
/// `file`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// The account root directory.
    Root,
    /// A chat-list view root: Main, Archive, or one custom folder.
    ChatList,
    /// The fixed directory grouping the custom-folder views.
    FolderCatalog,
    /// One appearance of a chat as a folder.
    Chat,
    /// A calendar-year directory of a chat's export.
    Year,
    /// The media directory of one chat-export year.
    Media,
    /// A generated document: `chat.json`, `messages.ndjson`, or `MM.md`.
    GeneratedDoc,
    /// A downloadable attachment file.
    Attachment,
}

impl NodeKind {
    /// Whether nodes of this kind are directories.
    pub fn is_directory(self) -> bool {
        !matches!(self, Self::GeneratedDoc | Self::Attachment)
    }
}

/// One provider-visible node of the virtual tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// Identity of this node — appearance-wrapped below a view root.
    pub id: ItemId,
    /// Identity of the parent appearance; `None` for the account root.
    pub parent: Option<ItemId>,
    /// Structural kind.
    pub kind: NodeKind,
    /// Raw display name (pre-sanitization; POL-1 stable form for chats).
    pub display_name: String,
    /// The canonical record this node projects. Multiple appearances of one
    /// item share this key — the non-duplication guarantee in observable
    /// form (PRD-013).
    pub canonical: CanonicalKey,
    /// What a provider may advertise for the node (read-only in v1).
    pub capabilities: Capabilities,
    /// Logical size in bytes, when known. Generated documents report `None`
    /// until the renderer owns their bytes.
    pub size: Option<u64>,
    /// Shared content identity of materialized bytes, when known
    /// (attachments only).
    pub content: Option<BlobKey>,
}

/// One page of children (SYNC-003).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildPage {
    /// The page's nodes, in canonical sibling order.
    pub nodes: Vec<TreeNode>,
    /// Page boundary: pass as `after` to fetch the next page. `None` means
    /// enumeration is complete.
    pub next: Option<ItemId>,
}

/// Why input records cannot form a projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeInputError {
    /// Two folder records share one folder ID.
    DuplicateFolder {
        /// The duplicated folder ID.
        folder: FolderId,
    },
    /// Two chat records share one chat ID.
    DuplicateChat {
        /// The duplicated chat ID.
        chat: ChatId,
    },
    /// A chat claims membership in a folder no folder record describes.
    UnknownFolderMembership {
        /// The chat with the dangling membership.
        chat: ChatId,
        /// The unknown folder.
        folder: FolderId,
    },
    /// A month stamp is outside 1–12.
    InvalidMonth {
        /// The chat carrying the stamp.
        chat: ChatId,
        /// The offending stamp.
        stamp: MonthStamp,
    },
    /// Two attachment records share one (message, index) identity.
    DuplicateAttachment {
        /// The chat carrying the attachment.
        chat: ChatId,
        /// The owning message.
        message: MessageId,
        /// The duplicated ordinal.
        index: AttachmentIndex,
    },
}

impl std::fmt::Display for TreeInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateFolder { folder } => {
                write!(f, "duplicate folder record for folder {}", folder.0)
            }
            Self::DuplicateChat { chat } => {
                write!(f, "duplicate chat record for chat {}", chat.0)
            }
            Self::UnknownFolderMembership { chat, folder } => write!(
                f,
                "chat {} is member of unknown folder {}",
                chat.0, folder.0
            ),
            Self::InvalidMonth { chat, stamp } => write!(
                f,
                "chat {} has invalid month {} in year {}",
                chat.0, stamp.month, stamp.year
            ),
            Self::DuplicateAttachment {
                chat,
                message,
                index,
            } => write!(
                f,
                "chat {} has duplicate attachment (message {}, index {})",
                chat.0, message.0, index.0
            ),
        }
    }
}

impl std::error::Error for TreeInputError {}

/// Why a children request cannot be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildrenError {
    /// The parent identity does not resolve to a node of this projection.
    UnknownParent,
    /// The parent resolves to a file; files have no children.
    NotADirectory,
    /// The `after` boundary is not a child of this parent in this
    /// projection. Page boundaries are snapshot-scoped: re-enumerate from
    /// the start against the new projection.
    ForeignPageBoundary,
}

impl std::fmt::Display for ChildrenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownParent => f.write_str("parent is not a node of this projection"),
            Self::NotADirectory => f.write_str("parent is not a directory"),
            Self::ForeignPageBoundary => {
                f.write_str("page boundary is not a child of this parent in this projection")
            }
        }
    }
}

impl std::error::Error for ChildrenError {}

/// Sort rank of a chat-list view. Fixed lists precede custom folders;
/// folders order by folder ID — identity, never discovery order.
fn view_rank(view: ChatListKind) -> (u8, i32) {
    match view {
        ChatListKind::Main => (0, 0),
        ChatListKind::Archive => (1, 0),
        ChatListKind::Folder(FolderId(id)) => (2, id),
    }
}

/// Canonical state of one chat — stored exactly once per projection.
#[derive(Debug)]
struct ChatState {
    title: String,
    username: Option<String>,
    /// Export years, each with its month partitions and media set.
    years: BTreeMap<u16, YearState>,
    /// Attachment metadata by (message ID, ordinal), for direct resolution.
    attachments: BTreeMap<(i64, u32), AttachmentState>,
}

#[derive(Debug, Default)]
struct YearState {
    /// Months (1–12) with a `MM.md` partition.
    months: BTreeSet<u8>,
    /// Attachments of this year, in (message ID, ordinal) order.
    media: BTreeSet<(i64, u32)>,
}

#[derive(Debug)]
struct AttachmentState {
    month: MonthStamp,
    display_name: String,
    size: Option<u64>,
    content: Option<ContentHash>,
}

/// A deterministic snapshot of the virtual tree for one account.
///
/// Immutable once built: enumeration is repeatable by construction
/// (SYNC-003), and a source change means building a new projection, never
/// mutating this one.
#[derive(Debug)]
pub struct TreeProjection {
    scope: AccountScope,
    display_name: String,
    schemas: DocSchemas,
    /// Custom folders by folder ID.
    folders: BTreeMap<i32, String>,
    /// Canonical chat records by chat ID — one entry per chat, whatever the
    /// number of appearances.
    chats: BTreeMap<i64, ChatState>,
    /// Member chat IDs per view, keyed by [`view_rank`]. Views hold only
    /// references into [`Self::chats`], never copies.
    members: BTreeMap<(u8, i32), BTreeSet<i64>>,
}

impl TreeProjection {
    /// Builds a projection from normalized records.
    ///
    /// Record order never influences the result: all state is keyed by
    /// stable identity. Inconsistent records — duplicate identities,
    /// memberships in unknown folders, months outside 1–12 — are input
    /// contract violations and fail loudly.
    pub fn new(
        account: AccountRecord,
        folders: Vec<FolderRecord>,
        chats: Vec<ChatRecord>,
        schemas: DocSchemas,
    ) -> Result<Self, TreeInputError> {
        let scope = AccountScope {
            account: AccountKey {
                account_id: account.account_id,
            },
            namespace_version: account.namespace_version,
        };

        let mut folder_map = BTreeMap::new();
        for folder in folders {
            if folder_map
                .insert(folder.folder_id.0, folder.title)
                .is_some()
            {
                return Err(TreeInputError::DuplicateFolder {
                    folder: folder.folder_id,
                });
            }
        }

        let mut members: BTreeMap<(u8, i32), BTreeSet<i64>> = BTreeMap::new();
        members.insert(view_rank(ChatListKind::Main), BTreeSet::new());
        members.insert(view_rank(ChatListKind::Archive), BTreeSet::new());
        for folder_id in folder_map.keys() {
            members.insert(
                view_rank(ChatListKind::Folder(FolderId(*folder_id))),
                BTreeSet::new(),
            );
        }

        let mut chat_map: BTreeMap<i64, ChatState> = BTreeMap::new();
        for chat in chats {
            if chat_map.contains_key(&chat.chat_id.0) {
                return Err(TreeInputError::DuplicateChat { chat: chat.chat_id });
            }

            let mut years: BTreeMap<u16, YearState> = BTreeMap::new();
            for stamp in &chat.message_months {
                validate_month(chat.chat_id, *stamp)?;
                years
                    .entry(stamp.year)
                    .or_default()
                    .months
                    .insert(stamp.month);
            }

            let mut attachments = BTreeMap::new();
            for attachment in chat.attachments {
                validate_month(chat.chat_id, attachment.month)?;
                let ordinal = (attachment.message_id.0, attachment.index.0);
                let state = AttachmentState {
                    month: attachment.month,
                    display_name: attachment.display_name,
                    size: attachment.size,
                    content: attachment.content,
                };
                if attachments.insert(ordinal, state).is_some() {
                    return Err(TreeInputError::DuplicateAttachment {
                        chat: chat.chat_id,
                        message: attachment.message_id,
                        index: attachment.index,
                    });
                }
                years
                    .entry(attachment.month.year)
                    .or_default()
                    .media
                    .insert(ordinal);
            }

            for membership in &chat.memberships {
                if let ChatListKind::Folder(folder) = membership
                    && !folder_map.contains_key(&folder.0)
                {
                    return Err(TreeInputError::UnknownFolderMembership {
                        chat: chat.chat_id,
                        folder: *folder,
                    });
                }
                if let Some(view_members) = members.get_mut(&view_rank(*membership)) {
                    view_members.insert(chat.chat_id.0);
                }
            }

            chat_map.insert(
                chat.chat_id.0,
                ChatState {
                    title: chat.title,
                    username: chat.username,
                    years,
                    attachments,
                },
            );
        }

        Ok(Self {
            scope,
            display_name: account.display_name,
            schemas,
            folders: folder_map,
            chats: chat_map,
            members,
        })
    }

    /// Identity of the account root directory.
    pub fn root_id(&self) -> ItemId {
        ItemKey::Canonical(CanonicalKey::Account(self.scope.account)).id()
    }

    /// The account root node.
    pub fn root(&self) -> TreeNode {
        TreeNode {
            id: self.root_id(),
            parent: None,
            kind: NodeKind::Root,
            display_name: self.display_name.clone(),
            canonical: CanonicalKey::Account(self.scope.account),
            capabilities: Capabilities::read_only_directory(),
            size: None,
            content: None,
        }
    }

    /// Resolves an identity to its node in this projection.
    ///
    /// `None` means the identity names no position in this tree: a foreign
    /// account, a view the item is not a member of, a partition that does
    /// not exist, or a key kind that is a record rather than a tree
    /// position (messages, blobs, unwrapped canonical chats).
    pub fn node(&self, id: &ItemId) -> Option<TreeNode> {
        self.resolve(&id.key())
    }

    /// Enumerates one page of a directory's children (SYNC-003).
    ///
    /// `after` is the page boundary returned by the previous page — `None`
    /// starts from the first child. Boundaries are scoped to this
    /// projection; a boundary minted against a different snapshot fails
    /// with [`ChildrenError::ForeignPageBoundary`] rather than silently
    /// skipping or repeating children.
    pub fn children(
        &self,
        parent: &ItemId,
        after: Option<&ItemId>,
        limit: NonZeroUsize,
    ) -> Result<ChildPage, ChildrenError> {
        let parent_key = parent.key();
        let parent_node = self
            .resolve(&parent_key)
            .ok_or(ChildrenError::UnknownParent)?;
        if !parent_node.kind.is_directory() {
            return Err(ChildrenError::NotADirectory);
        }

        let keys = self.child_keys(&parent_key);
        let start = match after {
            None => 0,
            Some(boundary) => {
                let boundary_key = boundary.key();
                keys.iter()
                    .position(|key| *key == boundary_key)
                    .ok_or(ChildrenError::ForeignPageBoundary)?
                    + 1
            }
        };

        let nodes: Vec<TreeNode> = keys
            .get(start..)
            .unwrap_or(&[])
            .iter()
            .take(limit.get())
            .filter_map(|key| self.resolve(key))
            .collect();
        let next = if start + nodes.len() < keys.len() {
            nodes.last().map(|node| node.id.clone())
        } else {
            None
        };
        Ok(ChildPage { nodes, next })
    }

    // -----------------------------------------------------------------------
    // Child key generation — sibling order is identity order, per parent kind
    // -----------------------------------------------------------------------

    /// Ordered child keys of a directory node. Only called for keys that
    /// already resolved to a directory; anything else yields no children.
    fn child_keys(&self, parent: &ItemKey) -> Vec<ItemKey> {
        match parent {
            ItemKey::Canonical(CanonicalKey::Account(_)) => vec![
                ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                    scope: self.scope,
                    kind: ChatListKind::Main,
                })),
                ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                    scope: self.scope,
                    kind: ChatListKind::Archive,
                })),
                ItemKey::Canonical(CanonicalKey::FolderCatalog(FolderCatalogKey {
                    scope: self.scope,
                })),
            ],
            ItemKey::Canonical(CanonicalKey::FolderCatalog(_)) => self
                .folders
                .keys()
                .map(|folder_id| {
                    ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                        scope: self.scope,
                        kind: ChatListKind::Folder(FolderId(*folder_id)),
                    }))
                })
                .collect(),
            ItemKey::Canonical(CanonicalKey::ChatList(list)) => self
                .members
                .get(&view_rank(list.kind))
                .into_iter()
                .flatten()
                .map(|chat_id| {
                    ItemKey::Appearance(AppearanceKey {
                        view: list.kind,
                        item: CanonicalKey::Chat(self.chat_key(*chat_id)),
                    })
                })
                .collect(),
            ItemKey::Appearance(AppearanceKey { view, item }) => {
                self.appearance_child_keys(*view, item)
            }
            _ => Vec::new(),
        }
    }

    fn appearance_child_keys(&self, view: ChatListKind, item: &CanonicalKey) -> Vec<ItemKey> {
        let appearance = |item: CanonicalKey| ItemKey::Appearance(AppearanceKey { view, item });
        match item {
            CanonicalKey::Chat(chat) => {
                let Some(state) = self.chats.get(&chat.chat_id.0) else {
                    return Vec::new();
                };
                let mut keys = vec![
                    appearance(CanonicalKey::GeneratedDoc(GeneratedDocKey {
                        chat: *chat,
                        partition: DocPartition::Chat,
                        format: DocFormat::Json,
                        schema_family: self.schemas.chat_json,
                    })),
                    appearance(CanonicalKey::GeneratedDoc(GeneratedDocKey {
                        chat: *chat,
                        partition: DocPartition::Chat,
                        format: DocFormat::Ndjson,
                        schema_family: self.schemas.messages_ndjson,
                    })),
                ];
                keys.extend(state.years.keys().map(|year| {
                    appearance(CanonicalKey::YearDir(YearDirKey {
                        chat: *chat,
                        year: *year,
                    }))
                }));
                keys
            }
            CanonicalKey::YearDir(dir) => {
                let Some(year) = self
                    .chats
                    .get(&dir.chat.chat_id.0)
                    .and_then(|state| state.years.get(&dir.year))
                else {
                    return Vec::new();
                };
                let mut keys: Vec<ItemKey> = year
                    .months
                    .iter()
                    .map(|month| {
                        appearance(CanonicalKey::GeneratedDoc(GeneratedDocKey {
                            chat: dir.chat,
                            partition: DocPartition::Month {
                                year: dir.year,
                                month: *month,
                            },
                            format: DocFormat::Markdown,
                            schema_family: self.schemas.month_markdown,
                        }))
                    })
                    .collect();
                if !year.media.is_empty() {
                    keys.push(appearance(CanonicalKey::MediaDir(MediaDirKey {
                        chat: dir.chat,
                        year: dir.year,
                    })));
                }
                keys
            }
            CanonicalKey::MediaDir(dir) => {
                let Some(year) = self
                    .chats
                    .get(&dir.chat.chat_id.0)
                    .and_then(|state| state.years.get(&dir.year))
                else {
                    return Vec::new();
                };
                year.media
                    .iter()
                    .map(|(message_id, index)| {
                        appearance(CanonicalKey::Attachment(AttachmentKey {
                            message: MessageKey {
                                chat: dir.chat,
                                message_id: MessageId(*message_id),
                            },
                            index: AttachmentIndex(*index),
                        }))
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Resolution — which (view, item) combinations are tree positions
    // -----------------------------------------------------------------------

    fn resolve(&self, key: &ItemKey) -> Option<TreeNode> {
        match key {
            ItemKey::Canonical(canonical) => self.resolve_canonical(canonical),
            ItemKey::Appearance(AppearanceKey { view, item }) => {
                self.resolve_appearance(*view, item)
            }
        }
    }

    /// Canonical tree positions: the account root, the three fixed roots,
    /// and custom-folder view roots. Every other canonical key is a record
    /// referenced by the tree, not a position in it.
    fn resolve_canonical(&self, key: &CanonicalKey) -> Option<TreeNode> {
        match key {
            CanonicalKey::Account(account) if *account == self.scope.account => Some(self.root()),
            CanonicalKey::ChatList(list) if list.scope == self.scope => {
                let (name, parent) = match list.kind {
                    ChatListKind::Main => (MAIN_NAME.to_string(), self.root_id()),
                    ChatListKind::Archive => (ARCHIVE_NAME.to_string(), self.root_id()),
                    ChatListKind::Folder(folder) => (
                        self.folders.get(&folder.0)?.clone(),
                        ItemKey::Canonical(CanonicalKey::FolderCatalog(FolderCatalogKey {
                            scope: self.scope,
                        }))
                        .id(),
                    ),
                };
                Some(self.directory_node(
                    ItemKey::Canonical(*key),
                    parent,
                    NodeKind::ChatList,
                    name,
                ))
            }
            CanonicalKey::FolderCatalog(catalog) if catalog.scope == self.scope => {
                Some(self.directory_node(
                    ItemKey::Canonical(*key),
                    self.root_id(),
                    NodeKind::FolderCatalog,
                    FOLDER_CATALOG_NAME.to_string(),
                ))
            }
            _ => None,
        }
    }

    fn resolve_appearance(&self, view: ChatListKind, item: &CanonicalKey) -> Option<TreeNode> {
        if !self.view_exists(view) {
            return None;
        }
        let appearance = |item: CanonicalKey| ItemKey::Appearance(AppearanceKey { view, item });
        match item {
            CanonicalKey::Chat(chat) => {
                let state = self.member_chat_state(view, chat)?;
                Some(
                    self.directory_node(
                        appearance(*item),
                        ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                            scope: self.scope,
                            kind: view,
                        }))
                        .id(),
                        NodeKind::Chat,
                        chat_folder_name(&state.title, state.username.as_deref()),
                    ),
                )
            }
            CanonicalKey::YearDir(dir) => {
                let state = self.member_chat_state(view, &dir.chat)?;
                state.years.get(&dir.year)?;
                Some(self.directory_node(
                    appearance(*item),
                    appearance(CanonicalKey::Chat(dir.chat)).id(),
                    NodeKind::Year,
                    format!("{:04}", dir.year),
                ))
            }
            CanonicalKey::MediaDir(dir) => {
                let state = self.member_chat_state(view, &dir.chat)?;
                let year = state.years.get(&dir.year)?;
                if year.media.is_empty() {
                    return None;
                }
                Some(
                    self.directory_node(
                        appearance(*item),
                        appearance(CanonicalKey::YearDir(YearDirKey {
                            chat: dir.chat,
                            year: dir.year,
                        }))
                        .id(),
                        NodeKind::Media,
                        MEDIA_DIR_NAME.to_string(),
                    ),
                )
            }
            CanonicalKey::GeneratedDoc(doc) => {
                let state = self.member_chat_state(view, &doc.chat)?;
                let (name, parent) = match (doc.partition, doc.format) {
                    (DocPartition::Chat, DocFormat::Json)
                        if doc.schema_family == self.schemas.chat_json =>
                    {
                        (
                            CHAT_JSON_NAME.to_string(),
                            appearance(CanonicalKey::Chat(doc.chat)).id(),
                        )
                    }
                    (DocPartition::Chat, DocFormat::Ndjson)
                        if doc.schema_family == self.schemas.messages_ndjson =>
                    {
                        (
                            MESSAGES_NDJSON_NAME.to_string(),
                            appearance(CanonicalKey::Chat(doc.chat)).id(),
                        )
                    }
                    (DocPartition::Month { year, month }, DocFormat::Markdown)
                        if doc.schema_family == self.schemas.month_markdown
                            && state
                                .years
                                .get(&year)
                                .is_some_and(|state| state.months.contains(&month)) =>
                    {
                        (
                            format!("{month:02}.md"),
                            appearance(CanonicalKey::YearDir(YearDirKey {
                                chat: doc.chat,
                                year,
                            }))
                            .id(),
                        )
                    }
                    _ => return None,
                };
                Some(TreeNode {
                    id: appearance(*item).id(),
                    parent: Some(parent),
                    kind: NodeKind::GeneratedDoc,
                    display_name: name,
                    canonical: *item,
                    capabilities: Capabilities::read_only_file(),
                    size: None,
                    content: None,
                })
            }
            CanonicalKey::Attachment(attachment) => {
                let state = self.member_chat_state(view, &attachment.message.chat)?;
                let ordinal = (attachment.message.message_id.0, attachment.index.0);
                let record = state.attachments.get(&ordinal)?;
                Some(TreeNode {
                    id: appearance(*item).id(),
                    parent: Some(
                        appearance(CanonicalKey::MediaDir(MediaDirKey {
                            chat: attachment.message.chat,
                            year: record.month.year,
                        }))
                        .id(),
                    ),
                    kind: NodeKind::Attachment,
                    display_name: record.display_name.clone(),
                    canonical: *item,
                    capabilities: Capabilities::read_only_file(),
                    size: record.size,
                    content: record.content.map(|hash| BlobKey {
                        account: self.scope.account,
                        hash,
                    }),
                })
            }
            // Accounts, chat lists, the folder catalog, messages, and blobs
            // never occur as appearances — that is this builder's
            // (view, item) discipline the identity layer defers to.
            _ => None,
        }
    }

    /// The canonical state of `chat`, provided the chat is a member of
    /// `view` — the check that makes non-member appearances unresolvable.
    fn member_chat_state(&self, view: ChatListKind, chat: &ChatKey) -> Option<&ChatState> {
        if chat.scope != self.scope {
            return None;
        }
        if !self
            .members
            .get(&view_rank(view))
            .is_some_and(|members| members.contains(&chat.chat_id.0))
        {
            return None;
        }
        self.chats.get(&chat.chat_id.0)
    }

    fn view_exists(&self, view: ChatListKind) -> bool {
        match view {
            ChatListKind::Main | ChatListKind::Archive => true,
            ChatListKind::Folder(folder) => self.folders.contains_key(&folder.0),
        }
    }

    fn chat_key(&self, chat_id: i64) -> ChatKey {
        ChatKey {
            scope: self.scope,
            chat_id: ChatId(chat_id),
        }
    }

    fn directory_node(
        &self,
        key: ItemKey,
        parent: ItemId,
        kind: NodeKind,
        display_name: String,
    ) -> TreeNode {
        let canonical = match key {
            ItemKey::Canonical(canonical) => canonical,
            ItemKey::Appearance(AppearanceKey { item, .. }) => item,
        };
        TreeNode {
            id: key.id(),
            parent: Some(parent),
            kind,
            display_name,
            canonical,
            capabilities: Capabilities::read_only_directory(),
            size: None,
            content: None,
        }
    }
}

fn validate_month(chat: ChatId, stamp: MonthStamp) -> Result<(), TreeInputError> {
    if (1..=12).contains(&stamp.month) {
        Ok(())
    } else {
        Err(TreeInputError::InvalidMonth { chat, stamp })
    }
}
