//! Stable item identities (DEC-008, DOM-001..DOM-024).
//!
//! Every provider-visible item resolves through one opaque [`ItemId`]
//! namespace: Apple item identifiers and Android document IDs carry its text
//! form, Windows file identity payloads its binary form, and the Linux inode
//! table maps to it (DOM-024). An `ItemId` is the versioned serialization of
//! exactly one typed [`ItemKey`], and the typed keys are the source of truth.
//!
//! # Canonical vs appearance identity (DOM-002, DOM-022)
//!
//! [`CanonicalKey`] identifies a source-derived record: an account, a chat
//! list, a chat, a message, an attachment, a generated document, or a blob.
//! Canonical identity never changes when presentation changes.
//!
//! [`AppearanceKey`] identifies one *virtual appearance* of a canonical item:
//! the same canonical chat shown in Main, in Archive, and in a custom folder
//! is three appearances with three distinct `ItemId`s over one unchanged
//! canonical key (PRD-013). Moving a chat between views creates and removes
//! appearances; the canonical key is untouched. Appearances cannot nest — an
//! [`AppearanceKey`] wraps a [`CanonicalKey`], never another appearance.
//!
//! Which `(view, item)` combinations actually occur is the virtual tree
//! builder's discipline (TASK-260715-3tjduq); this layer guarantees only that
//! distinct keys are distinct identities.
//!
//! # No path, title, or order dependence (DOM-001, DOM-005)
//!
//! No key type in this module carries a string or an ordering position.
//! Titles, filenames, paths, and display order are derived presentation state
//! and *cannot* influence identity — by construction, not by convention. A
//! rename, a re-sort, or a path change is a metadata update on an item whose
//! identity is unchanged (SYNC-026).
//!
//! # Namespace scoping (DOM-021)
//!
//! Telegram-derived keys (chat lists, chats, messages, attachments, generated
//! documents) are scoped by [`AccountScope`]: the account plus its
//! [`NamespaceVersion`]. Telegram IDs are only meaningful within one
//! authorized account; bumping the namespace version (for example after the
//! device re-authorizes as a different Telegram user) retires every derived
//! identity of that account at once without renumbering the account itself.
//! Blob keys are content-derived, not Telegram-derived, so they scope to the
//! bare [`AccountKey`]: the bytes a hash names do not change when the
//! Telegram namespace epoch does.
//!
//! # Collision behavior
//!
//! Distinct keys can never serialize to the same `ItemId`: decoding is a
//! deterministic function and every key round-trips, so `encode(a) ==
//! encode(b)` forces `a == b`. The property suite
//! (`tests/identity_properties.rs`) proves the round-trip and samples
//! cross-kind pairs directly. The residual collision surface lives in the
//! *inputs*, not the encoding:
//!
//! - Two blobs collide only if two different byte streams share a SHA-256
//!   digest; [`BlobKey`] accepts the hash's collision resistance.
//! - Reusing a Telegram ID for a different object (re-login as another user)
//!   is namespace reuse, which is what the [`NamespaceVersion`] bump exists
//!   to prevent.
//! - Equal *display names* are not identity collisions; deterministic name
//!   suffixing is the naming policy's job (SYNC-012, TASK-260715-1ffbkg).
//!
//! The serialization format itself is documented in this crate's README and
//! implemented in the private `codec` module.

mod codec;

use codec::{decode_key, decode_text, encode_key, encode_text};

/// Stable identifier of one configured account (DOM-021).
///
/// Assignment is the state layer's concern; per DOM-020 it must be derivable
/// from source facts so a database rebuild from unchanged source data yields
/// the same value. This vocabulary only guarantees that distinct values are
/// distinct identities. `i64` covers Telegram user IDs (int53) and local
/// sequence counters alike.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountId(pub i64);

/// Identity-namespace epoch of one account (DOM-021).
///
/// Bumped when every Telegram-derived identity of the account must be
/// retired at once — for example after re-authorization as a different
/// Telegram user, when the same numeric IDs would otherwise name different
/// objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NamespaceVersion(pub u32);

/// Canonical identity of one configured account.
///
/// Deliberately excludes the namespace version: the account item itself
/// survives a namespace bump; only identities *derived* from its Telegram ID
/// space (see [`AccountScope`]) are retired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountKey {
    /// The account this key names.
    pub account_id: AccountId,
}

/// Scope of every Telegram-derived key: account plus namespace epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountScope {
    /// Owning account.
    pub account: AccountKey,
    /// Namespace epoch within that account.
    pub namespace_version: NamespaceVersion,
}

/// Telegram folder (chat filter) identifier — int32 in the Telegram schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FolderId(pub i32);

/// Which chat-list view an item is seen through (PRD-010).
///
/// Unscoped on purpose: inside an [`AppearanceKey`] the account scope comes
/// from the wrapped canonical item, so a view/item account mismatch is not
/// representable. A folder ID is interpreted within that item's account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChatListKind {
    /// The main chat list.
    Main,
    /// The archive.
    Archive,
    /// A custom Telegram folder.
    Folder(FolderId),
}

