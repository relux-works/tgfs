//! Ordering projection (TASK-260715-1jmsdp; POL-1, DEC-013, SYNC-011).
//!
//! Telegram dialogs have an exact order. Filesystems sort by name. POL-1
//! resolves the mismatch by *not* fighting it: folder names stay stable, and
//! the exact order is published as data — [`ORDER_FILE_NAME`] at each chat-list
//! root (Main, Archive, and every custom folder), regenerated whenever a
//! position changes.
//!
//! [`OrderProjection`] is that document's model: positions in, a deterministic
//! `order.json` out.
//!
//! # Reordering is a content change, never a rename (DEC-013)
//!
//! This is the whole point of the projection, so it is worth stating as the
//! invariant it is: when only positions change, every [`ItemId`] in the
//! account, every folder name, and therefore every path is untouched — the one
//! thing that changes is the bytes of `order.json`. Nothing renames, nothing
//! moves, and no cached content is invalidated, because none of it is keyed by
//! order (`crate::identity`). Folder names change on exactly one input: the
//! chat's own title or username changing in Telegram (`chat_folder_name`).
//!
//! # One projection mode, not two (DEC-013)
//!
//! Earlier drafts (SYNC-011, PRD-012) floated a numeric-prefix mode —
//! `001 — Alex/` — as a configurable alternative. DEC-013 settled on stable
//! names only for v1, so there is no mode switch here, no prefix renderer, and
//! no migration between modes: a mode that ships disabled is untested code that
//! reads as a supported feature. Revisiting it post-v1 means a new decision row
//! and a new projection mode beside this one; it does not mean re-enabling
//! something dormant.
//!
//! # Ordering rule (Telegram `chatPosition`)
//!
//! Chats sort by `(order, chat_id)` descending — Telegram's own rule, and a
//! total order because chat IDs are unique within a list. Total is what makes
//! the output a pure function of the input *set*: shuffled input yields a
//! byte-identical document, with no tie left for input order to settle.
//!
//! Note that [`ChatPosition::is_pinned`] is *not* a sort key. Telegram already
//! encodes pinning in `order` (pinned chats carry the top values); sorting by
//! it again would be a second, disagreeing implementation of the server's
//! ranking. It is recorded because the pinned/unpinned boundary is not
//! recoverable from the sequence alone, and the app's UI draws it.

use crate::identity::{
    CanonicalKey, ChatId, ChatKey, ChatListKey, ChatListKind, ItemId, ItemKey, OrderDocKey,
    SchemaFamily,
};
use crate::naming::{NameKind, SafeName, SiblingName, chat_folder_name, resolve_siblings};

/// Name of the ordering document at every chat-list root (POL-1).
///
/// Fixed, and reserved against chat titles: a chat titled `order.json` is
/// suffixed rather than allowed to shadow the metadata
/// ([`SiblingName::fixed`]).
pub const ORDER_FILE_NAME: &str = "order.json";

/// `schema` marker written into every ordering document.
const SCHEMA_ID: &str = "gramdrive.order";

/// Where one chat sits in one chat list — the snapshot of Telegram's
/// `chatPosition` this projection consumes.
///
/// Provider-neutral by construction (DEC-003): an `i64` rank and a flag, with
/// no Telegram type behind them. A source adapts `chatPosition` into this; the
/// model never learns what TDLib is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatPosition {
    /// Server-assigned rank within the list. Opaque and non-contiguous —
    /// compared, never interpreted or recomputed.
    pub order: i64,
    /// Whether the chat is pinned in this list. Recorded, not sorted by; see
    /// the module docs.
    pub is_pinned: bool,
}

/// One chat's input to the projection: what it is called and where it sits.
///
/// Identifier-level like [`crate::tree::ChatRecord`], and for the same reason:
/// the projection derives every key from its own list scope, so a record from
/// another account is not representable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatOrderRecord {
    /// Telegram chat identifier.
    pub chat_id: ChatId,
    /// Raw chat title.
    pub title: String,
    /// Public username, when the chat has one (POL-1 name component).
    pub username: Option<String>,
    /// Position of the chat in this list.
    pub position: ChatPosition,
}

