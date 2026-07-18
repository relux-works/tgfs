//! The live chat-metadata/list update mapper: TDLib's push updates become a
//! deterministic, provider-neutral normalized change stream
//! (TASK-260715-1c8fea, SYNC-026).
//!
//! # Where it sits
//!
//! The [`SnapshotMachine`](crate::snapshot) bootstraps the baseline — every
//! chat's canonical metadata and every list's exact membership and order.
//! From then on TDLib pushes deltas: a renamed chat, a new avatar, a chat
//! pinned or reordered, a chat that left a list. [`UpdateMachine`] folds those
//! pushes into the same normalized vocabulary the snapshot commits in, so the
//! composing caller keeps the state layer current with the same
//! `upsert_chat` / list-membership writes — never a second, disagreeing
//! projection.
//!
//! # Shape: a sans-IO reducer
//!
//! Unlike the snapshot, this machine issues no requests and owns no client.
//! It is a pure reducer:
//!
//! 1. Feed every update from the client's
//!    [`UpdateStream`](crate::runtime::UpdateStream) to
//!    [`UpdateMachine::on_update`]. It consumes `updateNewChat`,
//!    `updateChatTitle`, `updateChatPhoto`, `updateChatPosition`,
//!    `updateChatRemovedFromList`, `updateChatHasProtectedContent`,
//!    `updateUser`, and `updateSupergroup`; everything else is ignored.
//! 2. Drain the accumulated normalized changes with
//!    [`UpdateMachine::take_batch`] and apply the [`UpdateBatch`] to the state
//!    layer in one transaction — the *transactional checkpoint*: canonical
//!    chats upserted first (the `chat_list_entries → chats` foreign key), then
//!    memberships. `take_batch` clears what it hands out, so the next drain
//!    reports only what changed since.
//!
//! Everything the machine emits is typed, provider-neutral vocabulary — no
//! TDLib JSON crosses outward (the DEC-003 direction the auth and snapshot
//! machines set).
//!
//! # Convergence, duplicates, and out-of-order delivery
//!
//! Every observed field is a full value, applied last-write-wins over the
//! machine's known state. A value equal to what is already known changes
//! nothing and produces no output, so a duplicated or replayed update is a
//! no-op, and re-driving a fixture converges to one result. Because the state
//! writes the caller makes are idempotent upserts, a process restart — a fresh
//! machine fed TDLib's re-pushed update burst — converges to the same rows;
//! the caller advances `metadata_version` only when the emitted facts actually
//! differ from the stored ones, so a restart re-emit is a true no-op and the
//! SYNC-003 enumeration anchor stays stable.
//!
//! # Provider invalidation (POL-1)
//!
//! Each change carries what it invalidates downstream, and the split is
//! POL-1's:
//!
//! - A **reorder** — a position or pin change — is a content change, never a
//!   rename: it emits only [`Invalidation::ListOrdering`], the signal to
//!   regenerate that list's `order.json`. No folder moves, no chat identity
//!   changes.
//! - A **rename** — a known chat's title or username changing — moves the
//!   chat's stable folder name, so it emits [`Invalidation::FolderName`].
//! - Other canonical metadata (a first sighting, an avatar, the
//!   protected-content flag) emits [`Invalidation::Metadata`]: the chat's
//!   metadata version advances, but no folder name and no list order does.
//!
//! # Gaps and recovery (SYNC-003/023)
//!
//! TDLib pushes `updateNewChat` before any update about a chat, so a metadata
//! or position update names a chat this machine already knows. An update that
//! names an *unknown* chat is a gap: the machine cannot forge a canonical row
//! (there is no chat type to trust, and the `chat_list_entries → chats`
//! foreign key would reject a membership anyway), so it drops the value and
//! reports the chat in [`UpdateBatch::unresolved`]. The caller resolves it
//! with `getChat` and feeds the returned chat object back through
//! [`UpdateMachine::on_update`] as an `updateNewChat` — which carries the
//! chat's current title, avatar, and positions, so nothing is lost — or
//! re-baselines through the snapshot (SYNC-023). There is deliberately no
//! server-side resume token: the live stream has no offset (as `loadChats`
//! has none), and durability is idempotent convergence plus snapshot
//! re-baselining, not a token that would only pretend to resume.
//!
//! # Boundaries
//!
//! Secret chats are out of v1 scope (POL-4/DEC-016) and a chat type this build
//! does not know fails safe the same way: both are remembered as excluded, so
//! their updates are neither emitted nor mistaken for gaps. Whether a chat that
//! left every list becomes a POL-3 tombstone is the composing engine's
//! retention decision; this layer reports the observable — the chat left the
//! list — and leaves the canonical row in place (SYNC-026).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::Value;

