//! The folder-catalog discovery machine: TDLib's `updateChatFolders` becomes a
//! deterministic, provider-neutral normalized change stream for the custom
//! Telegram folders (chat filters) that populate the "Telegram Folders/"
//! catalog (TASK-260715-54nopz, SYNC-026).
//!
//! # Where it sits
//!
//! A custom Telegram folder is two separate facts. Its *membership* — which
//! chats it contains and in what order — arrives as ordinary
//! `updateChatPosition` entries with a `chatListFolder` list, and the
//! [`SnapshotMachine`](crate::snapshot) and [`UpdateMachine`](crate::updates)
//! already fold those into `chat_list_entries` appearances (one canonical chat,
//! one membership row per list, DOM-022). What neither machine discovers is the
//! folder's *definition*: that the folder exists at all, its title (the name of
//! its directory under the catalog), and its position among the folder tabs.
//! TDLib carries that in exactly one update — `updateChatFolders`, pushed on
//! connect and on every catalog change — and [`FolderCatalogMachine`] is the
//! reducer that turns it into normalized create/rename/delete/reorder changes.
//!
//! The catalog is also what tells the snapshot *which folders to enumerate*:
//! [`FolderCatalogMachine::folders`] is the ordered folder-id set a composing
//! caller feeds into a [`SnapshotPlan`](crate::snapshot::SnapshotPlan) so each
//! folder's membership is loaded (the snapshot machine deliberately snapshots
//! only the lists it is given and left folder discovery here).
//!
//! # Shape: a sans-IO full-state reducer
//!
//! Like the [`UpdateMachine`](crate::updates) this machine issues no requests
//! and owns no client. `updateChatFolders` always carries the *complete*
//! ordered folder list, so every push is a full-state observation:
//!
//! 1. Feed each `updateChatFolders` to [`FolderCatalogMachine::on_update`]; it
//!    replaces the observed catalog wholesale and ignores every other update.
//! 2. Drain the accumulated normalized changes with
//!    [`FolderCatalogMachine::take_batch`] and apply the
//!    [`FolderCatalogBatch`] — the caller upserts each changed folder
//!    definition and, for every removed folder, clears its membership
//!    appearances (`replace_chat_list(folder, &[])`), which drops the
//!    appearances and leaves the canonical chats and every other list
//!    untouched (SYNC-026). `take_batch` folds the observed catalog into the
//!    committed baseline, so the next drain reports only what changed since.
//!
//! # Convergence, duplicates, and out-of-order delivery
//!
//! The batch is the difference between the last-observed catalog and the
//! last-drained one. A duplicated or replayed `updateChatFolders` observes the
//! same catalog, so its batch is empty; a process restart — a fresh machine fed
//! TDLib's re-pushed catalog — converges to the same definitions, and because
//! the caller's upserts are idempotent the restart re-emit is a true no-op.
//!
//! # Provider invalidation (POL-1)
//!
//! Each change carries what it invalidates downstream, and the split is the
//! same reorder/rename discipline the chat machines use:
//!
//! - A **created** folder appears as a new view root under the catalog
//!   ([`FolderInvalidation::Created`]).
//! - A **renamed** folder — its title changed — moves its stable directory
//!   name, so it emits [`FolderInvalidation::Renamed`]. A folder that only
//!   shifted position never does (POL-1/SYNC-011: a reorder is content, never a
//!   rename).
//! - A **removed** folder ([`FolderInvalidation::Removed`]) drops its view and
//!   every appearance under it.
//! - Any change to the set or its order emits a single
//!   [`FolderInvalidation::CatalogOrdering`] — regenerate the catalog's
//!   ordering document, and nothing else.
//!
//! # Boundaries
//!
//! Folder *membership* is out of scope here by construction — it flows through
//! the chat machines' `chat_list_entries` appearances, so a chat in several
//! folders stays one canonical record with one appearance per folder, and
//! deleting a folder removes only those appearances. This machine owns the
//! folder definitions and their order; it never touches a chat row.

use std::collections::BTreeMap;

use serde_json::Value;

use gramdrive_model::identity::FolderId;

