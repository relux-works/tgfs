//! Per-account live-content policy control (TASK-260721-2tamdj).
//!
//! This is the engine-side control backend consumed by native hosts. Retention
//! and Archive Mode are deliberately separate operations: Audit changes what
//! already-observed allowed content may remain, while Archive Mode creates
//! eager byte demand. Telegram restrictions have already reduced provider
//! items to unavailable before Archive candidates are selected.

use std::sync::Arc;

use gramdrive_model::identity::{AccountId, AccountKey};
use gramdrive_state::StateStore;
use gramdrive_state::repo::{AuditToMirrorConfirmation, RetentionChange, RetentionMode};

use crate::api::DriveError;
use crate::hydration::{Hydrator, state_error};
use crate::shared_state::shared_state_layout;

/// User-facing per-account retention selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RetentionSelection {
    /// Reflect current observed Telegram state.
    Mirror,
    /// Prospectively retain allowed observations without creating downloads.
    Audit,
}

impl From<RetentionSelection> for RetentionMode {
    fn from(value: RetentionSelection) -> Self {
        match value {
            RetentionSelection::Mirror => Self::Mirror,
            RetentionSelection::Audit => Self::Audit,
        }
    }
}

impl From<RetentionMode> for RetentionSelection {
    fn from(value: RetentionMode) -> Self {
        match value {
            RetentionMode::Mirror => Self::Mirror,
            RetentionMode::Audit => Self::Audit,
        }
    }
}

/// Truthful durable policy status for one account.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ContentPolicyStatus {
    /// Stable account identity.
    pub account_id: i64,
    /// Current committed retention selection.
    pub retention: RetentionSelection,
    /// Independent eager-byte policy.
    pub archive_mode: bool,
    /// Physical cache objects still awaiting crash-resumable deletion.
    pub pending_file_purges: u64,
    /// Allowed persistent items still awaiting verified Archive bytes.
    pub archive_backfill_pending_allowed_items: u64,
    /// Pending Archive items with a durable terminal transfer failure.
    pub archive_backfill_failed_allowed_items: u64,
    /// Stable category of the most recently updated failed pending item.
    pub archive_backfill_failure_category: Option<String>,
}

/// Database effects of one committed retention transition.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RetentionTransitionReport {
    /// Retention mode before the transaction.
    pub previous: RetentionSelection,
    /// Retention mode committed by the transaction.
    pub current: RetentionSelection,
    /// Historical event payloads removed from the canonical event log.
    pub purged_events: u64,
    /// Deleted-message attachment metadata removed.
    pub purged_attachments: u64,
    /// Superseded Audit attachment-version metadata removed.
    pub purged_attachment_versions: u64,
    /// Audit-retained profile-story metadata removed.
    pub purged_stories: u64,
    /// Blob rows no longer referenced by retained content.
    pub purged_blobs: u64,
    /// Audit-only cache ownership rows removed.
    pub purged_cache_entries: u64,
    /// Archive pins released with purged items.
    pub released_pins: u64,
    /// Provider items tombstoned for change enumeration.
    pub invalidated_items: u64,
    /// Render documents marked dirty for regeneration.
    pub invalidated_documents: u64,
    /// File-purge journal rows acknowledged during this call. Their objects
    /// were removed or were already absent/shared; an interrupted remainder
    /// stays visible in [`ContentPolicyStatus::pending_file_purges`].
    pub acknowledged_file_purges: u64,
}

/// Effects of toggling the independent Archive-Mode byte policy.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ArchiveModeTransitionReport {
    /// Archive Mode before the transaction.
    pub previous: bool,
    /// Archive Mode committed by the transaction.
    pub current: bool,
    /// Allowed persistent provider items newly pinned for eager hydration.
    pub pinned_items: u64,
    /// Archive-owned pins released without disturbing explicit user pins.
    pub released_items: u64,
}

/// Coordinator-owned policy backend for a shared data root.
#[derive(Debug, uniffi::Object)]
pub struct ContentPolicyController {
    data_root: String,
}

