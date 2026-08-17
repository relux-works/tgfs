//! Durable decision records for cross-resource authorization finalization.
//!
//! The journal contains no secret material. It records whether an incumbent
//! existed and which side of the account-row commit recovery must preserve;
//! TDLib directory and keychain rollback artifacts remain in their native
//! stores.

use gramdrive_model::identity::AccountId;
use rusqlite::{OptionalExtension, params};

use super::{ReadTxn, WriteTxn};
use crate::error::StateError;

/// Which side of authorization finalization owns the stable account name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFinalizationPhase {
    /// The incumbent still owns the account; recovery rolls staged changes back.
    Prepared,
    /// The successor row committed; recovery keeps it and removes backups.
    Committed,
}

impl AuthFinalizationPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Committed => "committed",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "prepared" => Ok(Self::Prepared),
            "committed" => Ok(Self::Committed),
            other => Err(StateError::CorruptRow {
                table: "auth_finalization_journal",
                detail: format!("unknown phase {other:?}"),
            }),
        }
    }
}

/// Non-secret recovery facts for one account replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthFinalizationRecord {
    /// Stable Telegram identity being installed.
    pub account: AccountId,
    /// Durable side of the replacement decision.
    pub phase: AuthFinalizationPhase,
    /// Whether shared state already contained this account before preparation.
    pub had_account_row: bool,
    /// Whether the stable keychain alias existed before preparation.
    pub had_database_key: bool,
    /// Whether the stable TDLib directory existed before preparation.
    pub had_tdlib_state: bool,
}

impl ReadTxn<'_> {
    /// One pending authorization finalization, when present.
    pub fn auth_finalization(
        &self,
        account: AccountId,
    ) -> Result<Option<AuthFinalizationRecord>, StateError> {
        let raw = self
            .conn()
            .prepare_cached(
                "SELECT phase, had_account_row, had_database_key, had_tdlib_state
                 FROM auth_finalization_journal WHERE account_id = ?1",
            )?
            .query_row([account.0], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })
            .optional()?;
        raw.map(
            |(phase, had_account_row, had_database_key, had_tdlib_state)| {
                Ok(AuthFinalizationRecord {
                    account,
                    phase: AuthFinalizationPhase::parse(&phase)?,
                    had_account_row,
                    had_database_key,
                    had_tdlib_state,
                })
            },
        )
        .transpose()
    }

    /// Every pending authorization finalization, in deterministic order.
    pub fn auth_finalizations(&self) -> Result<Vec<AuthFinalizationRecord>, StateError> {
        let mut statement = self.conn().prepare_cached(
            "SELECT account_id, phase, had_account_row, had_database_key, had_tdlib_state
             FROM auth_finalization_journal ORDER BY account_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (account, phase, had_account_row, had_database_key, had_tdlib_state) = row?;
            records.push(AuthFinalizationRecord {
                account: AccountId(account),
                phase: AuthFinalizationPhase::parse(&phase)?,
                had_account_row,
                had_database_key,
                had_tdlib_state,
            });
        }
        Ok(records)
    }
}

impl WriteTxn<'_> {
    /// Durably declares a reversible replacement before any incumbent mutation.
    pub fn prepare_auth_finalization(
        &self,
        record: AuthFinalizationRecord,
    ) -> Result<(), StateError> {
        self.conn().execute(
            "INSERT INTO auth_finalization_journal (
                 account_id, phase, had_account_row, had_database_key, had_tdlib_state
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (account_id) DO UPDATE SET
                 phase = excluded.phase,
                 had_account_row = excluded.had_account_row,
                 had_database_key = excluded.had_database_key,
                 had_tdlib_state = excluded.had_tdlib_state",
            params![
                record.account.0,
                record.phase.as_str(),
                record.had_account_row,
                record.had_database_key,
                record.had_tdlib_state,
            ],
        )?;
        Ok(())
    }

    /// Flips the durable decision in the same transaction as the successor row.
    pub fn commit_auth_finalization(&self, account: AccountId) -> Result<(), StateError> {
        let changed = self.conn().execute(
            "UPDATE auth_finalization_journal SET phase = 'committed'
             WHERE account_id = ?1 AND phase = 'prepared'",
            [account.0],
        )?;
        if changed != 1 {
            return Err(StateError::RowNotFound {
                entity: "prepared auth finalization",
            });
        }
        Ok(())
    }

    /// Removes a converged journal record. Missing is already converged.
    pub fn clear_auth_finalization(&self, account: AccountId) -> Result<(), StateError> {
        self.conn().execute(
            "DELETE FROM auth_finalization_journal WHERE account_id = ?1",
            [account.0],
        )?;
        Ok(())
    }
}