/// One custom Telegram folder as the catalog machine last observed it: its
/// identity, current title, and position among the folder tabs.
///
/// Identity is the [`FolderId`] alone (SYNC-026); `title` and `position` are
/// replaceable metadata. `position` is ordering metadata — the catalog's
/// `order.json`, not the folder's name — so a position-only change regenerates
/// the order without renaming the directory (POL-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderDefinition {
    /// Telegram folder (chat filter) id.
    pub id: FolderId,
    /// Current folder title as observed — the folder's directory name.
    pub title: String,
    /// Zero-based position among the folders in the catalog, in TDLib's tab
    /// order.
    pub position: u32,
}

/// What a normalized folder-catalog change invalidates downstream (POL-1). See
/// the module docs for the reorder/rename split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderInvalidation {
    /// A folder appeared — a new custom-folder view root under the catalog.
    Created {
        /// The new folder.
        id: FolderId,
    },
    /// A folder's title changed — its catalog directory must be renamed. A pure
    /// reorder never emits this (POL-1/SYNC-011).
    Renamed {
        /// The renamed folder.
        id: FolderId,
    },
    /// A folder was deleted — remove its view root and every appearance under
    /// it (the chat memberships, never the canonical chats).
    Removed {
        /// The deleted folder.
        id: FolderId,
    },
    /// The catalog's folder set or order changed — regenerate its ordering
    /// document, and nothing else.
    CatalogOrdering,
}

/// One drained checkpoint of normalized folder-catalog changes, applied to the
/// state layer in one transaction (module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FolderCatalogBatch {
    /// Folders whose definition is new or changed (created, renamed, or
    /// reordered), ascending by folder id. The caller upserts each.
    pub upserts: Vec<FolderDefinition>,
    /// Folders that left the catalog (deleted), ascending by id. For each, the
    /// caller drops the folder's membership appearances — `replace_chat_list`
    /// with an empty membership — which leaves the canonical chats and every
    /// other list untouched (SYNC-026).
    pub removed: Vec<FolderId>,
    /// The provider invalidations these changes imply (POL-1), deterministic:
    /// `Created`, then `Renamed`, then `Removed` — each ascending by id — then
    /// one `CatalogOrdering` if the set or order moved.
    pub invalidations: Vec<FolderInvalidation>,
}

impl FolderCatalogBatch {
    /// Whether the batch carries nothing to apply — a checkpoint over an
    /// unchanged catalog (the duplicate/replay case).
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.removed.is_empty() && self.invalidations.is_empty()
    }
}

/// One folder's observed facts. Identity is the id it is keyed under; this is
/// the replaceable metadata a diff compares.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FolderState {
    title: String,
    position: u32,
}

/// The deterministic sans-IO folder-catalog discovery machine for one
/// authorized account's client (module docs).
#[derive(Debug, Default)]
pub struct FolderCatalogMachine {
    /// The catalog as of the last [`FolderCatalogMachine::take_batch`] — the
    /// committed baseline every diff is taken against.
    committed: BTreeMap<i32, FolderState>,
    /// The catalog as last observed from `updateChatFolders`.
    current: BTreeMap<i32, FolderState>,
}

impl FolderCatalogMachine {
    /// A fresh machine that has observed no catalog yet.
    pub fn new() -> FolderCatalogMachine {
        FolderCatalogMachine::default()
    }

    /// Whether a [`FolderCatalogMachine::take_batch`] would return anything — a
    /// cheap check before opening a write transaction.
    pub fn has_pending(&self) -> bool {
        self.current != self.committed
    }

    /// The current folder set in catalog order (by position, then id) — the
    /// list a composing caller feeds into a snapshot plan so each folder's
    /// membership is enumerated.
    pub fn folders(&self) -> Vec<FolderId> {
        let mut ordered: Vec<(u32, i32)> = self
            .current
            .iter()
            .map(|(&id, state)| (state.position, id))
            .collect();
        ordered.sort_unstable();
        ordered.into_iter().map(|(_, id)| FolderId(id)).collect()
    }

    /// Feed one update from the client's stream. Only `updateChatFolders` is
    /// consumed — it carries the complete ordered folder list, so it replaces
    /// the observed catalog wholesale; every other update is ignored.
    ///
    /// A folder entry missing an integer id is skipped rather than guessed at;
    /// a missing title becomes empty text, the same fail-safe the chat machines
    /// apply to a titleless chat.
    pub fn on_update(&mut self, update: &Value) {
        if update.get("@type").and_then(Value::as_str) != Some("updateChatFolders") {
            return;
        }
        let Some(folders) = update.get("chat_folders").and_then(Value::as_array) else {
            return;
        };
        let mut next = BTreeMap::new();
        for (index, folder) in folders.iter().enumerate() {
            let Some(id) = folder
                .get("id")
                .and_then(Value::as_i64)
                .and_then(|id| i32::try_from(id).ok())
            else {
                continue;
            };
            next.insert(
                id,
                FolderState {
                    title: folder_title(folder),
                    position: u32::try_from(index).unwrap_or(u32::MAX),
                },
            );
        }
        self.current = next;
    }