#[uniffi::export]
impl ContentPolicyController {
    /// Opens the policy backend. The shared state is still opened per command
    /// so each operation observes the latest cross-process commit.
    #[uniffi::constructor]
    pub fn new(data_root: String) -> Result<Arc<Self>, DriveError> {
        shared_state_layout(data_root.clone())?;
        Ok(Arc::new(Self { data_root }))
    }

    /// Exact destructive phrase for an account. UI may display this string but
    /// cannot replace the user's typed value with a bool.
    pub fn audit_to_mirror_confirmation_phrase(&self, account_id: i64) -> String {
        AuditToMirrorConfirmation::expected_phrase(account_key(account_id))
    }

    /// Reads committed state and resumable purge progress.
    pub fn status(&self, account_id: i64) -> Result<ContentPolicyStatus, DriveError> {
        let account = account_key(account_id);
        let mut store = self.open_store()?;
        let read = store.read_txn().map_err(state_error)?;
        let record =
            read.account(account)
                .map_err(state_error)?
                .ok_or_else(|| DriveError::NotFound {
                    detail: format!("account {account_id} does not exist"),
                })?;
        let pending = read
            .retention_purge_queue(account, u32::MAX)
            .map_err(state_error)?
            .len();
        let archive_backfill = read
            .archive_backfill_progress(account)
            .map_err(state_error)?;
        Ok(ContentPolicyStatus {
            account_id,
            retention: record.retention_mode.into(),
            archive_mode: record.archive_mode,
            pending_file_purges: count(pending)?,
            archive_backfill_pending_allowed_items: archive_backfill.pending_allowed_items,
            archive_backfill_failed_allowed_items: archive_backfill.failed_allowed_items,
            archive_backfill_failure_category: archive_backfill.failure_category,
        })
    }

    /// Commits a retention transition. Audit-to-Mirror accepts only the exact
    /// account-specific typed phrase; cancel is represented by not calling.
    pub fn set_retention(
        &self,
        account_id: i64,
        target: RetentionSelection,
        typed_confirmation: Option<String>,
        now_ms: i64,
    ) -> Result<RetentionTransitionReport, DriveError> {
        let account = account_key(account_id);
        let confirmation = typed_confirmation
            .as_deref()
            .map(|typed| AuditToMirrorConfirmation::parse(account, typed))
            .transpose()
            .map_err(state_error)?;
        let mut store = self.open_store()?;
        let tx = store.write_txn().map_err(state_error)?;
        let change = tx
            .set_retention_mode(account, target.into(), confirmation, now_ms)
            .map_err(state_error)?;
        tx.commit().map_err(state_error)?;

        // The database transition is authoritative even if filesystem cleanup
        // is interrupted. A failure leaves queue rows for this method or agent
        // startup to retry and status continues to report the remainder.
        let acknowledged_file_purges =
            Hydrator::shared(&self.data_root)?.resume_retention_purge(account)?;
        report(change, acknowledged_file_purges)
    }

    /// Toggles eager byte ownership without changing retention.
    pub fn set_archive_mode(
        &self,
        account_id: i64,
        enabled: bool,
        now_ms: i64,
    ) -> Result<ArchiveModeTransitionReport, DriveError> {
        let account = account_key(account_id);
        let mut store = self.open_store()?;
        let tx = store.write_txn().map_err(state_error)?;
        let change = tx
            .set_archive_mode(account, enabled, now_ms)
            .map_err(state_error)?;
        tx.commit().map_err(state_error)?;
        Ok(ArchiveModeTransitionReport {
            previous: change.previous,
            current: change.current,
            pinned_items: count(change.pinned_items)?,
            released_items: count(change.released_items)?,
        })
    }

    /// Explicit relaunch/repair hook for an interrupted file purge.
    pub fn resume_retention_purge(&self, account_id: i64) -> Result<u64, DriveError> {
        Hydrator::shared(&self.data_root)?.resume_retention_purge(account_key(account_id))
    }
}