use gramdrive_model::identity::ChatListKind;

use crate::snapshot::SnapshotChatKind;
use crate::wire::{KindFact, active_username, parse_chat_kind, parse_int64, parse_list};

/// Canonical metadata of one chat as the live mapper last observed it — the
/// facts the caller upserts as the chat's canonical record (SYNC-026: identity
/// is the chat id; everything here is replaceable metadata).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMetadata {
    /// Telegram chat id (int53).
    pub chat_id: i64,
    /// Chat flavor (shared with the snapshot's vocabulary).
    pub kind: SnapshotChatKind,
    /// Current title as observed.
    pub title: String,
    /// Current public username, when the owning user/supergroup carries one.
    pub username: Option<String>,
    /// Telegram's protected-content flag (POL-4).
    pub is_protected: bool,
    /// Opaque token of the chat's current avatar. GramDrive v1 does not
    /// persist avatars, so there is no column for it; the token exists so a
    /// changed avatar advances the chat's metadata version (DOM-003) and
    /// re-renders the chat's metadata. `None` when the chat has no photo.
    pub photo: Option<String>,
}

/// One chat's membership change in one list — the incremental counterpart of a
/// snapshot's `ListEntrySnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipChange {
    /// The chat is (now) a member of `list` at this exact position; the caller
    /// upserts the membership row.
    Set {
        /// The list the chat is a member of.
        list: ChatListKind,
        /// The member chat.
        chat_id: i64,
        /// Telegram's opaque sort position — larger sorts first.
        sort_order: i64,
        /// Whether the chat is pinned in this list.
        pinned: bool,
    },
    /// The chat left `list` (an order-0 position or `updateChatRemovedFromList`);
    /// the caller removes the membership row.
    Removed {
        /// The list the chat left.
        list: ChatListKind,
        /// The chat that left.
        chat_id: i64,
    },
}

/// What a normalized change invalidates downstream (POL-1). See the module
/// docs for the reorder/rename split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalidation {
    /// A known chat's stable folder name changed (its title or username) — its
    /// folder must be renamed. A pure reorder never emits this.
    FolderName {
        /// The renamed chat.
        chat_id: i64,
    },
    /// A list's membership or order changed — regenerate its `order.json`, and
    /// nothing else (POL-1: a reorder is content, never a rename).
    ListOrdering {
        /// The list whose order changed.
        list: ChatListKind,
    },
    /// A chat's other canonical metadata changed (first sight, avatar, or the
    /// protected-content flag) — its metadata version advances, but no folder
    /// name and no list order does.
    Metadata {
        /// The chat whose metadata changed.
        chat_id: i64,
    },
}

/// One drained checkpoint of normalized changes, applied to the state layer in
/// one transaction (module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpdateBatch {
    /// Canonical chats whose metadata changed — the caller upserts each,
    /// ascending by chat id.
    pub chats: Vec<ChatMetadata>,
    /// Membership changes, in deterministic `(list, chat id)` order. Apply
    /// after `chats` so an added chat's canonical row exists first.
    pub memberships: Vec<MembershipChange>,
    /// The provider invalidations these changes imply (POL-1), deterministic.
    pub invalidations: Vec<Invalidation>,
    /// Chats named by updates the machine has no metadata for — gaps
    /// (SYNC-003/023) whose values were dropped. Resolve with `getChat` and
    /// feed the result back as `updateNewChat`, or re-baseline. Ascending.
    pub unresolved: Vec<i64>,
}

impl UpdateBatch {
    /// Whether the batch carries nothing to apply — a checkpoint over updates
    /// that changed no observed state (the duplicate/replay case).
    pub fn is_empty(&self) -> bool {
        self.chats.is_empty()
            && self.memberships.is_empty()
            && self.invalidations.is_empty()
            && self.unresolved.is_empty()
    }
}

/// One chat's last-observed position in one list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListPosition {
    sort_order: i64,
    pinned: bool,
}

/// Canonical facts of one chat as last observed by the mapper.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChatFacts {
    kind: KindFact,
    title: String,
    is_protected: bool,
    photo: Option<String>,
    /// The chat's resolved username, recomputed from the peer objects — kept
    /// on the facts so a metadata diff sees a username change directly.
    username: Option<String>,
}