/// One chat's resolved place in the published order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderEntry {
    /// 0-based position in the list, after sorting. This — not
    /// [`ChatPosition::order`] — is the order a reader should use.
    pub rank: usize,
    /// Canonical identity of the chat.
    pub chat: ChatKey,
    /// Identity of the chat's appearance in this list — the [`ItemId`] a
    /// provider enumerates, so a reader can join the order to the tree
    /// without guessing.
    pub id: ItemId,
    /// The chat's folder name as actually projected: sanitized, and
    /// collision-suffixed against its siblings.
    pub name: SafeName,
    /// The position this entry was derived from.
    pub position: ChatPosition,
}

/// Why input records cannot form an ordering projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderInputError {
    /// Two records describe one chat. A chat holds one position per list, so
    /// two are a source-normalization bug, and picking a winner here would
    /// silently publish an order that depends on input order.
    DuplicateChat {
        /// The duplicated chat ID.
        chat: ChatId,
    },
}

impl std::fmt::Display for OrderInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateChat { chat } => {
                write!(f, "duplicate position record for chat {}", chat.0)
            }
        }
    }
}

impl std::error::Error for OrderInputError {}

/// A deterministic ordering snapshot of one chat-list root.
///
/// Immutable once built. A reorder means building a new projection and
/// rewriting `order.json`; it never mutates this one, and never touches
/// identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderProjection {
    list: ChatListKey,
    schema_family: SchemaFamily,
    entries: Vec<OrderEntry>,
}

impl OrderProjection {
    /// Builds the ordering projection of one chat list.
    ///
    /// Record order never influences the result: entries sort by the total
    /// `(order, chat_id)` descending rule, and names resolve from the sibling
    /// *set*. Duplicate chats are an input contract violation and fail loudly.
    pub fn new(
        list: ChatListKey,
        schema_family: SchemaFamily,
        chats: Vec<ChatOrderRecord>,
    ) -> Result<Self, OrderInputError> {
        let mut records = chats;
        records.sort_by(|left, right| {
            right
                .position
                .order
                .cmp(&left.position.order)
                .then(right.chat_id.0.cmp(&left.chat_id.0))
        });
        // Checked against a set, not against the neighbour: the sort key
        // starts with `order`, so two records for one chat are only adjacent
        // when nothing else sorts between them. Scanning after the sort keeps
        // the reported chat independent of input order.
        let mut seen = std::collections::BTreeSet::new();
        for record in &records {
            if !seen.insert(record.chat_id.0) {
                return Err(OrderInputError::DuplicateChat {
                    chat: record.chat_id,
                });
            }
        }

        // Names resolve over the whole sibling set of the list root, which
        // includes order.json itself: the document has to name the directories
        // that are really there, suffixes and all, or it is a map to paths that
        // do not exist.
        let doc_id = Self::doc_id_of(list, schema_family);
        let chat_ids: Vec<ItemId> = records
            .iter()
            .map(|record| chat_appearance_id(list, record.chat_id))
            .collect();
        let raw_names: Vec<String> = records
            .iter()
            .map(|record| chat_folder_name(&record.title, record.username.as_deref()))
            .collect();

        let mut siblings = Vec::with_capacity(records.len() + 1);
        siblings.push(SiblingName {
            id: &doc_id,
            raw: ORDER_FILE_NAME,
            kind: NameKind::File,
            fixed: true,
        });
        siblings.extend(
            chat_ids
                .iter()
                .zip(raw_names.iter())
                .map(|(id, raw)| SiblingName {
                    id,
                    raw,
                    kind: NameKind::Directory,
                    fixed: false,
                }),
        );
        // Positional. The fixed document name was pushed first, so skipping
        // one puts the rest back in step with `records` and `chat_ids`.
        let names = resolve_siblings(&siblings).into_iter().skip(1);

        let entries = records
            .into_iter()
            .zip(chat_ids)
            .zip(names)
            .enumerate()
            .map(|(rank, ((record, id), name))| OrderEntry {
                rank,
                chat: chat_key(list, record.chat_id),
                id,
                name,
                position: record.position,
            })
            .collect();

        Ok(Self {
            list,
            schema_family,
            entries,
        })
    }

    /// The chat-list root this projection describes.
    pub fn list(&self) -> ChatListKey {
        self.list
    }

    /// Canonical identity of the `order.json` document.
    pub fn doc_key(&self) -> OrderDocKey {
        OrderDocKey {
            list: self.list,
            schema_family: self.schema_family,
        }
    }

    /// Identity of the `order.json` document — stable across every reorder.
    pub fn doc_id(&self) -> ItemId {
        Self::doc_id_of(self.list, self.schema_family)
    }