impl ContentPolicyController {
    fn open_store(&self) -> Result<StateStore, DriveError> {
        let layout = shared_state_layout(self.data_root.clone())?;
        std::fs::create_dir_all(&layout.state_dir).map_err(|error| DriveError::Storage {
            detail: format!("create shared state directory: {error}"),
        })?;
        StateStore::open(&layout.database_file).map_err(state_error)
    }
}

fn account_key(account_id: i64) -> AccountKey {
    AccountKey {
        account_id: AccountId(account_id),
    }
}

fn report(
    change: RetentionChange,
    acknowledged_file_purges: u64,
) -> Result<RetentionTransitionReport, DriveError> {
    Ok(RetentionTransitionReport {
        previous: change.previous.into(),
        current: change.current.into(),
        purged_events: count(change.purged_events)?,
        purged_attachments: count(change.purged_attachments)?,
        purged_attachment_versions: count(change.purged_attachment_versions)?,
        purged_stories: count(change.purged_stories)?,
        purged_blobs: count(change.purged_blobs)?,
        purged_cache_entries: count(change.purged_cache_entries)?,
        released_pins: count(change.released_pins)?,
        invalidated_items: count(change.invalidated_items)?,
        invalidated_documents: count(change.invalidated_docs)?,
        acknowledged_file_purges,
    })
}