/// Canonical identity of one chat-list view root (Main, Archive, or a
/// custom folder) within an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChatListKey {
    /// Owning account and namespace epoch.
    pub scope: AccountScope,
    /// Which list this key names.
    pub kind: ChatListKind,
}

/// Telegram chat/peer identifier — int53, may be negative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChatId(pub i64);

/// Canonical identity of one chat (DOM-021).
///
/// Independent of the chat's title, list membership, and position: those are
/// appearance and presentation state (SYNC-026).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChatKey {
    /// Owning account and namespace epoch.
    pub scope: AccountScope,
    /// Telegram chat ID within that scope.
    pub chat_id: ChatId,
}

/// Telegram message identifier — int53 within its chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageId(pub i64);

/// Canonical identity of one message (DOM-021).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageKey {
    /// The chat the message belongs to.
    pub chat: ChatKey,
    /// Telegram message ID within that chat.
    pub message_id: MessageId,
}

/// Position of one attachment within its message's attachment list.
///
/// A GramDrive ordinal, not a Telegram field: it is assigned once when the
/// message record is first normalized and never re-derived from display
/// order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttachmentIndex(pub u32);

/// Canonical identity of one downloadable attachment (DOM-021, DOM-007).
///
/// Telegram remote locators and file references are refreshable source
/// metadata and deliberately absent: a reference refresh must never change
/// provider item identity (SYNC-045).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttachmentKey {
    /// The message the attachment belongs to.
    pub message: MessageKey,
    /// Ordinal within that message's attachments.
    pub index: AttachmentIndex,
}

/// Output format of a generated document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocFormat {
    /// Newline-delimited JSON message records.
    Ndjson,
    /// Rendered Markdown transcript.
    Markdown,
}

/// Record-schema family of a generated document (DOM-023).
///
/// Identifies the schema *lineage*, not its revision: compatible schema
/// revisions bump the document's content version, while a new family is a
/// different document with a different identity. Family numbers are assigned
/// by the rendering layer (STORY-260715-1oq9jg).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SchemaFamily(pub u16);

/// Bounded source range a generated document covers (DOM-023).
///
/// Values are not semantically validated here (a month of 13 is encodable);
/// which partitions exist is the tree builder's and renderer's discipline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocPartition {
    /// The whole chat.
    Chat,
    /// One calendar year.
    Year {
        /// Calendar year of the partition.
        year: u16,
    },
    /// One calendar month.
    Month {
        /// Calendar year of the partition.
        year: u16,
        /// Calendar month, 1-12 by convention.
        month: u8,
    },
}

/// Canonical identity of one generated NDJSON/Markdown document (DOM-023).
///
/// Includes chat identity, partition, format, and schema family — and, per
/// DOM-023, *not* the current chat title. Renderer version and input
/// watermark are content-version state, not identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GeneratedDocKey {
    /// The chat the document is rendered from.
    pub chat: ChatKey,
    /// Source range the document covers.
    pub partition: DocPartition,
    /// Output format.
    pub format: DocFormat,
    /// Record-schema family.
    pub schema_family: SchemaFamily,
}

/// Strong content hash naming a fully downloaded blob (DOM-021).
///
/// An enum for algorithm agility: a future algorithm is a new variant with a
/// new serialization tag, and old identities keep parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContentHash {
    /// SHA-256 digest of the complete content.
    Sha256([u8; 32]),
}

/// Canonical identity of one materialized blob.
///
/// Content-addressed within one account: scoping to the account keeps one
/// account's holdings unobservable from another's, and the scope is the bare
/// [`AccountKey`] because content identity is orthogonal to the Telegram
/// namespace epoch. Blob identity never replaces attachment identity; partial
/// downloads live under transfer IDs and are not blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobKey {
    /// Owning account.
    pub account: AccountKey,
    /// Content hash of the complete bytes.
    pub hash: ContentHash,
}

/// Canonical, view-independent identity of a source-derived record
/// (DOM-002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanonicalKey {
    /// A configured account.
    Account(AccountKey),
    /// A chat-list view root (Main, Archive, or a custom folder).
    ChatList(ChatListKey),
    /// A chat.
    Chat(ChatKey),
    /// A message.
    Message(MessageKey),
    /// A downloadable attachment.
    Attachment(AttachmentKey),
    /// A generated NDJSON/Markdown document.
    GeneratedDoc(GeneratedDocKey),
    /// A content-addressed blob.
    Blob(BlobKey),
}

/// One virtual appearance of a canonical item (DOM-002, DOM-022).
///
/// The same canonical item viewed through different chat lists yields
/// distinct appearance identities; see the module docs for the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AppearanceKey {
    /// The chat-list view the item appears through, interpreted within the
    /// item's account scope.
    pub view: ChatListKind,
    /// The canonical item that appears.
    pub item: CanonicalKey,
}