/// The deterministic sans-IO live chat-metadata/list update mapper for one
/// authorized account's client (module docs).
#[derive(Debug, Default)]
pub struct UpdateMachine {
    /// Canonical facts of every known chat, including excluded flavors.
    facts: BTreeMap<i64, ChatFacts>,
    /// Present list memberships: `chat -> list -> position`. Absence means the
    /// chat is not a member; an order-0 position removes the entry.
    positions: BTreeMap<i64, HashMap<ChatListKind, ListPosition>>,
    /// Last-observed username of each user, for private-chat name resolution.
    user_names: HashMap<i64, Option<String>>,
    /// Last-observed username of each supergroup/channel.
    supergroup_names: HashMap<i64, Option<String>>,
    /// The private chat of a user id, for O(1) username propagation.
    chat_of_user: HashMap<i64, i64>,
    /// The chat of a supergroup/channel id, for O(1) username propagation.
    chat_of_supergroup: HashMap<i64, i64>,
    /// Chats whose canonical metadata changed since the last drain.
    dirty_meta: BTreeSet<i64>,
    /// The subset of `dirty_meta` whose name changed — a folder rename rather
    /// than a plain metadata refresh.
    renamed: HashSet<i64>,
    /// `(list, chat)` memberships that changed since the last drain.
    dirty_memberships: HashSet<(ChatListKind, i64)>,
    /// Lists whose membership or order changed since the last drain.
    dirty_lists: HashSet<ChatListKind>,
    /// Unknown chats named by updates since the last drain (gaps).
    gaps: BTreeSet<i64>,
}

impl UpdateMachine {
    /// A fresh mapper that has observed nothing yet.
    pub fn new() -> UpdateMachine {
        UpdateMachine::default()
    }

    /// Whether a [`UpdateMachine::take_batch`] would return anything — a cheap
    /// check before opening a write transaction.
    pub fn has_pending(&self) -> bool {
        !self.dirty_meta.is_empty()
            || !self.dirty_memberships.is_empty()
            || !self.dirty_lists.is_empty()
            || !self.gaps.is_empty()
    }

    /// Feed one update from the client's stream (module docs). Unrecognized
    /// and structurally malformed updates are ignored; the response-free
    /// design means the safety net is the caller's `getChat` re-resolution,
    /// not a strict parse here.
    pub fn on_update(&mut self, update: &Value) {
        match update.get("@type").and_then(Value::as_str) {
            Some("updateNewChat") => {
                if let Some(chat) = update.get("chat") {
                    self.ingest_chat(chat);
                }
            }
            Some("updateChatTitle") => {
                if let (Some(chat_id), Some(title)) = (
                    update.get("chat_id").and_then(Value::as_i64),
                    update.get("title").and_then(Value::as_str),
                ) {
                    self.on_title(chat_id, title);
                }
            }
            Some("updateChatPhoto") => {
                if let Some(chat_id) = update.get("chat_id").and_then(Value::as_i64) {
                    self.on_photo(chat_id, photo_token(update.get("photo")));
                }
            }
            Some("updateChatHasProtectedContent") => {
                if let (Some(chat_id), Some(is_protected)) = (
                    update.get("chat_id").and_then(Value::as_i64),
                    update.get("has_protected_content").and_then(Value::as_bool),
                ) {
                    self.on_protection(chat_id, is_protected);
                }
            }
            Some("updateChatPosition") => {
                if let Some(chat_id) = update.get("chat_id").and_then(Value::as_i64)
                    && let Some(position) = update.get("position")
                {
                    self.on_position(chat_id, position);
                }
            }
            Some("updateChatRemovedFromList") => {
                if let Some(chat_id) = update.get("chat_id").and_then(Value::as_i64)
                    && let Some(list) = update.get("chat_list").and_then(parse_list)
                {
                    self.on_removed_from_list(chat_id, list);
                }
            }
            Some("updateUser") => {
                if let Some(user) = update.get("user")
                    && let Some(user_id) = user.get("id").and_then(Value::as_i64)
                {
                    self.on_user_name(user_id, active_username(user));
                }
            }
            Some("updateSupergroup") => {
                if let Some(supergroup) = update.get("supergroup")
                    && let Some(id) = supergroup.get("id").and_then(Value::as_i64)
                {
                    self.on_supergroup_name(id, active_username(supergroup));
                }
            }
            _ => {}
        }
    }

