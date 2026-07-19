//! The provider-visible item change journal (TASK-260715-rhcnhc;
//! PLAT-MAC-004): the durable "what changed after anchor N" read a native
//! file-system provider pages its change enumeration from.
//!
//! [`crate::StateStore::data_version`] answers only *whether* anything
//! changed, and is connection-relative by contract — it can never be
//! persisted as a sync anchor. This journal is what can: the item write
//! paths in [`super::items`] refresh a coalesced row per item on every
//! provider-visible change, and the row's `change_seq` — issued by
//! AUTOINCREMENT, so never reused and never rewound — is the durable anchor
//! vocabulary.
//!
//! # Coalescing
//!
//! One row per item, carrying the sequence of its *latest* change, keeps
//! the journal bounded by item count instead of change count (NFR-021's
//! spirit at the storage layer). Nothing a provider needs is lost: change
//! enumeration replays current item state, not history, so an anchor taken
//! before several changes of one item meets that item once, at its newest
//! sequence, joined live against `items`.
//!
//! # No-op discipline
//!
//! The write paths journal a row only when the stored provider-visible row
//! actually changed. An identical re-push — the engine re-baselining after
//! a restart (SYNC-021 replay) — advances nothing, so a provider's anchor
//! stays quiet across restarts instead of replaying the whole tree.
//!
//! # Journal identity
//!
//! Sequences are meaningful only within one database life: corruption
//! recovery quarantines a file, and the fresh database starts its sequences
//! over. [`ChangeJournalState::instance_id`] names the sequence space so an
//! anchor from a previous life is recognizably foreign (the provider
//! answers "anchor expired" and re-enumerates) instead of silently pointing
//! at unrelated changes.

use gramdrive_model::identity::AccountKey;
use gramdrive_model::identity::ItemId;
use rusqlite::{OptionalExtension, params};

use crate::error::StateError;
use crate::repo::{ItemRecord, ReadTxn, WriteTxn, item_id_from_column};

/// One journal entry: an item's latest provider-visible change, joined live
/// against its current `items` row (which may be a POL-3 tombstone — a
/// deletion is a change like any other).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemChangeRecord {
    /// The change's sequence — strictly increasing across the journal's
    /// life, never reused.
    pub sequence: i64,
    /// The item's current state, tombstone included.
    pub item: ItemRecord,
}

/// The journal's identity and high-water mark, read together: what a
/// provider host mints sync anchors from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeJournalState {
    /// Names this journal's sequence space; anchors carrying a different
    /// value are from another database life and must be treated as expired.
    pub instance_id: String,
    /// The highest sequence ever issued (0 on a journal that has never
    /// recorded a change). Monotonic even across coalescing and cascade
    /// deletes — it tracks issuance, not surviving rows.
    pub latest_sequence: i64,
}

impl ReadTxn<'_> {
    /// The journal's identity and high-water mark.
    pub fn change_journal_state(&self) -> Result<ChangeJournalState, StateError> {
        let instance_id: Option<String> = self
            .conn()
            .prepare_cached("SELECT instance_id FROM item_change_journal WHERE id = 1")?
            .query_row([], |row| row.get(0))
            .optional()?;
        let instance_id = instance_id.ok_or(StateError::CorruptRow {
            table: "item_change_journal",
            detail: "the journal identity row is missing".to_owned(),
        })?;
        // sqlite_sequence, not MAX(change_seq): a cascade delete (account
        // removal sweeping its items) lowers the maximum surviving row but
        // never un-issues a sequence, and anchors compare against issuance.
        let latest_sequence: i64 = self
            .conn()
            .prepare_cached(
                "SELECT COALESCE(
                     (SELECT seq FROM sqlite_sequence WHERE name = 'item_changes'), 0)",
            )?
            .query_row([], |row| row.get(0))?;
        Ok(ChangeJournalState {
            instance_id,
            latest_sequence,
        })
    }

    /// One page of an account's item changes with sequence greater than
    /// `after`, in sequence order. `after = 0` starts from the beginning of
    /// the journal's life; a full page (`len == limit`) means another page
    /// may follow, anchored after the last returned sequence.
    pub fn item_changes_since(
        &self,
        account: AccountKey,
        after: i64,
        limit: u32,
    ) -> Result<Vec<ItemChangeRecord>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT change_seq, item_id FROM item_changes
             WHERE account_id = ?1 AND change_seq > ?2
             ORDER BY change_seq LIMIT ?3",
        )?;
        let rows: Vec<(i64, Vec<u8>)> = statement
            .query_map(
                params![account.account_id.0, after, i64::from(limit)],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<Result<_, _>>()?;

        let mut changes = Vec::with_capacity(rows.len());
        for (sequence, id_bytes) in rows {
            let id = item_id_from_column("item_changes", &id_bytes)?;
            // The FK guarantees the item row outlives its journal row; a
            // miss here is corruption, not a race — this is one snapshot.
            let item = self.item(&id)?.ok_or(StateError::CorruptRow {
                table: "item_changes",
                detail: format!("journal row {sequence} references a missing item"),
            })?;
            changes.push(ItemChangeRecord { sequence, item });
        }
        Ok(changes)
    }
}

impl WriteTxn<'_> {
    /// Records that `id`'s provider-visible row changed, refreshing the
    /// item's coalesced journal row under a fresh sequence.
    ///
    /// Called by the item write paths after the row is written — the
    /// journal row copies `account_id` from the item row it references.
    /// Callers own the no-op discipline (module docs): only actual changes
    /// reach here.
    pub(super) fn journal_item_change(&self, id: &ItemId) -> Result<(), StateError> {
        // DELETE + INSERT rather than an upsert: AUTOINCREMENT issues the
        // fresh sequence only on INSERT, and issuance is the property the
        // journal exists for.
        self.conn()
            .prepare_cached("DELETE FROM item_changes WHERE item_id = ?1")?
            .execute(params![id.as_bytes()])?;
        self.conn()
            .prepare_cached(
                "INSERT INTO item_changes (item_id, account_id)
                 SELECT item_id, account_id FROM items WHERE item_id = ?1",
            )?
            .execute(params![id.as_bytes()])?;
        Ok(())
    }
}
