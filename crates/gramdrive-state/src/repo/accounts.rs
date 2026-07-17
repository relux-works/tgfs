//! Account rows: per-account policy facts and the namespace epoch
//! (DOM-021, POL-2, POL-3).

use gramdrive_model::identity::{AccountId, AccountKey, AccountScope, NamespaceVersion};
use rusqlite::{OptionalExtension, Row, params};

use crate::error::StateError;
use crate::repo::{ReadTxn, WriteTxn, namespace_from_column};

/// Which source implementation serves an account (`accounts.source_kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A local TDLib session.
    LocalTdlib,
    /// A remote HTTP drive service.
    RemoteHttp,
}

impl SourceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::LocalTdlib => "local_tdlib",
            Self::RemoteHttp => "remote_http",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "local_tdlib" => Ok(Self::LocalTdlib),
            "remote_http" => Ok(Self::RemoteHttp),
            other => Err(StateError::CorruptRow {
                table: "accounts",
                detail: format!("unknown source_kind '{other}'"),
            }),
        }
    }
}

/// The per-account POL-3 retention selection (`accounts.retention_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionMode {
    /// Mirror mode: content purged on observed deletion, minimal markers
    /// kept for sync correctness.
    Mirror,
    /// Audit mode: observed history retained until explicitly purged.
    Audit,
}

impl RetentionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::Audit => "audit",
        }
    }

    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "mirror" => Ok(Self::Mirror),
            "audit" => Ok(Self::Audit),
            other => Err(StateError::CorruptRow {
                table: "accounts",
                detail: format!("unknown retention_mode '{other}'"),
            }),
        }
    }
}

/// One configured account (domain-model § Account).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    /// The account's stable identity.
    pub account: AccountKey,
    /// Which source implementation serves it.
    pub source_kind: SourceKind,
    /// Display name shown as the account's root directory.
    pub display_name: String,
    /// Source-defined authorization state text (never secret material).
    pub auth_state: String,
    /// Current identity-namespace epoch (DOM-021). Advanced only by
    /// [`WriteTxn::bump_namespace`]; ignored on upsert of an existing row.
    pub namespace_version: NamespaceVersion,
    /// POL-3 retention selection.
    pub retention_mode: RetentionMode,
    /// POL-2 Archive-Mode toggle.
    pub archive_mode: bool,
    /// Reference into platform secure storage — never key material.
    pub secret_ref: Option<String>,
    /// When the account was configured (ms since the Unix epoch).
    pub created_at_ms: i64,
    /// Last update to this row (ms since the Unix epoch).
    pub updated_at_ms: i64,
}

impl AccountRecord {
    /// The account's current scope: identity plus namespace epoch.
    pub fn scope(&self) -> AccountScope {
        AccountScope {
            account: self.account,
            namespace_version: self.namespace_version,
        }
    }
}

/// One account row exactly as stored, before enum text and namespace
/// columns are converted (conversion can fail, and `rusqlite` row mapping
/// cannot carry [`StateError`]).
struct RawAccount {
    account_id: i64,
    source_kind: String,
    display_name: String,
    auth_state: String,
    namespace_version: i64,
    retention_mode: String,
    archive_mode: bool,
    secret_ref: Option<String>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

fn read_account(row: &Row<'_>) -> Result<RawAccount, rusqlite::Error> {
    Ok(RawAccount {
        account_id: row.get("account_id")?,
        source_kind: row.get("source_kind")?,
        display_name: row.get("display_name")?,
        auth_state: row.get("auth_state")?,
        namespace_version: row.get("namespace_version")?,
        retention_mode: row.get("retention_mode")?,
        archive_mode: row.get("archive_mode")?,
        secret_ref: row.get("secret_ref")?,
        created_at_ms: row.get("created_at_ms")?,
        updated_at_ms: row.get("updated_at_ms")?,
    })
}

fn finish_account(raw: RawAccount) -> Result<AccountRecord, StateError> {
    Ok(AccountRecord {
        account: AccountKey {
            account_id: AccountId(raw.account_id),
        },
        source_kind: SourceKind::parse(&raw.source_kind)?,
        display_name: raw.display_name,
        auth_state: raw.auth_state,
        namespace_version: namespace_from_column("accounts", raw.namespace_version)?,
        retention_mode: RetentionMode::parse(&raw.retention_mode)?,
        archive_mode: raw.archive_mode,
        secret_ref: raw.secret_ref,
        created_at_ms: raw.created_at_ms,
        updated_at_ms: raw.updated_at_ms,
    })
}

const SELECT_ACCOUNT: &str = "SELECT account_id, source_kind, display_name, auth_state,
            namespace_version, retention_mode, archive_mode, secret_ref,
            created_at_ms, updated_at_ms
     FROM accounts";

impl ReadTxn<'_> {
    /// One account by identity, or `None` if it is not configured.
    pub fn account(&self, account: AccountKey) -> Result<Option<AccountRecord>, StateError> {
        let parts = self
            .conn()
            .prepare_cached(&format!("{SELECT_ACCOUNT} WHERE account_id = ?1"))?
            .query_row(params![account.account_id.0], read_account)
            .optional()?;
        parts.map(finish_account).transpose()
    }