    /// Drain the normalized changes accumulated since the last call, and clear
    /// them — the transactional checkpoint (module docs).
    pub fn take_batch(&mut self) -> UpdateBatch {
        let mut chats = Vec::with_capacity(self.dirty_meta.len());
        let mut invalidations = Vec::new();
        for &chat_id in &self.dirty_meta {
            let Some(facts) = self.facts.get(&chat_id) else {
                continue;
            };
            let Some(kind) = emittable_kind(&facts.kind) else {
                continue;
            };
            chats.push(ChatMetadata {
                chat_id,
                kind,
                title: facts.title.clone(),
                username: facts.username.clone(),
                is_protected: facts.is_protected,
                photo: facts.photo.clone(),
            });
            if self.renamed.contains(&chat_id) {
                invalidations.push(Invalidation::FolderName { chat_id });
            } else {
                invalidations.push(Invalidation::Metadata { chat_id });
            }
        }

        let mut memberships: Vec<MembershipChange> = self
            .dirty_memberships
            .iter()
            .map(|&(list, chat_id)| {
                match self.positions.get(&chat_id).and_then(|map| map.get(&list)) {
                    Some(position) => MembershipChange::Set {
                        list,
                        chat_id,
                        sort_order: position.sort_order,
                        pinned: position.pinned,
                    },
                    None => MembershipChange::Removed { list, chat_id },
                }
            })
            .collect();
        memberships.sort_by_key(|change| {
            let (list, chat_id) = match change {
                MembershipChange::Set { list, chat_id, .. }
                | MembershipChange::Removed { list, chat_id } => (*list, *chat_id),
            };
            (list_sort_key(list), chat_id)
        });

        let mut dirty_lists: Vec<ChatListKind> = self.dirty_lists.iter().copied().collect();
        dirty_lists.sort_by_key(|list| list_sort_key(*list));
        for list in dirty_lists {
            invalidations.push(Invalidation::ListOrdering { list });
        }

        let unresolved: Vec<i64> = self.gaps.iter().copied().collect();

        self.dirty_meta.clear();
        self.renamed.clear();
        self.dirty_memberships.clear();
        self.dirty_lists.clear();
        self.gaps.clear();

        UpdateBatch {
            chats,
            memberships,
            invalidations,
            unresolved,
        }
    }

    // -- update handlers ----------------------------------------------------

    fn ingest_chat(&mut self, chat: &Value) {
        let Some(chat_id) = chat.get("id").and_then(Value::as_i64) else {
            return;
        };
        let kind = match chat.get("type") {
            Some(kind) => parse_chat_kind(kind),
            None => KindFact::Unsupported,
        };
        let title = chat
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let is_protected = chat
            .get("has_protected_content")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let photo = photo_token(chat.get("photo"));
        let username = self.resolve_username(&kind);
        match &kind {
            KindFact::Private { user_id } => {
                self.chat_of_user.insert(*user_id, chat_id);
            }
            KindFact::Supergroup { supergroup_id } | KindFact::Channel { supergroup_id } => {
                self.chat_of_supergroup.insert(*supergroup_id, chat_id);
            }
            _ => {}
        }
        let emittable = emittable_kind(&kind).is_some();
        let new_facts = ChatFacts {
            kind,
            title,
            is_protected,
            photo,
            username,
        };
        match self.facts.get(&chat_id) {
            None => {
                if emittable {
                    // First sight: a creation, not a rename.
                    self.dirty_meta.insert(chat_id);
                }
            }
            Some(old) if *old != new_facts => {
                if emittable {
                    self.dirty_meta.insert(chat_id);
                    if old.title != new_facts.title || old.username != new_facts.username {
                        self.renamed.insert(chat_id);
                    }
                }
            }
            Some(_) => {}
        }
        self.facts.insert(chat_id, new_facts);
        self.gaps.remove(&chat_id);
        if let Some(positions) = chat.get("positions").and_then(Value::as_array) {
            for position in positions {
                self.on_position(chat_id, position);
            }
        }
    }

    fn on_title(&mut self, chat_id: i64, title: &str) {
        match self.facts.get_mut(&chat_id) {
            Some(facts) => {
                if emittable_kind(&facts.kind).is_some() && facts.title != title {
                    facts.title = title.to_owned();
                    self.dirty_meta.insert(chat_id);
                    self.renamed.insert(chat_id);
                }
            }
            None => {
                self.gaps.insert(chat_id);
            }
        }
    }

    fn on_photo(&mut self, chat_id: i64, photo: Option<String>) {
        match self.facts.get_mut(&chat_id) {
            Some(facts) => {
                if emittable_kind(&facts.kind).is_some() && facts.photo != photo {
                    facts.photo = photo;
                    self.dirty_meta.insert(chat_id);
                }
            }
            None => {
                self.gaps.insert(chat_id);
            }
        }
    }