fn count(value: usize) -> Result<u64, DriveError> {
    u64::try_from(value).map_err(|_| DriveError::Internal {
        detail: "policy change count exceeds u64".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use gramdrive_model::identity::{
        AccountScope, AttachmentIndex, AttachmentKey, CanonicalKey, ChatId, ChatKey, ItemKey,
        MessageId, MessageKey, NamespaceVersion,
    };
    use gramdrive_model::version::{ContentVersion, MetadataVersion};
    use gramdrive_state::repo::{
        AccountRecord, FailureCategory, FileFacts, ItemAvailability, ItemRecord, SourceKind,
        TransferFailure,
    };

    use super::*;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "gramdrive-policy-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }

        fn text(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn seed_account(controller: &ContentPolicyController, mode: RetentionMode) {
        let mut store = controller.open_store().expect("open state");
        let tx = store.write_txn().expect("write");
        tx.upsert_account(&AccountRecord {
            account: account_key(7),
            source_kind: SourceKind::LocalTdlib,
            display_name: "Policy Test".to_owned(),
            auth_state: "authorized".to_owned(),
            namespace_version: NamespaceVersion(1),
            display_timezone: "UTC".to_owned(),
            retention_mode: mode,
            archive_mode: false,
            secret_ref: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        })
        .expect("account");
        tx.commit().expect("commit");
    }

    #[test]
    fn policy_control_reports_durable_independent_state_and_requires_typed_purge() {
        let root = TempRoot::new();
        let controller = ContentPolicyController::new(root.text()).expect("controller");
        seed_account(&controller, RetentionMode::Audit);

        assert_eq!(
            controller.status(7).expect("status"),
            ContentPolicyStatus {
                account_id: 7,
                retention: RetentionSelection::Audit,
                archive_mode: false,
                pending_file_purges: 0,
                archive_backfill_pending_allowed_items: 0,
                archive_backfill_failed_allowed_items: 0,
                archive_backfill_failure_category: None,
            }
        );
        assert!(
            controller
                .set_retention(7, RetentionSelection::Mirror, None, 2)
                .is_err(),
            "a bool-like unconfirmed transition must be rejected"
        );
        assert_eq!(
            controller
                .status(7)
                .expect("status after rejection")
                .retention,
            RetentionSelection::Audit
        );

        controller
            .set_archive_mode(7, true, 3)
            .expect("enable Archive Mode");
        let status = controller.status(7).expect("independent status");
        assert_eq!(status.retention, RetentionSelection::Audit);
        assert!(status.archive_mode);

        let phrase = controller.audit_to_mirror_confirmation_phrase(7);
        let report = controller
            .set_retention(7, RetentionSelection::Mirror, Some(phrase), 4)
            .expect("confirmed transition");
        assert_eq!(report.previous, RetentionSelection::Audit);
        assert_eq!(report.current, RetentionSelection::Mirror);
        let status = controller.status(7).expect("committed status");
        assert_eq!(status.retention, RetentionSelection::Mirror);
        assert!(
            status.archive_mode,
            "retention must not toggle Archive Mode"
        );
    }

    #[test]
    fn explicit_repair_is_idempotent_after_automatic_startup_replay() {
        let root = TempRoot::new();
        let controller = ContentPolicyController::new(root.text()).expect("controller");
        seed_account(&controller, RetentionMode::Mirror);
        let layout = shared_state_layout(root.text()).expect("layout");
        let blob_dir = PathBuf::from(&layout.cache_dir).join("blobs/sha256");
        std::fs::create_dir_all(&blob_dir).expect("blob directory");
        let object = blob_dir.join("audit-only-object");
        std::fs::write(&object, b"retained bytes").expect("object");
        let reference = object.to_string_lossy().into_owned();
        let store = controller.open_store().expect("open state");
        store
            .connection()
            .execute(
                "INSERT INTO retention_purge_queue (
                     account_id, materialization_ref, queued_at_ms)
                 VALUES (7, ?1, 10)",
                [&reference],
            )
            .expect("queue purge");
        drop(store);

        assert_eq!(
            controller.resume_retention_purge(7).expect("resume"),
            0,
            "constructing the production hydrator drains the queue first"
        );
        assert!(!object.exists());
        assert_eq!(controller.status(7).expect("status").pending_file_purges, 0);
        assert_eq!(
            controller
                .resume_retention_purge(7)
                .expect("idempotent resume"),
            0
        );
    }

    #[test]
    fn production_status_reports_archive_backfill_and_durable_failure() {
        let root = TempRoot::new();
        let controller = ContentPolicyController::new(root.text()).expect("controller");
        seed_account(&controller, RetentionMode::Mirror);
        let scope = AccountScope {
            account: account_key(7),
            namespace_version: NamespaceVersion(1),
        };
        let item = ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
            message: MessageKey {
                chat: ChatKey {
                    scope,
                    chat_id: ChatId(9),
                },
                message_id: MessageId(11),
            },
            index: AttachmentIndex(0),
        }))
        .id();
        let root_item = ItemKey::Canonical(CanonicalKey::Account(scope.account)).id();
        let content_version = ContentVersion::new("archive-content-v1").expect("content version");
        let mut store = controller.open_store().expect("open state");
        let tx = store.write_txn().expect("write candidate");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: root_item.clone(),
            parent: None,
            display_name: "Policy Test".to_owned(),
            safe_name: "Policy Test".to_owned(),
            metadata_version: MetadataVersion::new("account-metadata-v1")
                .expect("account metadata version"),
            content: None,
            availability: ItemAvailability::Fetchable,
            created_at_ms: None,
            modified_at_ms: None,
            deleted_at_ms: None,
        })
        .expect("root item");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: item.clone(),
            parent: Some(root_item),
            display_name: "archive.bin".to_owned(),
            safe_name: "archive.bin".to_owned(),
            metadata_version: MetadataVersion::new("archive-metadata-v1")
                .expect("metadata version"),
            content: Some(FileFacts {
                mime_type: Some("application/octet-stream".to_owned()),
                logical_size: Some(64),
                content_version: Some(content_version.clone()),
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: None,
            modified_at_ms: None,
            deleted_at_ms: None,
        })
        .expect("item");
        tx.set_archive_mode(scope.account, true, 2)
            .expect("enable Archive Mode");
        let transfer = tx
            .enqueue_transfer(&item, &content_version, &[], 1, 3)
            .expect("enqueue")
            .transfer_id();
        tx.mark_transfer_failed(
            transfer,
            FailureCategory::DiskFull,
            TransferFailure::Final,
            4,
        )
        .expect("fail transfer");
        tx.commit().expect("commit candidate");

        let status = controller.status(7).expect("production status");
        assert!(status.archive_mode);
        assert_eq!(status.archive_backfill_pending_allowed_items, 1);
        assert_eq!(status.archive_backfill_failed_allowed_items, 1);
        assert_eq!(
            status.archive_backfill_failure_category.as_deref(),
            Some("disk_full")
        );
    }
}
