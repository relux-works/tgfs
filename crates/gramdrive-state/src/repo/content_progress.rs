//! Privacy-safe per-chat history/live progress (TASK-260721-yrcjlo).
//!
//! The normalized cursor lives in `chat_sync_state`; this repository records
//! why that cursor is or is not moving without persisting Telegram error text,
//! chat titles, message contents, or other sensitive diagnostics.

use gramdrive_model::identity::ChatKey;
use rusqlite::{OptionalExtension, params};

use crate::error::StateError;
use crate::repo::{ReadTxn, WriteTxn, scope_columns};

type RawChatContentProgress = (String, Option<String>, bool, Option<i64>, i64, i64);

/// Durable operational phase for one chat's content synchronization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatContentPhase {
    /// Known chat, not yet selected by the bounded scheduler.
    Pending,
    /// A history page or live checkpoint is currently being advanced.
    Syncing,
    /// History reached its beginning; live updates may still append.
    Ready,
    /// Telegram rejected history for this chat; an explicit retry may help.
    Unavailable,
    /// Product privacy policy forbids background content persistence.
    Protected,
    /// A retry budget or runtime operation failed.
    Failed,
    /// Live gap recovery froze the cursor until a later crawl retries.
    Degraded,
    /// The owning session stopped at a safe page boundary.
    Cancelled,
}

impl ChatContentPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Syncing => "syncing",
            Self::Ready => "ready",
            Self::Unavailable => "unavailable",
            Self::Protected => "protected",
            Self::Failed => "failed",
            Self::Degraded => "degraded",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "pending" => Ok(Self::Pending),
            "syncing" => Ok(Self::Syncing),
            "ready" => Ok(Self::Ready),
            "unavailable" => Ok(Self::Unavailable),
            "protected" => Ok(Self::Protected),
            "failed" => Ok(Self::Failed),
            "degraded" => Ok(Self::Degraded),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(StateError::CorruptRow {
                table: "chat_content_progress",
                detail: format!("unknown content phase '{other}'"),
            }),
        }
    }

    fn requires_category(self) -> bool {
        matches!(
            self,
            Self::Unavailable | Self::Protected | Self::Failed | Self::Degraded
        )
    }
}

/// One privacy-safe progress row. `failure_category` is a stable local
/// vocabulary, never raw TDLib error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatContentProgressRecord {
    /// Current operational phase.
    pub phase: ChatContentPhase,
    /// Stable privacy-safe category for exceptional phases.
    pub failure_category: Option<String>,
    /// Whether an explicit retry can be meaningful.
    pub retryable: bool,
    /// Earliest suggested retry time, when a server delay is known.
    pub retry_at_ms: Option<i64>,
    /// Consecutive attempts represented by this state.
    pub attempt_count: u32,
    /// Caller-supplied observation time.
    pub updated_at_ms: i64,
}

impl ChatContentProgressRecord {
    fn validate(&self) -> Result<(), StateError> {
        if self.phase.requires_category() != self.failure_category.is_some() {
            return Err(StateError::InvalidArgument {
                what: "content progress category must match the exceptional phase",
            });
        }
        if self.retryable
            && !matches!(
                self.phase,
                ChatContentPhase::Unavailable
                    | ChatContentPhase::Failed
                    | ChatContentPhase::Degraded
            )
        {
            return Err(StateError::InvalidArgument {
                what: "only unavailable, failed, or degraded content is retryable",
            });
        }
        if self.retry_at_ms.is_some() && !self.retryable {
            return Err(StateError::InvalidArgument {
                what: "content retry_at_ms requires retryable progress",
            });
        }
        Ok(())
    }
}

impl ReadTxn<'_> {
    /// Reads one chat's privacy-safe content progress.
    pub fn chat_content_progress(
        &self,
        chat: &ChatKey,
    ) -> Result<Option<ChatContentProgressRecord>, StateError> {
        let (account_id, namespace) = scope_columns(&chat.scope);
        let raw: Option<RawChatContentProgress> = self
            .conn()
            .prepare_cached(
                "SELECT phase, failure_category, retryable, retry_at_ms,
                        attempt_count, updated_at_ms
                 FROM chat_content_progress
                 WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3",
            )?
            .query_row(params![account_id, namespace, chat.chat_id.0], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .optional()?;
        let Some((phase, failure_category, retryable, retry_at_ms, attempts, updated_at_ms)) = raw
        else {
            return Ok(None);
        };
        let attempt_count = u32::try_from(attempts).map_err(|_| StateError::CorruptRow {
            table: "chat_content_progress",
            detail: format!("attempt_count {attempts} does not fit u32"),
        })?;
        let record = ChatContentProgressRecord {
            phase: ChatContentPhase::parse(&phase)?,
            failure_category,
            retryable,
            retry_at_ms,
            attempt_count,
            updated_at_ms,
        };
        record.validate()?;
        Ok(Some(record))
    }
}

impl WriteTxn<'_> {
    /// Upserts one progress state. Call inside the same transaction as
    /// message and cursor changes when this state describes their commit.
    pub fn put_chat_content_progress(
        &self,
        chat: &ChatKey,
        record: &ChatContentProgressRecord,
    ) -> Result<(), StateError> {
        record.validate()?;
        let (account_id, namespace) = scope_columns(&chat.scope);
        self.conn()
            .prepare_cached(
                "INSERT INTO chat_content_progress (
                     account_id, namespace_version, chat_id, phase,
                     failure_category, retryable, retry_at_ms, attempt_count, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT (account_id, namespace_version, chat_id) DO UPDATE SET
                     phase = excluded.phase,
                     failure_category = excluded.failure_category,
                     retryable = excluded.retryable,
                     retry_at_ms = excluded.retry_at_ms,
                     attempt_count = excluded.attempt_count,
                     updated_at_ms = excluded.updated_at_ms",
            )?
            .execute(params![
                account_id,
                namespace,
                chat.chat_id.0,
                record.phase.as_str(),
                record.failure_category,
                record.retryable,
                record.retry_at_ms,
                i64::from(record.attempt_count),
                record.updated_at_ms,
            ])?;
        Ok(())
    }
}