    fn on_protection(&mut self, chat_id: i64, is_protected: bool) {
        match self.facts.get_mut(&chat_id) {
            Some(facts) => {
                if emittable_kind(&facts.kind).is_some() && facts.is_protected != is_protected {
                    facts.is_protected = is_protected;
                    self.dirty_meta.insert(chat_id);
                }
            }
            None => {
                self.gaps.insert(chat_id);
            }
        }
    }

    fn on_position(&mut self, chat_id: i64, position: &Value) {
        // Membership references chats; an unknown or excluded chat cannot carry
        // one. Excluded flavors are silently ignored (POL-4); truly unknown
        // chats are gaps.
        match self.facts.get(&chat_id) {
            Some(facts) if emittable_kind(&facts.kind).is_some() => {}
            Some(_) => return,
            None => {
                self.gaps.insert(chat_id);
                return;
            }
        }
        let Some(list) = position.get("list").and_then(parse_list) else {
            return;
        };
        let Some(order) = position.get("order").and_then(parse_int64) else {
            return;
        };
        let map = self.positions.entry(chat_id).or_default();
        if order == 0 {
            if map.remove(&list).is_some() {
                self.mark_membership(list, chat_id);
            }
        } else {
            let next = ListPosition {
                sort_order: order,
                pinned: position
                    .get("is_pinned")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            };
            if map.get(&list) != Some(&next) {
                map.insert(list, next);
                self.mark_membership(list, chat_id);
            }
        }
    }

    fn on_removed_from_list(&mut self, chat_id: i64, list: ChatListKind) {
        match self.facts.get(&chat_id) {
            Some(facts) if emittable_kind(&facts.kind).is_some() => {}
            Some(_) => return,
            None => {
                self.gaps.insert(chat_id);
                return;
            }
        }
        if let Some(map) = self.positions.get_mut(&chat_id)
            && map.remove(&list).is_some()
        {
            self.mark_membership(list, chat_id);
        }
    }

    fn on_user_name(&mut self, user_id: i64, username: Option<String>) {
        self.user_names.insert(user_id, username.clone());
        if let Some(&chat_id) = self.chat_of_user.get(&user_id) {
            self.apply_username(chat_id, username);
        }
    }

    fn on_supergroup_name(&mut self, supergroup_id: i64, username: Option<String>) {
        self.supergroup_names
            .insert(supergroup_id, username.clone());
        if let Some(&chat_id) = self.chat_of_supergroup.get(&supergroup_id) {
            self.apply_username(chat_id, username);
        }
    }

    // -- internals ----------------------------------------------------------

    fn mark_membership(&mut self, list: ChatListKind, chat_id: i64) {
        self.dirty_memberships.insert((list, chat_id));
        self.dirty_lists.insert(list);
    }

    fn apply_username(&mut self, chat_id: i64, username: Option<String>) {
        if let Some(facts) = self.facts.get_mut(&chat_id)
            && emittable_kind(&facts.kind).is_some()
            && facts.username != username
        {
            facts.username = username;
            self.dirty_meta.insert(chat_id);
            self.renamed.insert(chat_id);
        }
    }

    fn resolve_username(&self, kind: &KindFact) -> Option<String> {
        match kind {
            KindFact::Private { user_id } => self.user_names.get(user_id).cloned().flatten(),
            KindFact::Supergroup { supergroup_id } | KindFact::Channel { supergroup_id } => {
                self.supergroup_names.get(supergroup_id).cloned().flatten()
            }
            _ => None,
        }
    }
}

/// The provider-facing chat flavor of a fact kind, or `None` for the flavors
/// excluded from every commit (secret and unknown types).
fn emittable_kind(kind: &KindFact) -> Option<SnapshotChatKind> {
    match kind {
        KindFact::Private { .. } => Some(SnapshotChatKind::Private),
        KindFact::Group => Some(SnapshotChatKind::Group),
        KindFact::Supergroup { .. } => Some(SnapshotChatKind::Supergroup),
        KindFact::Channel { .. } => Some(SnapshotChatKind::Channel),
        KindFact::Secret | KindFact::Unsupported => None,
    }
}

