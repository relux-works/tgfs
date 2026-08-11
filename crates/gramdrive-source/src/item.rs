//! The provider-neutral item a source serves (SYNC-001, DOM entities;
//! TASK-260715-1j4ij3).
//!
//! A [`SourceItem`] is one provider-visible node as the *source* knows it:
//! identity, one virtual parent, raw display name, versions, and — through
//! [`ItemContent`] — either directory structure or file facts. It carries
//! the subset of `.spec/domain-model.md` § Item that a backend can know;
//! state-layer enrichments (materialization hints, provenance references)
//! belong to `gramdrive-state` rows, not to this contract.
//!
//! # Invalid states are structural
//!
//! Directory-ness is not a flag next to file fields: [`ItemContent`] is an
//! enum, so a directory with a content version, a byte size on a chat
//! folder, or a fetchable year directory cannot be expressed at all. The
//! same split carries the kind vocabulary — [`DirectoryKind`] and
//! [`FileKind`] partition `model::tree::NodeKind` so "which kind" and
//! "has bytes" cannot disagree; the bridges back to [`NodeKind`] are total.
//!
//! # Capabilities are derived
//!
//! [`SourceItem::capabilities`] computes what a provider may advertise from
//! the item's structure and availability. Deriving instead of storing makes
//! the contradiction — a restricted placeholder advertising `read_content`,
//! a file with `enumerate_children` — unrepresentable, and keeps v1
//! strictly read-only (DEC-007, SYNC-060): no code path here can produce a
//! writable capability set.

use gramdrive_model::identity::ItemId;
use gramdrive_model::tree::{Capabilities, NodeKind};
use gramdrive_model::version::{ContentVersion, MetadataVersion};

/// Structural kind of a directory item — the directory half of
/// [`NodeKind`] / the DOM `Item.kind` vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DirectoryKind {
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
}

impl DirectoryKind {
    /// The tree vocabulary this kind projects to.
    pub fn node_kind(self) -> NodeKind {
        match self {
            Self::Root => NodeKind::Root,
            Self::ChatList => NodeKind::ChatList,
            Self::FolderCatalog => NodeKind::FolderCatalog,
            Self::Chat => NodeKind::Chat,
            Self::Year => NodeKind::Year,
            Self::Media => NodeKind::Media,
        }
    }
}

/// Structural kind of a file item — the file half of [`NodeKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileKind {
    /// A generated document: `.chat.json`, `messages.ndjson`, `MM.md`, or
    /// `order.json`.
    GeneratedDoc,
    /// A downloadable attachment file.
    Attachment,
}

impl FileKind {
    /// The tree vocabulary this kind projects to.
    pub fn node_kind(self) -> NodeKind {
        match self {
            Self::GeneratedDoc => NodeKind::GeneratedDoc,
            Self::Attachment => NodeKind::Attachment,
        }
    }
}

/// Whether a file's bytes can be fetched (POL-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentAvailability {
    /// The bytes may be fetched.
    Fetchable,
    /// The source forbids serving the bytes — Telegram protected content.
    /// The item stays visible as an explicit "restricted by Telegram"
    /// placeholder; a fetch attempt fails with
    /// [`SourceError::Restricted`](crate::SourceError::Restricted) and the
    /// bytes never enter the archive (POL-4).
    Restricted,
}

/// What a file item's bytes are, as far as the source knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    /// Which structural file this is.
    pub kind: FileKind,
    /// Version of the bytes; fetches pin to it (DOM-003).
    pub content_version: ContentVersion,
    /// Logical size in bytes, when the source knows it. Generated documents
    /// report `None` until the renderer owns their bytes.
    pub size: Option<u64>,
    /// MIME type, when the source knows one.
    pub mime_type: Option<String>,
    /// Whether the bytes may be fetched at all.
    pub availability: ContentAvailability,
}

/// Directory structure or file facts — the enum that keeps "is a
/// directory" and "has bytes" from ever disagreeing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemContent {
    /// An enumerable directory with no bytes of its own.
    Directory(DirectoryKind),
    /// A file with byte-level facts and no children.
    File(FileFacts),
}

impl ItemContent {
    /// The tree vocabulary this content projects to.
    pub fn node_kind(&self) -> NodeKind {
        match self {
            Self::Directory(kind) => kind.node_kind(),
            Self::File(facts) => facts.kind.node_kind(),
        }
    }

    /// Whether this is directory content.
    pub fn is_directory(&self) -> bool {
        matches!(self, Self::Directory(_))
    }
}