    /// Drain the normalized changes accumulated since the last call, folding
    /// the observed catalog into the committed baseline — the transactional
    /// checkpoint (module docs).
    pub fn take_batch(&mut self) -> FolderCatalogBatch {
        let mut upserts = Vec::new();
        let mut created = Vec::new();
        let mut renamed = Vec::new();
        let mut removed = Vec::new();
        let mut order_changed = false;

        // BTreeMap iterates ascending by id, so every list built here is
        // already ascending — the determinism the batch contract promises.
        for (&id, state) in &self.current {
            match self.committed.get(&id) {
                None => {
                    created.push(id);
                    order_changed = true;
                    upserts.push(definition(id, state));
                }
                Some(old) if old != state => {
                    if old.title != state.title {
                        renamed.push(id);
                    }
                    if old.position != state.position {
                        order_changed = true;
                    }
                    upserts.push(definition(id, state));
                }
                Some(_) => {}
            }
        }
        for &id in self.committed.keys() {
            if !self.current.contains_key(&id) {
                removed.push(id);
                order_changed = true;
            }
        }

        let mut invalidations = Vec::new();
        for &id in &created {
            invalidations.push(FolderInvalidation::Created { id: FolderId(id) });
        }
        for &id in &renamed {
            invalidations.push(FolderInvalidation::Renamed { id: FolderId(id) });
        }
        for &id in &removed {
            invalidations.push(FolderInvalidation::Removed { id: FolderId(id) });
        }
        if order_changed {
            invalidations.push(FolderInvalidation::CatalogOrdering);
        }

        self.committed = self.current.clone();

        FolderCatalogBatch {
            upserts,
            removed: removed.into_iter().map(FolderId).collect(),
            invalidations,
        }
    }
}

/// Build a [`FolderDefinition`] from one observed folder state.
fn definition(id: i32, state: &FolderState) -> FolderDefinition {
    FolderDefinition {
        id: FolderId(id),
        title: state.title.clone(),
        position: state.position,
    }
}

