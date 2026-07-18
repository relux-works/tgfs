//! Durable per-scope state of the metadata-first backfill scheduler
//! (TASK-260715-mua1ng; POL-2/DEC-014, NFR-031, SYNC-070, SEC-031, NFR-033).
//!
//! The engine's backfill scheduler keeps no authoritative state in memory
//! (`.spec/architecture.md`): the pause a user set and the flood wait a
//! Telegram 429 mandated must both survive a process restart. Losing the
//! pause would resume paused work on the next launch; losing the flood-wait
//! deadline would re-hammer an account still under a wait — a ban risk
//! (NFR-033: a flood wait is never a tight retry loop). Durable state is the
//! guard: this progress must survive a process restart (NFR-031, SYNC-070).
//! This module persists exactly that: one row per `(account,
//! namespace_version)` scope.
//!
//! Per-chat history progress and the backlog order the scheduler paces are
//! *not* here — they are the [`chat_sync_state`](crate::repo::changes) rows
//! ([`ReadTxn::backfill_backlog`](crate::ReadTxn::backfill_backlog)). This
//! row holds only the account-global control the scheduler owns: the pause
//! switch, the request spacer, and the honored flood-wait deadline.

use gramdrive_model::identity::AccountScope;
use rusqlite::{OptionalExtension, params};

use crate::error::StateError;
use crate::repo::{ReadTxn, WriteTxn, scope_columns};

/// Durable control state of one scope's backfill scheduler.
///
/// Absence of the row (a fresh account, or one never paced) reads back as
/// `None`: the scheduler treats that as unpaused with no pending deadline,
/// so no explicit initialization step is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackfillControlRecord {
    /// Whether the user paused backfill for this scope (the task AC's
    /// user-pausable requirement; SYNC-043/SYNC-005 durable resumable state).
    /// A paused scheduler issues no history or media work until resumed.
    pub paused: bool,
    /// Account-global request spacer: the earliest wall-clock ms at which
    /// the next provider request may issue (SEC-031). `None` means no
    /// spacing is currently armed.
    pub next_request_at_ms: Option<i64>,
    /// A honored Telegram flood-wait deadline in ms: no provider request
    /// issues before it (NFR-033). `None` means no flood wait is in effect.
    pub flood_wait_until_ms: Option<i64>,
    /// When this control row last changed, ms since the Unix epoch.
    pub updated_at_ms: i64,
}

impl BackfillControlRecord {
    /// The unpaused, deadline-free control a never-paced scope behaves as,
    /// stamped `updated_at_ms`. The scheduler uses this as the base it
    /// modifies when no row exists yet.
    pub fn fresh(updated_at_ms: i64) -> Self {
        Self {
            paused: false,
            next_request_at_ms: None,
            flood_wait_until_ms: None,
            updated_at_ms,
        }
    }
}

impl ReadTxn<'_> {
    /// The durable backfill control of `scope`, or `None` if none was ever
    /// stored (SYNC-043/SYNC-005 durable pause, NFR-033 flood wait).
    pub fn backfill_control(
        &self,
        scope: AccountScope,
    ) -> Result<Option<BackfillControlRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        let raw: Option<(bool, Option<i64>, Option<i64>, i64)> = self
            .conn()
            .prepare_cached(
                "SELECT paused, next_request_at_ms, flood_wait_until_ms, updated_at_ms
                 FROM backfill_control
                 WHERE account_id = ?1 AND namespace_version = ?2",
            )?
            .query_row(params![account_id, namespace], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .optional()?;
        Ok(raw.map(
            |(paused, next_request_at_ms, flood_wait_until_ms, updated_at_ms)| {
                BackfillControlRecord {
                    paused,
                    next_request_at_ms,
                    flood_wait_until_ms,
                    updated_at_ms,
                }
            },
        ))
    }
}

impl WriteTxn<'_> {
    /// Stores `record` as the durable backfill control of `scope`,
    /// replacing any previous row.
    ///
    /// The scheduler reads the current control, applies one transition
    /// (pause, arm spacer, arm flood wait), and writes it back in the same
    /// transaction — the read-modify-write is race-free because the write
    /// transaction is `BEGIN IMMEDIATE`.
    pub fn put_backfill_control(
        &self,
        scope: AccountScope,
        record: &BackfillControlRecord,
    ) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        self.conn()
            .prepare_cached(
                "INSERT INTO backfill_control (account_id, namespace_version, paused,
                                               next_request_at_ms, flood_wait_until_ms,
                                               updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (account_id, namespace_version) DO UPDATE SET
                     paused = excluded.paused,
                     next_request_at_ms = excluded.next_request_at_ms,
                     flood_wait_until_ms = excluded.flood_wait_until_ms,
                     updated_at_ms = excluded.updated_at_ms",
            )?
            .execute(params![
                account_id,
                namespace,
                record.paused,
                record.next_request_at_ms,
                record.flood_wait_until_ms,
                record.updated_at_ms,
            ])?;
        Ok(())
    }
}