    /// Every configured account, in identity order.
    pub fn accounts(&self) -> Result<Vec<AccountRecord>, StateError> {
        let mut statement = self
            .conn()
            .prepare_cached(&format!("{SELECT_ACCOUNT} ORDER BY account_id"))?;
        let rows = statement.query_map([], read_account)?;
        let mut accounts = Vec::new();
        for row in rows {
            accounts.push(finish_account(row?)?);
        }
        Ok(accounts)
    }

    /// The current scope of an account — its identity at today's namespace
    /// epoch — or `None` if the account is not configured.
    pub fn current_scope(&self, account: AccountKey) -> Result<Option<AccountScope>, StateError> {
        let namespace: Option<i64> = self
            .conn()
            .prepare_cached("SELECT namespace_version FROM accounts WHERE account_id = ?1")?
            .query_row(params![account.account_id.0], |row| row.get(0))
            .optional()?;
        namespace
            .map(|value| {
                Ok(AccountScope {
                    account,
                    namespace_version: namespace_from_column("accounts", value)?,
                })
            })
            .transpose()
    }
}

impl WriteTxn<'_> {
    /// Inserts the account, or updates every fact of an existing row except
    /// the namespace epoch and creation time.
    ///
    /// The epoch is excluded on purpose: it only moves forward, through
    /// [`WriteTxn::bump_namespace`], so a stale in-memory record replayed
    /// through upsert can never rewind it (DOM-021).
    pub fn upsert_account(&self, record: &AccountRecord) -> Result<(), StateError> {
        if record.auth_state.is_empty() {
            return Err(StateError::InvalidArgument {
                what: "account auth_state must not be empty",
            });
        }
        if record.secret_ref.as_deref() == Some("") {
            return Err(StateError::InvalidArgument {
                what: "account secret_ref must not be empty text",
            });
        }
        self.conn()
            .prepare_cached(
                "INSERT INTO accounts (account_id, source_kind, display_name, auth_state,
                                       namespace_version, retention_mode, archive_mode,
                                       secret_ref, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT (account_id) DO UPDATE SET
                     source_kind = excluded.source_kind,
                     display_name = excluded.display_name,
                     auth_state = excluded.auth_state,
                     retention_mode = excluded.retention_mode,
                     archive_mode = excluded.archive_mode,
                     secret_ref = excluded.secret_ref,
                     updated_at_ms = excluded.updated_at_ms",
            )?
            .execute(params![
                record.account.account_id.0,
                record.source_kind.as_str(),
                record.display_name,
                record.auth_state,
                i64::from(record.namespace_version.0),
                record.retention_mode.as_str(),
                record.archive_mode,
                record.secret_ref,
                record.created_at_ms,
                record.updated_at_ms,
            ])?;
        Ok(())
    }

    /// Advances the account's namespace epoch by one and returns the new
    /// epoch (DOM-021).
    ///
    /// Rows of the retired epoch stay until reconciliation sweeps them; this
    /// operation only moves the boundary.
    pub fn bump_namespace(
        &self,
        account: AccountKey,
        updated_at_ms: i64,
    ) -> Result<NamespaceVersion, StateError> {
        let namespace: Option<i64> = self
            .conn()
            .prepare_cached(
                "UPDATE accounts
                 SET namespace_version = namespace_version + 1, updated_at_ms = ?2
                 WHERE account_id = ?1
                 RETURNING namespace_version",
            )?
            .query_row(params![account.account_id.0, updated_at_ms], |row| {
                row.get(0)
            })
            .optional()?;
        match namespace {
            Some(value) => namespace_from_column("accounts", value),
            None => Err(StateError::RowNotFound { entity: "account" }),
        }
    }
}