/// One provider-visible item as served by a `DriveSource` (SYNC-001).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceItem {
    /// Stable opaque identity (DOM-001) — appearance identity below a view
    /// root, canonical above.
    pub id: ItemId,
    /// The parent appearance this item is enumerated under; `None` for the
    /// account root and only the account root.
    pub parent: Option<ItemId>,
    /// Raw display name. Sanitization and collision suffixing are the
    /// naming policy's job (`gramdrive_model::naming`), applied by the
    /// consumer over a sibling set — never by the source.
    pub display_name: String,
    /// Changes whenever provider-visible metadata or parent membership
    /// changes (DOM-003).
    pub metadata_version: MetadataVersion,
    /// Creation time in milliseconds since the Unix epoch (UTC), when the
    /// source knows it. Integer milliseconds by boundary rule — no OS time
    /// type crosses the contract.
    pub created_at_ms: Option<i64>,
    /// Last modification time in milliseconds since the Unix epoch (UTC),
    /// when the source knows it.
    pub modified_at_ms: Option<i64>,
    /// Directory structure or file facts.
    pub content: ItemContent,
}

impl SourceItem {
    /// What a provider may advertise for this item — derived, read-only in
    /// v1 by construction (DEC-007, SYNC-060).
    ///
    /// A restricted file advertises neither children nor readable content:
    /// it is a visible placeholder whose bytes cannot be requested (POL-4).
    pub fn capabilities(&self) -> Capabilities {
        match &self.content {
            ItemContent::Directory(_) => Capabilities::read_only_directory(),
            ItemContent::File(facts) => match facts.availability {
                ContentAvailability::Fetchable => Capabilities::read_only_file(),
                ContentAvailability::Restricted => Capabilities {
                    enumerate_children: false,
                    read_content: false,
                    write_content: false,
                    rename: false,
                    relocate: false,
                    delete: false,
                },
            },
        }
    }

    /// Whether this item is a directory.
    pub fn is_directory(&self) -> bool {
        self.content.is_directory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gramdrive_model::identity::{AccountId, AccountKey, CanonicalKey, ItemKey};

    fn root_id() -> ItemId {
        ItemKey::Canonical(CanonicalKey::Account(AccountKey {
            account_id: AccountId(7),
        }))
        .id()
    }

    fn file_item(availability: ContentAvailability) -> SourceItem {
        SourceItem {
            id: root_id(),
            parent: Some(root_id()),
            display_name: "photo.jpg".to_owned(),
            metadata_version: MetadataVersion::new("m1").unwrap(),
            created_at_ms: Some(1_700_000_000_000),
            modified_at_ms: None,
            content: ItemContent::File(FileFacts {
                kind: FileKind::Attachment,
                content_version: ContentVersion::new("c1").unwrap(),
                size: Some(1024),
                mime_type: Some("image/jpeg".to_owned()),
                availability,
            }),
        }
    }

    #[test]
    fn kind_bridges_partition_node_kind() {
        let directories = [
            (DirectoryKind::Root, NodeKind::Root),
            (DirectoryKind::ChatList, NodeKind::ChatList),
            (DirectoryKind::FolderCatalog, NodeKind::FolderCatalog),
            (DirectoryKind::Chat, NodeKind::Chat),
            (DirectoryKind::Year, NodeKind::Year),
            (DirectoryKind::Media, NodeKind::Media),
        ];
        for (kind, expected) in directories {
            assert_eq!(kind.node_kind(), expected);
            assert!(expected.is_directory(), "{expected:?} must be a directory");
        }
        let files = [
            (FileKind::GeneratedDoc, NodeKind::GeneratedDoc),
            (FileKind::Attachment, NodeKind::Attachment),
        ];
        for (kind, expected) in files {
            assert_eq!(kind.node_kind(), expected);
            assert!(!expected.is_directory(), "{expected:?} must be a file");
        }
    }

    #[test]
    fn directory_capabilities_enumerate_and_never_write() {
        let item = SourceItem {
            id: root_id(),
            parent: None,
            display_name: "Account".to_owned(),
            metadata_version: MetadataVersion::new("m1").unwrap(),
            created_at_ms: None,
            modified_at_ms: None,
            content: ItemContent::Directory(DirectoryKind::Root),
        };
        assert!(item.is_directory());
        let caps = item.capabilities();
        assert!(caps.enumerate_children);
        assert!(!caps.read_content);
        assert!(!caps.write_content && !caps.rename && !caps.relocate && !caps.delete);
    }

    #[test]
    fn fetchable_file_reads_and_never_writes() {
        let caps = file_item(ContentAvailability::Fetchable).capabilities();
        assert!(!caps.enumerate_children);
        assert!(caps.read_content);
        assert!(!caps.write_content && !caps.rename && !caps.relocate && !caps.delete);
    }

    #[test]
    fn restricted_file_advertises_nothing() {
        let item = file_item(ContentAvailability::Restricted);
        assert!(!item.is_directory());
        let caps = item.capabilities();
        assert!(!caps.enumerate_children);
        assert!(!caps.read_content);
        assert!(!caps.write_content && !caps.rename && !caps.relocate && !caps.delete);
    }
}