/// The folder's title across TDLib shapes: the modern `name.text.text`
/// (`chatFolderName` wrapping a `formattedText`), the intermediate `name.text`
/// or bare-string `name`, and the oldest bare-string `title`. Absent
/// everywhere, the empty string — the same fail-safe as a titleless chat.
fn folder_title(folder: &Value) -> String {
    let name = folder.get("name");
    if let Some(text) = name
        .and_then(|name| name.get("text"))
        .and_then(|formatted| formatted.get("text"))
        .and_then(Value::as_str)
    {
        return text.to_owned();
    }
    if let Some(text) = name
        .and_then(|name| name.get("text"))
        .and_then(Value::as_str)
    {
        return text.to_owned();
    }
    if let Some(text) = name.and_then(Value::as_str) {
        return text.to_owned();
    }
    folder
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An `updateChatFolders` from `(id, title)` pairs, in the given tab order.
    fn folders_update(folders: &[(i32, &str)]) -> Value {
        let chat_folders: Vec<Value> = folders
            .iter()
            .map(|(id, title)| {
                json!({
                    "@type": "chatFolderInfo",
                    "id": id,
                    "name": {
                        "@type": "chatFolderName",
                        "text": {"@type": "formattedText", "text": title, "entities": []},
                        "animate_custom_emoji": false,
                    },
                    "color_id": -1,
                    "is_shareable": false,
                    "has_my_invite_links": false,
                })
            })
            .collect();
        json!({
            "@type": "updateChatFolders",
            "chat_folders": chat_folders,
            "main_chat_list_position": 0,
            "are_tags_enabled": false,
        })
    }

    fn definition(id: i32, title: &str, position: u32) -> FolderDefinition {
        FolderDefinition {
            id: FolderId(id),
            title: title.to_owned(),
            position,
        }
    }

    #[test]
    fn first_sight_creates_every_folder_in_catalog_order() {
        let mut machine = FolderCatalogMachine::new();
        machine.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
        let batch = machine.take_batch();
        assert_eq!(
            batch.upserts,
            vec![definition(4, "Work", 0), definition(7, "Family", 1)],
            "upserts ascend by id and carry the tab position"
        );
        assert!(batch.removed.is_empty());
        assert_eq!(
            batch.invalidations,
            vec![
                FolderInvalidation::Created { id: FolderId(4) },
                FolderInvalidation::Created { id: FolderId(7) },
                FolderInvalidation::CatalogOrdering,
            ]
        );
        assert_eq!(machine.folders(), vec![FolderId(4), FolderId(7)]);
        // Drained: a replay of the same catalog is a no-op.
        assert!(!machine.has_pending());
        assert!(machine.take_batch().is_empty());
    }

    #[test]
    fn rename_emits_folder_name_and_never_catalog_ordering() {
        let mut machine = FolderCatalogMachine::new();
        machine.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
        let _ = machine.take_batch();

        machine.on_update(&folders_update(&[(4, "Job"), (7, "Family")]));
        let batch = machine.take_batch();
        assert_eq!(batch.upserts, vec![definition(4, "Job", 0)]);
        assert!(batch.removed.is_empty());
        assert_eq!(
            batch.invalidations,
            vec![FolderInvalidation::Renamed { id: FolderId(4) }],
            "a rename regenerates no order"
        );
    }

    #[test]
    fn reorder_emits_catalog_ordering_only_and_never_a_rename() {
        let mut machine = FolderCatalogMachine::new();
        machine.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
        let _ = machine.take_batch();

        // Same folders, swapped tab order — a pure reorder.
        machine.on_update(&folders_update(&[(7, "Family"), (4, "Work")]));
        let batch = machine.take_batch();
        assert_eq!(
            batch.upserts,
            vec![definition(4, "Work", 1), definition(7, "Family", 0)],
            "both positions moved; neither title did"
        );
        assert!(batch.removed.is_empty());
        assert_eq!(
            batch.invalidations,
            vec![FolderInvalidation::CatalogOrdering],
            "a reorder is content, never a rename (POL-1)"
        );
        assert_eq!(
            machine.folders(),
            vec![FolderId(7), FolderId(4)],
            "the folder set follows tab order, not id"
        );
    }

    #[test]
    fn deletion_removes_the_folder_and_shifts_the_survivors() {
        let mut machine = FolderCatalogMachine::new();
        machine.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
        let _ = machine.take_batch();

        // Deleting the first folder pulls the survivor up to position 0, so it
        // is re-upserted for its order (never renamed) alongside the removal.
        machine.on_update(&folders_update(&[(7, "Family")]));
        let batch = machine.take_batch();
        assert_eq!(batch.upserts, vec![definition(7, "Family", 0)]);
        assert_eq!(batch.removed, vec![FolderId(4)]);
        assert_eq!(
            batch.invalidations,
            vec![
                FolderInvalidation::Removed { id: FolderId(4) },
                FolderInvalidation::CatalogOrdering,
            ],
            "the survivor shifted position; a shift is order, not a rename"
        );
        assert_eq!(machine.folders(), vec![FolderId(7)]);
    }

    #[test]
    fn deleting_the_last_folder_leaves_the_earlier_ones_untouched() {
        let mut machine = FolderCatalogMachine::new();
        machine.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
        let _ = machine.take_batch();

        // Removing the tail folder shifts nobody, so no survivor is re-upserted.
        machine.on_update(&folders_update(&[(4, "Work")]));
        let batch = machine.take_batch();
        assert!(batch.upserts.is_empty(), "no survivor moved: {batch:?}");
        assert_eq!(batch.removed, vec![FolderId(7)]);
        assert_eq!(
            batch.invalidations,
            vec![
                FolderInvalidation::Removed { id: FolderId(7) },
                FolderInvalidation::CatalogOrdering,
            ]
        );
        assert_eq!(machine.folders(), vec![FolderId(4)]);
    }

    #[test]
    fn creating_a_folder_shifts_and_re_upserts_the_ones_after_it() {
        let mut machine = FolderCatalogMachine::new();
        machine.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
        let _ = machine.take_batch();

        // A new folder inserted at the front shifts both existing positions.
        machine.on_update(&folders_update(&[
            (2, "Unread"),
            (4, "Work"),
            (7, "Family"),
        ]));
        let batch = machine.take_batch();
        assert_eq!(
            batch.upserts,
            vec![
                definition(2, "Unread", 0),
                definition(4, "Work", 1),
                definition(7, "Family", 2),
            ],
            "the new folder and every folder it displaced are re-upserted"
        );
        assert_eq!(
            batch.invalidations,
            vec![
                FolderInvalidation::Created { id: FolderId(2) },
                FolderInvalidation::CatalogOrdering,
            ],
            "only the new folder is a creation; the shifts are order, not renames"
        );
    }

    #[test]
    fn a_duplicate_catalog_coalesces_to_a_noop() {
        let mut machine = FolderCatalogMachine::new();
        machine.on_update(&folders_update(&[(4, "Work")]));
        let _ = machine.take_batch();

        machine.on_update(&folders_update(&[(4, "Work")]));
        assert!(!machine.has_pending());
        assert!(machine.take_batch().is_empty());
    }

    #[test]
    fn intermediate_observations_between_drains_coalesce() {
        // Two pushes before a single drain: the batch is the net change from
        // the committed baseline, so the transient "Work" title never leaks.
        let mut machine = FolderCatalogMachine::new();
        machine.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
        let _ = machine.take_batch();

        machine.on_update(&folders_update(&[(4, "Job")]));
        machine.on_update(&folders_update(&[(4, "Career")]));
        let batch = machine.take_batch();
        assert_eq!(batch.upserts, vec![definition(4, "Career", 0)]);
        assert_eq!(batch.removed, vec![FolderId(7)]);
        assert_eq!(
            batch.invalidations,
            vec![
                FolderInvalidation::Renamed { id: FolderId(4) },
                FolderInvalidation::Removed { id: FolderId(7) },
                FolderInvalidation::CatalogOrdering,
            ]
        );
    }

    #[test]
    fn a_restart_re_push_converges_without_churn() {
        // A fresh machine fed the same catalog the first drained is a no-op the
        // moment its baseline is set — the SYNC-003 restart-stability property.
        let mut first = FolderCatalogMachine::new();
        first.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
        let committed = first.take_batch();

        let mut restarted = FolderCatalogMachine::new();
        restarted.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
        let after_restart = restarted.take_batch();
        assert_eq!(
            committed, after_restart,
            "a restart re-derives the same catalog from the same push"
        );
        // The caller's upserts being idempotent, re-applying changes nothing.
        restarted.on_update(&folders_update(&[(4, "Work"), (7, "Family")]));
        assert!(restarted.take_batch().is_empty());
    }

    #[test]
    fn non_folder_updates_and_malformed_entries_are_ignored() {
        let mut machine = FolderCatalogMachine::new();
        machine.on_update(&json!({"@type": "updateChatPosition", "chat_id": 1}));
        machine.on_update(&json!({"@type": "updateChatFolders"}));
        assert!(!machine.has_pending(), "no folder list, no change");

        // A folder without an id is skipped; the well-formed one survives.
        machine.on_update(&json!({
            "@type": "updateChatFolders",
            "chat_folders": [
                {"@type": "chatFolderInfo", "name": {"text": {"text": "Ghost"}}},
                {"@type": "chatFolderInfo", "id": 4,
                 "name": {"text": {"text": "Work"}}},
            ],
        }));
        let batch = machine.take_batch();
        assert_eq!(
            batch.upserts,
            vec![definition(4, "Work", 1)],
            "the malformed entry is skipped but still consumes its index"
        );
    }

    #[test]
    fn title_is_read_across_tdlib_name_shapes() {
        let mut machine = FolderCatalogMachine::new();
        machine.on_update(&json!({
            "@type": "updateChatFolders",
            "chat_folders": [
                {"id": 1, "name": {"text": {"text": "modern"}}},
                {"id": 2, "name": {"text": "intermediate"}},
                {"id": 3, "name": "bare-name"},
                {"id": 4, "title": "legacy"},
                {"id": 5},
            ],
        }));
        let titles: Vec<String> = machine
            .take_batch()
            .upserts
            .into_iter()
            .map(|folder| folder.title)
            .collect();
        assert_eq!(
            titles,
            vec![
                "modern".to_owned(),
                "intermediate".to_owned(),
                "bare-name".to_owned(),
                "legacy".to_owned(),
                String::new(),
            ]
        );
    }
}
