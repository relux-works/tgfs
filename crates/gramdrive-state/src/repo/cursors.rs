//! Durable change-feed positions (DOM-004, SYNC-004, SYNC-022).
//!
//! One cursor per (account, stream). The atomicity requirement of SYNC-022 —
//! a checkpoint commits together with the normalized state it witnessed —
//! is met by calling [`WriteTxn::put_cursor`] under the same [`WriteTxn`]
//! as the [`WriteTxn::apply_message_changes`] it seals.
//!
//! Scope discipline (SYNC-004): a cursor is stored and returned only for
//! the scope it was minted under. A cursor from a retired namespace epoch —
//! stale worker writing, or reader restoring after a bump — is rejected
//! with [`StateError::CursorOutOfScope`], never silently applied; the
//! correct reaction is re-baselining under the current scope.

use gramdrive_model::cursor::ChangeCursor;
use gramdrive_model::identity::{AccountKey, AccountScope};
use rusqlite::{OptionalExtension, params};

use crate::error::StateError;
use crate::repo::{ReadTxn, WriteTxn};

fn require_stream(stream: &str) -> Result<(), StateError> {
    if stream.is_empty() {
        return Err(StateError::InvalidArgument {
            what: "cursor stream name must not be empty",
        });
    }
    Ok(())
}

impl ReadTxn<'_> {
    /// The stored cursor of `(scope.account, stream)`, verified to belong
    /// to `scope`.
    ///
    /// `None` means no position was ever stored — start from a baseline. A
    /// stored cursor that does not parse is [`StateError::CursorCorrupt`];
    /// one minted under another scope (a retired epoch after a namespace
    /// bump) is [`StateError::CursorOutOfScope`] — both are explicit
    /// re-baseline signals, never a silent `None` (SYNC-004).
    pub fn cursor(
        &self,
        scope: AccountScope,
        stream: &str,
    ) -> Result<Option<ChangeCursor>, StateError> {
        require_stream(stream)?;
        let text: Option<String> = self
            .conn()
            .prepare_cached(
                "SELECT cursor_text FROM change_cursors WHERE account_id = ?1 AND stream = ?2",
            )?
            .query_row(params![scope.account.account_id.0, stream], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(text) = text else {
            return Ok(None);
        };
        let cursor =
            ChangeCursor::decode(&text).map_err(|source| StateError::CursorCorrupt { source })?;
        cursor
            .require_scope(scope)
            .map_err(|source| StateError::CursorOutOfScope { source })?;
        Ok(Some(cursor))
    }
}

impl WriteTxn<'_> {
    /// Stores `cursor` as the durable position of `(account, stream)`,
    /// replacing any previous position.
    ///
    /// The cursor's scope must match the account's *current* scope: a
    /// namespace bump retires every cursor minted before it, and a stale
    /// worker checkpointing into the new epoch is exactly the silent
    /// mis-apply SYNC-004 exists to prevent — rejected as
    /// [`StateError::CursorOutOfScope`], with the account's current scope
    /// as the expected side.
    ///
    /// Call under the same transaction as the state the cursor witnesses
    /// (SYNC-022).
    pub fn put_cursor(
        &self,
        stream: &str,
        cursor: &ChangeCursor,
        updated_at_ms: i64,
    ) -> Result<(), StateError> {
        require_stream(stream)?;
        let scope = cursor.scope();
        let current = self
            .read()
            .current_scope(scope.account)?
            .ok_or(StateError::RowNotFound { entity: "account" })?;
        cursor
            .require_scope(current)
            .map_err(|source| StateError::CursorOutOfScope { source })?;
        self.conn()
            .prepare_cached(
                "INSERT INTO change_cursors (account_id, namespace_version, stream,
                                             cursor_text, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (account_id, stream) DO UPDATE SET
                     namespace_version = excluded.namespace_version,
                     cursor_text = excluded.cursor_text,
                     updated_at_ms = excluded.updated_at_ms",
            )?
            .execute(params![
                scope.account.account_id.0,
                i64::from(scope.namespace_version.0),
                stream,
                cursor.encode(),
                updated_at_ms,
            ])?;
        Ok(())
    }

    /// Removes the stored cursor of `(account, stream)`, if any — the
    /// re-baseline step after a scope rejection. Returns whether a cursor
    /// was removed.
    pub fn clear_cursor(&self, account: AccountKey, stream: &str) -> Result<bool, StateError> {
        require_stream(stream)?;
        let changed = self
            .conn()
            .prepare_cached("DELETE FROM change_cursors WHERE account_id = ?1 AND stream = ?2")?
            .execute(params![account.account_id.0, stream])?;
        Ok(changed > 0)
    }
}