/// Any key an [`ItemId`] can carry: canonical or appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKey {
    /// A canonical, view-independent identity.
    Canonical(CanonicalKey),
    /// One virtual appearance of a canonical item.
    Appearance(AppearanceKey),
}

impl From<CanonicalKey> for ItemKey {
    fn from(key: CanonicalKey) -> Self {
        Self::Canonical(key)
    }
}

impl From<AppearanceKey> for ItemKey {
    fn from(key: AppearanceKey) -> Self {
        Self::Appearance(key)
    }
}

impl ItemKey {
    /// Serializes this key into its opaque provider identity.
    pub fn id(&self) -> ItemId {
        ItemId {
            key: *self,
            bytes: encode_key(self),
        }
    }
}

/// Opaque, versioned provider identity (DOM-001, DOM-020, DOM-024).
///
/// The value handed to and received from every native provider. Opaque means
/// consumers must not interpret the content — the encoding is nevertheless
/// specified (crate README) and frozen per format version, which is what
/// makes the identity stable across process restarts and app updates.
///
/// Both constructors validate fully: an `ItemId` that exists decodes to a
/// well-formed key, so [`ItemId::key`] is infallible. The parser accepts
/// exactly the canonical encodings — one valid byte string and one valid text
/// string per key, byte-for-byte.
#[derive(Debug, Clone)]
pub struct ItemId {
    key: ItemKey,
    bytes: Vec<u8>,
}

impl ItemId {
    /// The typed key this identity serializes.
    pub fn key(&self) -> ItemKey {
        self.key
    }

    /// Binary form — Windows file identity payloads (DOM-024).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Text form — Apple item identifiers and Android document IDs
    /// (DOM-024): `"gd"` followed by unpadded lowercase base32 of
    /// [`ItemId::as_bytes`].
    pub fn text(&self) -> String {
        encode_text(&self.bytes)
    }

    /// Parses and validates the binary form.
    pub fn parse_bytes(bytes: &[u8]) -> Result<Self, IdParseError> {
        let key = decode_key(bytes)?;
        Ok(Self {
            key,
            bytes: bytes.to_vec(),
        })
    }

    /// Parses and validates the text form.
    pub fn parse_text(text: &str) -> Result<Self, IdParseError> {
        Self::parse_bytes(&decode_text(text)?)
    }
}

impl From<ItemKey> for ItemId {
    fn from(key: ItemKey) -> Self {
        key.id()
    }
}

impl std::fmt::Display for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

// Equality and hashing use the canonical bytes alone; `key` is derived from
// them bijectively, so comparing it too would be redundant work, and a
// manual `Hash` next to a derived `PartialEq` (or vice versa) risks the
// classic eq/hash mismatch.
impl PartialEq for ItemId {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for ItemId {}

impl std::hash::Hash for ItemId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.bytes.hash(state);
    }
}

/// Why a byte string or text string is not a valid [`ItemId`].
///
/// Structured for diagnostics, not for recovery: a provider handing back an
/// unparseable identity is either data corruption or a version skew, and the
/// caller's job is to report which.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdParseError {
    /// The format version byte is not one this build understands — most
    /// likely an identity minted by a newer app version.
    UnsupportedVersion {
        /// The version byte found.
        version: u8,
    },
    /// A tag byte has no meaning in its position.
    UnknownTag {
        /// The tag byte found.
        tag: u8,
        /// Which field the tag was read for.
        field: &'static str,
    },
    /// The payload ended before the key was complete.
    Truncated,
    /// Bytes remained after a complete key was decoded.
    TrailingBytes {
        /// How many bytes were left over.
        extra: usize,
    },
    /// The text form does not start with the `"gd"` prefix.
    MissingPrefix,
    /// The text form contains a byte outside the lowercase base32 alphabet.
    InvalidCharacter {
        /// Byte offset of the offending character within the full input.
        position: usize,
    },
    /// The text form is not the canonical base32 encoding of any byte
    /// string (invalid length residue or nonzero padding bits).
    NonCanonicalText,
}

impl std::fmt::Display for IdParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion { version } => {
                write!(f, "unsupported item id format version {version}")
            }
            Self::UnknownTag { tag, field } => {
                write!(f, "unknown tag {tag:#04x} for {field}")
            }
            Self::Truncated => f.write_str("item id payload is truncated"),
            Self::TrailingBytes { extra } => {
                write!(f, "item id payload has {extra} trailing byte(s)")
            }
            Self::MissingPrefix => f.write_str("item id text lacks the 'gd' prefix"),
            Self::InvalidCharacter { position } => {
                write!(f, "invalid base32 character at byte {position}")
            }
            Self::NonCanonicalText => f.write_str("item id text is not canonical base32"),
        }
    }
}

impl std::error::Error for IdParseError {}
