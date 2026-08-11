//! Ordered Telegram chat-filter catalog and metadata bootstrap checkpoint.
//!
//! Folder definitions are canonical source metadata. Membership remains in
//! `chat_list_entries`, so deleting a folder removes only its appearances and
//! never a canonical chat. The snapshot resume token commits in the same
//! transaction as the list rows it witnesses.

use gramdrive_model::identity::{AccountScope, FolderId};
use rusqlite::{OptionalExtension, params};

use crate::error::StateError;
use crate::repo::{ReadTxn, WriteTxn, scope_columns};

/// One user-defined Telegram chat filter in Telegram's catalog order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRecord {
    /// Account namespace this folder belongs to.
    pub scope: AccountScope,
    /// Telegram folder identity.
    pub folder_id: FolderId,
    /// Current source title.
    pub title: String,
    /// Zero-based Telegram catalog position.
    pub position: u32,
}

/// Durable progress of the metadata-only list snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceBootstrapRecord {
    /// Account namespace this checkpoint belongs to.
    pub scope: AccountScope,
    /// Opaque `SnapshotMachine` resume token.
    pub resume_token: Vec<u8>,
    /// Commit time for diagnostics only.
    pub updated_at_ms: i64,
}

impl ReadTxn<'_> {
    /// The complete folder catalog in Telegram order.
    pub fn folders(&self, scope: AccountScope) -> Result<Vec<FolderRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        let mut statement = self.conn().prepare_cached(
            "SELECT folder_id, title, position FROM chat_folders
             WHERE account_id = ?1 AND namespace_version = ?2
             ORDER BY position, folder_id",
        )?;
        let rows = statement.query_map(params![account_id, namespace], |row| {
            let position: i64 = row.get(2)?;
            Ok((row.get::<_, i32>(0)?, row.get::<_, String>(1)?, position))
        })?;
        let mut folders = Vec::new();
        for row in rows {
            let (folder_id, title, position) = row?;
            let position = u32::try_from(position).map_err(|_| StateError::CorruptRow {
                table: "chat_folders",
                detail: format!("position {position} does not fit u32"),
            })?;
            folders.push(FolderRecord {
                scope,
                folder_id: FolderId(folder_id),
                title,
                position,
            });
        }
        Ok(folders)
    }

    /// The last atomically committed snapshot checkpoint, if interrupted.
    pub fn namespace_bootstrap(
        &self,
        scope: AccountScope,
    ) -> Result<Option<NamespaceBootstrapRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        self.conn()
            .prepare_cached(
                "SELECT resume_token, updated_at_ms FROM namespace_bootstrap
                 WHERE account_id = ?1 AND namespace_version = ?2",
            )?
            .query_row(params![account_id, namespace], |row| {
                Ok(NamespaceBootstrapRecord {
                    scope,
                    resume_token: row.get(0)?,
                    updated_at_ms: row.get(1)?,
                })
            })
            .optional()
            .map_err(StateError::from)
    }
}

impl WriteTxn<'_> {
    /// Replaces the complete folder catalog and removes membership rows only
    /// for folders no longer present. Canonical chats are never deleted.
    pub fn replace_folders(
        &self,
        scope: AccountScope,
        folders: &[FolderRecord],
    ) -> Result<Vec<FolderId>, StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        let previous = self.read().folders(scope)?;
        let next_ids: std::collections::BTreeSet<i32> =
            folders.iter().map(|folder| folder.folder_id.0).collect();
        if next_ids.len() != folders.len() {
            return Err(StateError::InvalidArgument {
                what: "folder catalog contains duplicate ids",
            });
        }
        let positions: std::collections::BTreeSet<u32> =
            folders.iter().map(|folder| folder.position).collect();
        if positions.len() != folders.len()
            || folders
                .iter()
                .any(|folder| folder.scope != scope || folder.folder_id.0 == 0)
        {
            return Err(StateError::InvalidArgument {
                what: "folder catalog has an invalid scope, id, or position",
            });
        }

        self.conn()
            .prepare_cached(
                "DELETE FROM chat_folders
                 WHERE account_id = ?1 AND namespace_version = ?2",
            )?
            .execute(params![account_id, namespace])?;
        let mut insert = self.conn().prepare_cached(
            "INSERT INTO chat_folders
             (account_id, namespace_version, folder_id, title, position)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for folder in folders {
            insert.execute(params![
                account_id,
                namespace,
                folder.folder_id.0,
                folder.title,
                i64::from(folder.position),
            ])?;
        }

        let removed: Vec<FolderId> = previous
            .into_iter()
            .filter(|folder| !next_ids.contains(&folder.folder_id.0))
            .map(|folder| folder.folder_id)
            .collect();
        let mut clear = self.conn().prepare_cached(
            "DELETE FROM chat_list_entries
             WHERE account_id = ?1 AND namespace_version = ?2
               AND list_kind = 'folder' AND folder_id = ?3",
        )?;
        for folder in &removed {
            clear.execute(params![account_id, namespace, folder.0])?;
        }
        Ok(removed)
    }

    /// Commits a snapshot resume token with the normalized list rows.
    pub fn put_namespace_bootstrap(
        &self,
        record: &NamespaceBootstrapRecord,
    ) -> Result<(), StateError> {
        let (account_id, namespace) = scope_columns(&record.scope);
        self.conn()
            .prepare_cached(
                "INSERT INTO namespace_bootstrap
                 (account_id, namespace_version, resume_token, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (account_id, namespace_version) DO UPDATE SET
                   resume_token = excluded.resume_token,
                   updated_at_ms = excluded.updated_at_ms",
            )?
            .execute(params![
                account_id,
                namespace,
                record.resume_token,
                record.updated_at_ms,
            ])?;
        Ok(())
    }

    /// Clears a fully consumed checkpoint. Idempotent.
    pub fn clear_namespace_bootstrap(&self, scope: AccountScope) -> Result<bool, StateError> {
        let (account_id, namespace) = scope_columns(&scope);
        Ok(self
            .conn()
            .prepare_cached(
                "DELETE FROM namespace_bootstrap
                 WHERE account_id = ?1 AND namespace_version = ?2",
            )?
            .execute(params![account_id, namespace])?
            > 0)
    }
}