/// A total order over list kinds, so a drained batch is deterministic even
/// though [`ChatListKind`] is intentionally unordered.
fn list_sort_key(list: ChatListKind) -> (u8, i32) {
    match list {
        ChatListKind::Main => (0, 0),
        ChatListKind::Archive => (1, 0),
        ChatListKind::Folder(folder) => (2, folder.0),
    }
}

/// An opaque, stable token for a `chatPhotoInfo` object (or its absence).
/// Prefers TDLib's per-file `unique_id`; falls back to the object's
/// deterministic text form so any change is still observed.
fn photo_token(photo: Option<&Value>) -> Option<String> {
    let photo = photo?;
    if photo.is_null() {
        return None;
    }
    let unique_id = photo
        .get("small")
        .and_then(|file| file.get("remote"))
        .and_then(|remote| remote.get("unique_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    match unique_id {
        Some(id) => Some(id.to_owned()),
        None => Some(photo.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gramdrive_model::identity::FolderId;
    use serde_json::json;

    const MAIN: ChatListKind = ChatListKind::Main;

    fn private_type(user_id: i64) -> Value {
        json!({"@type": "chatTypePrivate", "user_id": user_id})
    }

    fn new_chat(id: i64, title: &str, chat_type: Value, positions: Value) -> Value {
        json!({
            "@type": "updateNewChat",
            "chat": {
                "id": id,
                "type": chat_type,
                "title": title,
                "positions": positions,
            },
        })
    }

    fn position(list: ChatListKind, order: i64, pinned: bool) -> Value {
        let list = match list {
            ChatListKind::Main => json!({"@type": "chatListMain"}),
            ChatListKind::Archive => json!({"@type": "chatListArchive"}),
            ChatListKind::Folder(folder) => {
                json!({"@type": "chatListFolder", "chat_folder_id": folder.0})
            }
        };
        json!({
            "@type": "chatPosition",
            "list": list,
            "order": order.to_string(),
            "is_pinned": pinned,
        })
    }

    fn position_update(chat_id: i64, list: ChatListKind, order: i64, pinned: bool) -> Value {
        json!({
            "@type": "updateChatPosition",
            "chat_id": chat_id,
            "position": position(list, order, pinned),
        })
    }

    #[test]
    fn first_sight_emits_metadata_and_membership() {
        let mut machine = UpdateMachine::new();
        machine.on_update(&new_chat(
            10,
            "Alice",
            private_type(110),
            json!([position(MAIN, 9_000, true)]),
        ));
        let batch = machine.take_batch();
        assert_eq!(
            batch.chats,
            vec![ChatMetadata {
                chat_id: 10,
                kind: SnapshotChatKind::Private,
                title: "Alice".to_owned(),
                username: None,
                is_protected: false,
                photo: None,
            }]
        );
        assert_eq!(
            batch.memberships,
            vec![MembershipChange::Set {
                list: MAIN,
                chat_id: 10,
                sort_order: 9_000,
                pinned: true,
            }]
        );
        assert_eq!(
            batch.invalidations,
            vec![
                Invalidation::Metadata { chat_id: 10 },
                Invalidation::ListOrdering { list: MAIN },
            ]
        );
        assert!(batch.unresolved.is_empty());
        // Drained: nothing pending, replaying the same updates is a no-op.
        assert!(!machine.has_pending());
        assert!(machine.take_batch().is_empty());
    }

    #[test]
    fn rename_emits_folder_name_and_leaves_membership_untouched() {
        let mut machine = UpdateMachine::new();
        machine.on_update(&new_chat(10, "Alice", private_type(110), json!([])));
        let _ = machine.take_batch();

        machine.on_update(&json!({
            "@type": "updateChatTitle", "chat_id": 10, "title": "Alicia",
        }));
        let batch = machine.take_batch();
        assert_eq!(batch.chats.len(), 1);
        assert_eq!(batch.chats[0].title, "Alicia");
        assert_eq!(
            batch.invalidations,
            vec![Invalidation::FolderName { chat_id: 10 }]
        );
        assert!(batch.memberships.is_empty(), "a rename touches no list");
    }

    #[test]
    fn reorder_emits_order_only_and_never_metadata() {
        let mut machine = UpdateMachine::new();
        machine.on_update(&new_chat(
            10,
            "Alice",
            private_type(110),
            json!([position(MAIN, 9_000, false)]),
        ));
        let _ = machine.take_batch();

        // Same chat, new order and pin — a pure reorder.
        machine.on_update(&position_update(10, MAIN, 12_000, true));
        let batch = machine.take_batch();
        assert!(
            batch.chats.is_empty(),
            "a reorder never re-upserts the canonical chat (stable id)"
        );
        assert_eq!(
            batch.memberships,
            vec![MembershipChange::Set {
                list: MAIN,
                chat_id: 10,
                sort_order: 12_000,
                pinned: true,
            }]
        );
        assert_eq!(
            batch.invalidations,
            vec![Invalidation::ListOrdering { list: MAIN }],
            "reorder triggers order.json regen only"
        );
    }

    #[test]
    fn duplicate_position_and_title_coalesce_to_noop() {
        let mut machine = UpdateMachine::new();
        machine.on_update(&new_chat(
            10,
            "Alice",
            private_type(110),
            json!([position(MAIN, 9_000, false)]),
        ));
        let _ = machine.take_batch();

        // Identical repeats change nothing.
        machine.on_update(&position_update(10, MAIN, 9_000, false));
        machine.on_update(&json!({
            "@type": "updateChatTitle", "chat_id": 10, "title": "Alice",
        }));
        assert!(!machine.has_pending());
        assert!(machine.take_batch().is_empty());
    }

    #[test]
    fn photo_and_protection_changes_emit_metadata_not_rename() {
        let mut machine = UpdateMachine::new();
        machine.on_update(&new_chat(10, "Alice", private_type(110), json!([])));
        let _ = machine.take_batch();

        machine.on_update(&json!({
            "@type": "updateChatPhoto",
            "chat_id": 10,
            "photo": {"small": {"remote": {"unique_id": "avatar-v2"}}},
        }));
        let batch = machine.take_batch();
        assert_eq!(batch.chats.len(), 1);
        assert_eq!(batch.chats[0].photo.as_deref(), Some("avatar-v2"));
        assert_eq!(
            batch.invalidations,
            vec![Invalidation::Metadata { chat_id: 10 }]
        );

        machine.on_update(&json!({
            "@type": "updateChatHasProtectedContent",
            "chat_id": 10,
            "has_protected_content": true,
        }));
        let batch = machine.take_batch();
        assert!(batch.chats[0].is_protected);
        assert_eq!(
            batch.invalidations,
            vec![Invalidation::Metadata { chat_id: 10 }]
        );
    }

    #[test]
    fn leaving_a_list_removes_membership_two_ways() {
        let mut machine = UpdateMachine::new();
        machine.on_update(&new_chat(
            10,
            "Alice",
            private_type(110),
            json!([
                position(MAIN, 9_000, false),
                position(ChatListKind::Archive, 5, false)
            ]),
        ));
        let _ = machine.take_batch();

        // Order 0 leaves Main.
        machine.on_update(&position_update(10, MAIN, 0, false));
        let batch = machine.take_batch();
        assert_eq!(
            batch.memberships,
            vec![MembershipChange::Removed {
                list: MAIN,
                chat_id: 10,
            }]
        );
        assert_eq!(
            batch.invalidations,
            vec![Invalidation::ListOrdering { list: MAIN }]
        );

        // updateChatRemovedFromList leaves Archive.
        machine.on_update(&json!({
            "@type": "updateChatRemovedFromList",
            "chat_id": 10,
            "chat_list": {"@type": "chatListArchive"},
        }));
        let batch = machine.take_batch();
        assert_eq!(
            batch.memberships,
            vec![MembershipChange::Removed {
                list: ChatListKind::Archive,
                chat_id: 10,
            }]
        );
        // A repeated removal of an absent membership is a no-op.
        machine.on_update(&position_update(10, MAIN, 0, false));
        assert!(machine.take_batch().is_empty());
    }

    #[test]
    fn username_propagates_from_a_peer_update() {
        let mut machine = UpdateMachine::new();
        machine.on_update(&new_chat(10, "Alice", private_type(110), json!([])));
        let _ = machine.take_batch();

        machine.on_update(&json!({
            "@type": "updateUser",
            "user": {"id": 110, "usernames": {"editable_username": "alice", "active_usernames": ["alice"]}},
        }));
        let batch = machine.take_batch();
        assert_eq!(batch.chats.len(), 1);
        assert_eq!(batch.chats[0].username.as_deref(), Some("alice"));
        assert_eq!(
            batch.invalidations,
            vec![Invalidation::FolderName { chat_id: 10 }],
            "a username is part of the folder name"
        );
    }

    #[test]
    fn a_peer_update_before_the_chat_is_resolved_on_first_sight() {
        let mut machine = UpdateMachine::new();
        machine.on_update(&json!({
            "@type": "updateUser",
            "user": {"id": 110, "usernames": {"editable_username": "alice", "active_usernames": ["alice"]}},
        }));
        machine.on_update(&new_chat(10, "Alice", private_type(110), json!([])));
        let batch = machine.take_batch();
        assert_eq!(batch.chats[0].username.as_deref(), Some("alice"));
        // First sight is a creation, not a rename.
        assert_eq!(
            batch.invalidations,
            vec![Invalidation::Metadata { chat_id: 10 }]
        );
    }

    #[test]
    fn an_update_about_an_unknown_chat_is_a_gap_then_resolves() {
        let mut machine = UpdateMachine::new();
        machine.on_update(&json!({
            "@type": "updateChatTitle", "chat_id": 999, "title": "Ghost",
        }));
        machine.on_update(&position_update(999, MAIN, 5, false));
        let batch = machine.take_batch();
        assert_eq!(batch.unresolved, vec![999]);
        assert!(batch.chats.is_empty(), "no forged row for an unknown chat");
        assert!(
            batch.memberships.is_empty(),
            "no membership without a chat row"
        );

        // The caller getChats it and feeds the full object back: the current
        // title and positions arrive together, so nothing is lost.
        machine.on_update(&new_chat(
            999,
            "Casper",
            private_type(1999),
            json!([position(MAIN, 5, false)]),
        ));
        let batch = machine.take_batch();
        assert!(batch.unresolved.is_empty());
        assert_eq!(batch.chats[0].title, "Casper");
        assert_eq!(
            batch.memberships,
            vec![MembershipChange::Set {
                list: MAIN,
                chat_id: 999,
                sort_order: 5,
                pinned: false,
            }]
        );
    }

    #[test]
    fn secret_and_unknown_chats_are_excluded_never_gaps() {
        let mut machine = UpdateMachine::new();
        machine.on_update(&new_chat(
            10,
            "Secret",
            json!({"@type": "chatTypeSecret", "secret_chat_id": 5}),
            json!([position(MAIN, 9_000, false)]),
        ));
        machine.on_update(&position_update(10, MAIN, 8_000, false));
        machine.on_update(&json!({
            "@type": "updateChatTitle", "chat_id": 10, "title": "Renamed Secret",
        }));
        let batch = machine.take_batch();
        assert!(
            batch.is_empty(),
            "secret chats are excluded, not gaps: {batch:?}"
        );
    }

    #[test]
    fn independent_updates_converge_regardless_of_order() {
        let title = json!({"@type": "updateChatTitle", "chat_id": 10, "title": "Renamed"});
        let mut forward = UpdateMachine::new();
        forward.on_update(&new_chat(10, "Alice", private_type(110), json!([])));
        forward.on_update(&new_chat(20, "Bob", private_type(120), json!([])));
        forward.on_update(&title);

        let mut shuffled = UpdateMachine::new();
        shuffled.on_update(&new_chat(20, "Bob", private_type(120), json!([])));
        shuffled.on_update(&title);
        // A duplicate that must coalesce.
        shuffled.on_update(&new_chat(20, "Bob", private_type(120), json!([])));
        shuffled.on_update(&new_chat(10, "Alice", private_type(110), json!([])));
        shuffled.on_update(&title);

        assert_eq!(forward.take_batch(), shuffled.take_batch());
    }

    #[test]
    fn batch_ordering_is_deterministic_across_lists() {
        let folder = ChatListKind::Folder(FolderId(3));
        let mut machine = UpdateMachine::new();
        machine.on_update(&new_chat(
            30,
            "C",
            private_type(130),
            json!([position(folder, 1, false)]),
        ));
        machine.on_update(&new_chat(
            10,
            "A",
            private_type(110),
            json!([position(ChatListKind::Archive, 2, false)]),
        ));
        machine.on_update(&new_chat(
            20,
            "B",
            private_type(120),
            json!([position(MAIN, 3, false)]),
        ));
        let batch = machine.take_batch();
        let chat_ids: Vec<i64> = batch.chats.iter().map(|chat| chat.chat_id).collect();
        assert_eq!(chat_ids, vec![10, 20, 30], "chats ascend by id");
        let membership_lists: Vec<ChatListKind> = batch
            .memberships
            .iter()
            .map(|change| match change {
                MembershipChange::Set { list, .. } | MembershipChange::Removed { list, .. } => {
                    *list
                }
            })
            .collect();
        assert_eq!(
            membership_lists,
            vec![MAIN, ChatListKind::Archive, folder],
            "memberships follow Main, Archive, then folders"
        );
    }
}
