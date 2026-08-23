//! Durable, privacy-safe namespace usability and projection convergence.

use gramdrive_model::identity::{
    AccountScope, CanonicalKey, ChatId, ChatListKey, ChatListKind, FolderCatalogKey, ItemKey,
};
use rusqlite::{OptionalExtension, params};

use crate::error::StateError;
use crate::repo::{ReadTxn, WriteTxn, scope_columns};

/// Last known good namespace publication and its bounded convergence cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceReadinessRecord {
    /// Account namespace epoch this publication validates.
    pub scope: AccountScope,
    /// Monotonic successful snapshot publication generation.
    pub generation: u64,
    /// Time the usable generation was published.
    pub published_at_ms: i64,
    /// Last chat identity committed by bounded deep convergence.
    pub projection_after_chat_id: Option<ChatId>,
    /// Whether every listed chat was reconciled for this generation.
    pub convergence_complete: bool,
}

impl ReadTxn<'_> {
    /// Reads the last known good publication for the exact namespace epoch.
    pub fn namespace_readiness(
        &self,
        scope: AccountScope,
    ) -> Result<Option<NamespaceReadinessRecord>, StateError> {
        let (account_id, namespace_version) = scope_columns(&scope);
        self.conn()
            .prepare_cached(
                "SELECT generation, published_at_ms, projection_after_chat_id,
                        convergence_complete
                   FROM namespace_readiness
                  WHERE account_id = ?1 AND namespace_version = ?2",
            )?
            .query_row(params![account_id, namespace_version], |row| {
                let generation: i64 = row.get(0)?;
                Ok(NamespaceReadinessRecord {
                    scope,
                    generation: generation as u64,
                    published_at_ms: row.get(1)?,
                    projection_after_chat_id: row.get::<_, Option<i64>>(2)?.map(ChatId),
                    convergence_complete: row.get(3)?,
                })
            })
            .optional()
            .map_err(StateError::from)
    }
}

impl WriteTxn<'_> {
    /// Conservatively adopts an existing structurally valid namespace once.
    ///
    /// This migration bridge is intentionally stricter than ordinary reads:
    /// authorization, fixed live structure, complete historical snapshot
    /// evidence, and empty bootstrap/migration/repair journals must all agree.
    pub fn adopt_namespace_readiness(
        &self,
        scope: AccountScope,
        adopted_at_ms: i64,
    ) -> Result<bool, StateError> {
        if self.read().namespace_readiness(scope)?.is_some() {
            return Ok(true);
        }
        let account = self.read().account(scope.account)?;
        if account
            .as_ref()
            .is_none_or(|account| account.scope() != scope || account.auth_state != "authorized")
            || self.read().namespace_bootstrap(scope)?.is_some()
        {
            return Ok(false);
        }
        let has_complete_audits = [ChatListKind::Main, ChatListKind::Archive]
            .into_iter()
            .map(|kind| ChatListKey { scope, kind })
            .map(|list| self.read().latest_chat_list_commit_audit(&list))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .all(|audit| audit.is_some_and(|audit| audit.is_complete));
        if !has_complete_audits {
            return Ok(false);
        }
        let pending_migration: bool = self.conn().query_row(
            "SELECT EXISTS (SELECT 1 FROM migration_progress)",
            [],
            |row| row.get(0),
        )?;
        let pending_repair: bool =
            self.conn()
                .query_row("SELECT EXISTS (SELECT 1 FROM repair_markers)", [], |row| {
                    row.get(0)
                })?;
        if pending_migration || pending_repair {
            return Ok(false);
        }
        let expected = [
            ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                scope,
                kind: ChatListKind::Main,
            }))
            .id(),
            ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                scope,
                kind: ChatListKind::Archive,
            }))
            .id(),
            ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
                scope,
                kind: ChatListKind::Stories,
            }))
            .id(),
            ItemKey::Canonical(CanonicalKey::FolderCatalog(FolderCatalogKey { scope })).id(),
        ];
        for item in expected {
            if self
                .read()
                .item(&item)?
                .is_none_or(|item| item.deleted_at_ms.is_some())
            {
                return Ok(false);
            }
        }
        self.publish_namespace_readiness(scope, adopted_at_ms)?;
        Ok(true)
    }

    /// Publishes a new usable generation and resets bounded convergence.
    pub fn publish_namespace_readiness(
        &self,
        scope: AccountScope,
        published_at_ms: i64,
    ) -> Result<NamespaceReadinessRecord, StateError> {
        if published_at_ms < 0 {
            return Err(StateError::InvalidArgument {
                what: "namespace readiness time is negative",
            });
        }
        let generation = self
            .read()
            .namespace_readiness(scope)?
            .map_or(1, |record| record.generation.saturating_add(1));
        let generation_sql =
            i64::try_from(generation).map_err(|_| StateError::InvalidArgument {
                what: "namespace readiness generation exceeds sqlite integer",
            })?;
        let (account_id, namespace_version) = scope_columns(&scope);
        self.conn()
            .prepare_cached(
                "INSERT INTO namespace_readiness (
                     account_id, namespace_version, generation, published_at_ms,
                     projection_after_chat_id, convergence_complete, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, NULL, 0, ?4)
                 ON CONFLICT (account_id, namespace_version) DO UPDATE SET
                     generation = excluded.generation,
                     published_at_ms = excluded.published_at_ms,
                     projection_after_chat_id = NULL,
                     convergence_complete = 0,
                     updated_at_ms = excluded.updated_at_ms",
            )?
            .execute(params![
                account_id,
                namespace_version,
                generation_sql,
                published_at_ms
            ])?;
        Ok(NamespaceReadinessRecord {
            scope,
            generation,
            published_at_ms,
            projection_after_chat_id: None,
            convergence_complete: false,
        })
    }

    /// Advances one generation's projection cursor atomically with its slice.
    pub fn advance_namespace_projection(
        &self,
        scope: AccountScope,
        generation: u64,
        after_chat_id: Option<ChatId>,
        convergence_complete: bool,
        updated_at_ms: i64,
    ) -> Result<(), StateError> {
        let generation = i64::try_from(generation).map_err(|_| StateError::InvalidArgument {
            what: "namespace readiness generation exceeds sqlite integer",
        })?;
        let (account_id, namespace_version) = scope_columns(&scope);
        let changed = self
            .conn()
            .prepare_cached(
                "UPDATE namespace_readiness
                    SET projection_after_chat_id = ?4,
                        convergence_complete = ?5,
                        updated_at_ms = ?6
                  WHERE account_id = ?1 AND namespace_version = ?2 AND generation = ?3",
            )?
            .execute(params![
                account_id,
                namespace_version,
                generation,
                after_chat_id.map(|chat| chat.0),
                convergence_complete,
                updated_at_ms,
            ])?;
        if changed == 0 {
            return Err(StateError::RowNotFound {
                entity: "namespace readiness generation",
            });
        }
        Ok(())
    }
}