    /// The published order, rank 0 first.
    pub fn entries(&self) -> &[OrderEntry] {
        &self.entries
    }

    /// Renders `order.json` (SYNC-011).
    ///
    /// A pure function of the projection: no timestamps, no host state, no
    /// map iteration — equal projections render byte-identical documents, so a
    /// sync that changed nothing rewrites nothing. The schema is documented in
    /// this crate's README.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str("  \"schema\": ");
        write_json_string(&mut out, SCHEMA_ID);
        out.push_str(",\n");
        out.push_str(&format!("  \"schema_family\": {},\n", self.schema_family.0));
        out.push_str("  \"list\": ");
        self.write_list(&mut out);
        out.push_str(",\n");
        out.push_str("  \"chats\": [");
        if self.entries.is_empty() {
            out.push_str("]\n}\n");
            return out;
        }
        out.push('\n');
        for (index, entry) in self.entries.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!("      \"rank\": {},\n", entry.rank));
            out.push_str("      \"id\": ");
            write_json_string(&mut out, &entry.id.text());
            out.push_str(",\n");
            out.push_str(&format!("      \"chat_id\": {},\n", entry.chat.chat_id.0));
            out.push_str("      \"name\": ");
            write_json_string(&mut out, entry.name.as_str());
            out.push_str(",\n");
            // A string, not a number: `order` is int64, and a JSON number is
            // an IEEE-754 double to most parsers (jq, JavaScript), which
            // silently rounds the top-of-range values Telegram gives pinned
            // chats — two distinct pinned chats can compare equal after the
            // round trip. `chat_id` above is int53 by Telegram's own schema
            // and stays a number, where no such loss exists.
            out.push_str("      \"order\": ");
            write_json_string(&mut out, &entry.position.order.to_string());
            out.push_str(",\n");
            out.push_str(&format!("      \"pinned\": {}\n", entry.position.is_pinned));
            out.push_str("    }");
            if index + 1 < self.entries.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  ]\n}\n");
        out
    }

    fn write_list(&self, out: &mut String) {
        match self.list.kind {
            ChatListKind::Main => out.push_str("{ \"kind\": \"main\" }"),
            ChatListKind::Archive => out.push_str("{ \"kind\": \"archive\" }"),
            ChatListKind::Folder(folder) => out.push_str(&format!(
                "{{ \"kind\": \"folder\", \"folder_id\": {} }}",
                folder.0
            )),
        }
    }

    fn doc_id_of(list: ChatListKey, schema_family: SchemaFamily) -> ItemId {
        ItemKey::Canonical(CanonicalKey::OrderDoc(OrderDocKey {
            list,
            schema_family,
        }))
        .id()
    }
}

fn chat_key(list: ChatListKey, chat_id: ChatId) -> ChatKey {
    ChatKey {
        scope: list.scope,
        chat_id,
    }
}

fn chat_appearance_id(list: ChatListKey, chat_id: ChatId) -> ItemId {
    ItemKey::Appearance(crate::identity::AppearanceKey {
        view: list.kind,
        item: CanonicalKey::Chat(chat_key(list, chat_id)),
    })
    .id()
}

/// Writes `value` as a JSON string literal (RFC 8259 §7).
///
/// Hand-rolled for the reason the identity codec's base32 is
/// (`crate::identity`): the whole requirement is one escaping rule, and a
/// dependency would be more supply-chain surface (POL-6) than code. Rust's
/// `&str` is valid UTF-8, so the lone-surrogate case that makes JSON escaping
/// genuinely hard cannot arise here; what is left is the two mandatory
/// escapes and the C0 controls. The property suite pins it against the
/// grammar.
fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // Every other C0 control has no short escape and must not appear
            // raw. \u00xx is the general form; DEL (0x7f) is legal raw and is
            // deliberately not escaped.
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_mandatory_and_control_characters() {
        let mut out = String::new();
        write_json_string(&mut out, "a\"b\\c\nd\te\u{01}f");
        assert_eq!(out, r#""a\"b\\c\nd\te\u0001f""#);
    }

    #[test]
    fn leaves_unicode_and_del_raw() {
        let mut out = String::new();
        write_json_string(&mut out, "Привет 👨‍👩‍👧 \u{7f}");
        assert_eq!(out, "\"Привет 👨‍👩‍👧 \u{7f}\"");
    }
}
