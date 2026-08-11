//! POL-3 edit/delete retention mapping (TASK-260715-37nhe5; DEC-015).
//!
//! The append-only event log is the canonical store in both retention modes;
//! the mode governs *content*, applied as the schema's single sanctioned
//! payload purge. These suites pin the mapping deterministically:
//!
//! * **Audit** keeps every observed revision and preserves a deleted
//!   message's content behind its tombstone.
//! * **Mirror** keeps only current Telegram state — an edit replaces prior
//!   revisions, an observed deletion purges the message's content, and only
//!   id/timestamp markers survive for sync correctness.
//! * A **mid-life mode switch** applies the Mirror invariant retroactively,
//!   recovers nothing already purged, and invalidates the account's rendered
//!   documents (the mode is stamped in every document header).
//!
//! Cache eviction is deliberately untouched here — that is POL-2 accounting on
//! a separate axis (see `repo_cache_render.rs`).

// clippy.toml exempts test code on the grounds that a panicking test is just
// a failing test. That exemption keys on `#[test]` functions, and the shared
// fixture helpers below sit at module level in an integration-test binary.
// The rationale still applies in full — this file links into no product
// artifact — so the exemption is restated here, matching the established
// test-suite pattern.
#![allow(clippy::expect_used, clippy::panic)]

mod common;

use common::{account_record, chat_record, revision, scope};
use gramdrive_state::model::identity::{
    AccountId, AccountKey, AccountScope, AppearanceKey, AttachmentIndex, AttachmentKey,
    CanonicalKey, ChatId, ChatKey, ChatListKind, ContentHash, DocFormat, DocPartition, ItemId,
    ItemKey, MessageId, MessageKey, NamespaceVersion, StoryAppearanceKey, StoryAppearanceLocation,
    StoryId, StoryKey,
};
use gramdrive_state::model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::repo::{
    AccountRecord, AttachmentAvailability, AttachmentFacts, AttachmentFidelity,
    AttachmentLogicalKind, AuditToMirrorConfirmation, CacheEntryRecord, CacheKind,
    CacheVerification, FileFacts, ItemAvailability, ItemRecord, MessageChange, MessageEventKind,
    MessageState, PinOrigin, RenderOutput, RetentionMode, TelegramRepresentation,
};
use gramdrive_state::{StateError, StateStore};

const CHAT: i64 = 100;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn account_in(mode: RetentionMode) -> AccountRecord {
    let mut record = account_record();
    record.retention_mode = mode;
    record
}

/// A store whose one account runs in `mode`, with its chat scaffolded.
fn store(mode: RetentionMode) -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&account_in(mode)).expect("account");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    tx.commit().expect("commit");
    store
}

fn chat_key() -> ChatKey {
    common::chat_key(CHAT)
}

fn doc_id() -> ItemId {
    ItemKey::Appearance(AppearanceKey {
        view: ChatListKind::Main,
        item: common::doc_key(CHAT, DocPartition::Year { year: 2026 }, DocFormat::Ndjson),
    })
    .id()
}

/// A store in `mode` with one generated document whose render state has been
/// published clean, so a later invalidation is observable as the dirty bit
/// flipping back.
fn store_with_clean_doc(mode: RetentionMode) -> (StateStore, ItemId) {
    let mut store = StateStore::open_in_memory().expect("open");
    let doc = doc_id();
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&account_in(mode)).expect("account");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    let root = common::account_root_id();
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: root.clone(),
        parent: None,
        display_name: "Root".to_owned(),
        safe_name: "Root".to_owned(),
        metadata_version: MetadataVersion::new("m1").expect("version"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("root");
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: doc.clone(),
        parent: Some(root),
        display_name: "2026.ndjson".to_owned(),
        safe_name: "2026.ndjson".to_owned(),
        metadata_version: MetadataVersion::new("m1").expect("version"),
        content: Some(FileFacts::default()),
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("doc");
    tx.ensure_render_state(&doc, 1, 1).expect("render state");
    tx.commit().expect("commit");
    (store, doc)
}

/// Publishes the document clean at the chat's current watermark.
fn publish_clean(store: &mut StateStore, doc: &ItemId) {
    let tx = store.write_txn().expect("write");
    let watermark = tx.read().latest_event_seq(&chat_key()).expect("seq");
    let publish = tx
        .publish_render(
            doc,
            &chat_key(),
            watermark,
            &RenderOutput {
                content_version: ContentVersion::new("r1-w").expect("version"),
                content_hash: Some(ContentHash::Sha256([1u8; 32])),
                logical_size: 64,
            },
            9_000,
        )
        .expect("publish");
    assert!(publish.clean, "fixture must publish clean");
    tx.commit().expect("commit");
}

// ---------------------------------------------------------------------------
// Change builders
// ---------------------------------------------------------------------------

fn observe(message: i64, sent_at_ms: i64) -> MessageChange {
    MessageChange::Observed(revision(message, sent_at_ms))
}

/// An edit of `message`: same send time, a later edit time, and distinct
/// content tagged so purging is observable.
fn edit(message: i64, sent_at_ms: i64, edited_at_ms: i64, tag: &str) -> MessageChange {
    let mut revision = revision(message, sent_at_ms);
    revision.edited_at_ms = Some(edited_at_ms);
    revision.observed_at_ms = edited_at_ms + 5;
    revision.payload = format!("payload-{message}-{tag}").into_bytes();
    MessageChange::Observed(revision)
}

fn deletion(message: i64, observed_at_ms: i64) -> MessageChange {
    MessageChange::Deleted {
        message_id: MessageId(message),
        observed_at_ms,
    }
}

fn attachment(message: i64) -> AttachmentFacts {
    AttachmentFacts {
        key: AttachmentKey {
            message: MessageKey {
                chat: chat_key(),
                message_id: MessageId(message),
            },
            index: AttachmentIndex(0),
        },
        logical_kind: AttachmentLogicalKind::Document,
        telegram_representation: TelegramRepresentation::OriginalDocument,
        fidelity: AttachmentFidelity::Original,
        source_name: Some("observed.bin".to_owned()),
        mime_type: Some("application/octet-stream".to_owned()),
        exact_size: Some(64),
        content_version: ContentVersion::new("attachment-v1").expect("version"),
        telegram_unique_id: Some(format!("unique-{message}")),
        telegram_local_file_id: Some(700),
        telegram_file_id: Some(format!("remote-{message}")),
        file_reference: Some(vec![1, 2, 3]),
        availability: AttachmentAvailability::Fetchable,
        can_be_saved: true,
    }
}

fn attachment_version(message: i64, version: &str, name: &str) -> AttachmentFacts {
    let mut facts = attachment(message);
    facts.content_version = ContentVersion::new(version).expect("version");
    facts.source_name = Some(name.to_owned());
    facts
}

#[test]
fn authoritative_restriction_redacts_payloads_and_attachment_locators_per_account() {
    let mut store = store(RetentionMode::Audit);
    let mut sibling_account = account_in(RetentionMode::Audit);
    sibling_account.account = AccountKey {
        account_id: AccountId(8),
    };
    sibling_account.display_name = "Sibling".to_owned();
    let sibling_scope = AccountScope {
        account: sibling_account.account,
        namespace_version: NamespaceVersion(1),
    };
    let sibling_chat = ChatKey {
        scope: sibling_scope,
        chat_id: ChatId(CHAT),
    };
    let sibling_message = MessageKey {
        chat: sibling_chat,
        message_id: MessageId(1),
    };
    let mut sibling_chat_record = chat_record(CHAT);
    sibling_chat_record.key = sibling_chat;
    let mut primary_attachment = attachment(1);
    primary_attachment.source_name = Some("primary-secret.bin".to_owned());
    let mut sibling_attachment = primary_attachment.clone();
    sibling_attachment.key = AttachmentKey {
        message: sibling_message,
        index: AttachmentIndex(0),
    };
    sibling_attachment.source_name = Some("sibling-safe.bin".to_owned());

    let tx = store.write_txn().expect("seed restricted fixtures");
    tx.upsert_account(&sibling_account)
        .expect("sibling account");
    tx.upsert_chat(&sibling_chat_record).expect("sibling chat");
    tx.apply_message_changes(&chat_key(), &[observe(1, 1_000)])
        .expect("primary message");
    let mut sibling_revision = revision(1, 1_000);
    sibling_revision.payload = b"sibling-payload".to_vec();
    tx.apply_message_changes(&sibling_chat, &[MessageChange::Observed(sibling_revision)])
        .expect("sibling message");
    tx.upsert_attachment(&primary_attachment)
        .expect("primary attachment");
    tx.upsert_attachment(&sibling_attachment)
        .expect("sibling attachment");
    assert_eq!(
        tx.purge_restricted_chat_message_content(&chat_key())
            .expect("purge primary payload"),
        1
    );
    assert_eq!(
        tx.redact_protected_chat_attachments(&chat_key())
            .expect("redact primary attachment"),
        1
    );
    assert_eq!(
        tx.purge_restricted_chat_message_content(&chat_key())
            .expect("repeat payload purge"),
        0
    );
    assert_eq!(
        tx.redact_protected_chat_attachments(&chat_key())
            .expect("repeat attachment redaction"),
        0
    );
    tx.commit().expect("commit restriction");

    let read = store.read_txn().expect("read restriction");
    let primary_event = read
        .events_after(&chat_key(), 0, 10)
        .expect("primary events")
        .pop()
        .expect("primary event");
    assert_eq!(primary_event.payload, None);
    let primary = read
        .attachment(&primary_attachment.key)
        .expect("primary attachment")
        .expect("primary row");
    assert_eq!(primary.facts.source_name, None);
    assert_eq!(primary.facts.mime_type, None);
    assert_eq!(primary.facts.exact_size, None);
    assert_eq!(primary.facts.telegram_unique_id, None);
    assert_eq!(primary.facts.telegram_local_file_id, None);
    assert_eq!(primary.facts.telegram_file_id, None);
    assert_eq!(primary.facts.file_reference, None);
    assert_eq!(
        primary.facts.availability,
        AttachmentAvailability::Restricted
    );
    assert!(!primary.facts.can_be_saved);
    assert_eq!(primary.blob_hash, None);

    let sibling_event = read
        .events_after(&sibling_chat, 0, 10)
        .expect("sibling events")
        .pop()
        .expect("sibling event");
    assert_eq!(
        sibling_event
            .payload
            .as_ref()
            .map(|payload| payload.bytes.as_slice()),
        Some(b"sibling-payload".as_slice())
    );
    let sibling = read
        .attachment(&sibling_attachment.key)
        .expect("sibling attachment")
        .expect("sibling row");
    assert_eq!(
        sibling.facts.source_name.as_deref(),
        Some("sibling-safe.bin")
    );
    assert_eq!(sibling.facts.telegram_local_file_id, Some(700));
}

fn materialize_attachment(
    store: &mut StateStore,
    facts: &AttachmentFacts,
    hash: ContentHash,
    reference: &str,
) -> ItemId {
    let item = ItemKey::Canonical(CanonicalKey::Attachment(facts.key)).id();
    let tx = store.write_txn().expect("materialize attachment");
    tx.upsert_attachment(facts).expect("attachment");
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: common::account_root_id(),
        parent: None,
        display_name: "Root".to_owned(),
        safe_name: "Root".to_owned(),
        metadata_version: MetadataVersion::new("root-v1").expect("version"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("root");
    tx.record_blob(facts.key.message.chat.scope.account, &hash, 64, 1_100)
        .expect("blob");
    tx.link_attachment_blob(&facts.key, &hash, 1_200)
        .expect("link");
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: item.clone(),
        parent: Some(common::account_root_id()),
        display_name: facts.source_name.clone().expect("source name"),
        safe_name: facts.source_name.clone().expect("safe name"),
        metadata_version: MetadataVersion::new("attachment-meta-v1").expect("version"),
        content: Some(FileFacts {
            mime_type: facts.mime_type.clone(),
            logical_size: facts.exact_size,
            content_version: Some(facts.content_version.clone()),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: Some(1_000),
        modified_at_ms: Some(1_000),
        deleted_at_ms: None,
    })
    .expect("item");
    tx.upsert_cache_entry(&CacheEntryRecord {
        item: item.clone(),
        account: facts.key.message.chat.scope.account,
        content_version: facts.content_version.clone(),
        kind: CacheKind::Blob,
        size: 64,
        blob_hash: Some(hash),
        verification: CacheVerification::Verified,
        pin: None,
        last_access_at_ms: 1_200,
        materialized_at_ms: 1_200,
        materialization_ref: Some(reference.to_owned()),
    })
    .expect("cache");
    tx.commit().expect("commit materialization");
    item
}

fn apply(store: &mut StateStore, changes: &[MessageChange]) {
    let tx = store.write_txn().expect("write");
    tx.apply_message_changes(&chat_key(), changes)
        .expect("apply");
    tx.commit().expect("commit");
}

#[test]
fn attachment_version_replacement_purges_mirror_and_retains_only_materialized_audit_bytes() {
    let old = attachment_version(82, "attachment-v1", "first.bin");
    let new = attachment_version(82, "attachment-v2", "second.bin");
    let hash = ContentHash::Sha256([0x82; 32]);

    let mut mirror = store(RetentionMode::Mirror);
    apply(&mut mirror, &[observe(82, 1_000)]);
    let mirror_item = materialize_attachment(&mut mirror, &old, hash, "blobs/sha256/mirror-82");
    let tx = mirror.write_txn().expect("replace Mirror version");
    tx.replace_message_attachments(&old.key.message, std::slice::from_ref(&new), 2_000)
        .expect("replace");
    tx.commit().expect("commit replacement");
    let read = mirror.read_txn().expect("read Mirror replacement");
    assert!(
        read.retained_attachment_versions(&old.key)
            .expect("retained versions")
            .is_empty()
    );
    assert!(read.cache_entry(&mirror_item).expect("cache").is_none());
    assert_eq!(
        read.retention_purge_queue(scope().account, 10)
            .expect("purge queue")
            .iter()
            .map(|entry| entry.materialization_ref.as_str())
            .collect::<Vec<_>>(),
        vec!["blobs/sha256/mirror-82"]
    );
    assert_eq!(
        read.attachment(&new.key)
            .expect("attachment")
            .expect("current")
            .facts
            .content_version,
        new.content_version
    );
    drop(read);

    let db = common::TempDb::new();
    let mut audit = StateStore::open(&db.path).expect("open Audit store");
    let tx = audit.write_txn().expect("seed Audit");
    tx.upsert_account(&account_in(RetentionMode::Audit))
        .expect("account");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    tx.commit().expect("commit seed");
    apply(&mut audit, &[observe(82, 1_000)]);
    let audit_item = materialize_attachment(&mut audit, &old, hash, "blobs/sha256/audit-82");
    let tx = audit.write_txn().expect("replace Audit version");
    tx.replace_message_attachments(&old.key.message, std::slice::from_ref(&new), 2_000)
        .expect("replace");
    tx.commit().expect("commit replacement");
    drop(audit);

    let mut audit = StateStore::open(&db.path).expect("relaunch Audit store");
    let read = audit.read_txn().expect("read retained version");
    let retained = read
        .retained_attachment_versions(&old.key)
        .expect("retained versions");
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].content_version, old.content_version);
    assert_eq!(retained[0].source_name.as_deref(), Some("first.bin"));
    assert_eq!(retained[0].blob_hash, Some(hash));
    assert_eq!(retained[0].materialized_size, Some(64));
    assert_eq!(
        retained[0].materialization_ref.as_deref(),
        Some("blobs/sha256/audit-82")
    );
    assert!(
        read.cache_entry(&audit_item)
            .expect("current cache")
            .is_none(),
        "a superseded cache row must never satisfy the new content version"
    );
    assert!(
        read.retention_purge_queue(scope().account, 10)
            .expect("purge queue")
            .is_empty(),
        "Audit keeps already materialized allowed bytes"
    );
    assert_eq!(read.cache_totals().expect("totals").pinned_bytes, 64);
    assert!(
        read.eviction_candidates_after(None, 10)
            .expect("eviction candidates")
            .is_empty(),
        "Audit-retained bytes are durable policy owners, not quota victims"
    );
    drop(read);

    let confirmation = AuditToMirrorConfirmation::parse(
        scope().account,
        &AuditToMirrorConfirmation::expected_phrase(scope().account),
    )
    .expect("typed confirmation");
    let tx = audit.write_txn().expect("destructive transition");
    let report = tx
        .set_retention_mode(
            scope().account,
            RetentionMode::Mirror,
            Some(confirmation),
            3_000,
        )
        .expect("Audit to Mirror");
    tx.commit().expect("commit purge");
    assert_eq!(report.purged_attachment_versions, 1);
    let read = audit.read_txn().expect("read purge result");
    assert!(
        read.retained_attachment_versions(&old.key)
            .expect("retained versions")
            .is_empty()
    );
    assert_eq!(
        read.retention_purge_queue(scope().account, 10)
            .expect("purge queue")
            .iter()
            .map(|entry| entry.materialization_ref.as_str())
            .collect::<Vec<_>>(),
        vec!["blobs/sha256/audit-82"]
    );
}

#[test]
fn audit_attachment_removal_by_edit_retains_metadata_and_only_observed_bytes() {
    for archive_mode in [false, true] {
        let db = common::TempDb::new();
        let mut audit = StateStore::open(&db.path).expect("open Audit store");
        let tx = audit.write_txn().expect("seed Audit");
        tx.upsert_account(&account_in(RetentionMode::Audit))
            .expect("account");
        tx.upsert_chat(&chat_record(CHAT)).expect("chat");
        tx.commit().expect("commit seed");
        apply(&mut audit, &[observe(83, 1_000), observe(84, 1_100)]);

        let materialized = attachment_version(83, "attachment-83-v1", "materialized.bin");
        let metadata_only = attachment_version(84, "attachment-84-v1", "metadata-only.bin");
        let hash = ContentHash::Sha256([0x83; 32]);
        let materialized_item = materialize_attachment(
            &mut audit,
            &materialized,
            hash,
            "blobs/sha256/audit-removed-83",
        );
        let metadata_item = ItemKey::Canonical(CanonicalKey::Attachment(metadata_only.key)).id();
        let tx = audit.write_txn().expect("seed metadata-only attachment");
        tx.upsert_attachment(&metadata_only)
            .expect("metadata-only attachment");
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: metadata_item.clone(),
            parent: Some(common::account_root_id()),
            display_name: metadata_only.source_name.clone().expect("source name"),
            safe_name: metadata_only.source_name.clone().expect("safe name"),
            metadata_version: MetadataVersion::new("attachment-84-meta-v1")
                .expect("metadata version"),
            content: Some(FileFacts {
                mime_type: metadata_only.mime_type.clone(),
                logical_size: metadata_only.exact_size,
                content_version: Some(metadata_only.content_version.clone()),
            }),
            availability: ItemAvailability::Fetchable,
            created_at_ms: Some(1_100),
            modified_at_ms: Some(1_100),
            deleted_at_ms: None,
        })
        .expect("metadata-only item");
        tx.set_archive_mode(scope().account, archive_mode, 1_500)
            .expect("Archive Mode");
        tx.commit().expect("commit metadata fixture");

        let tx = audit.write_txn().expect("remove attachments by edit");
        tx.replace_message_attachments(&materialized.key.message, &[], 2_000)
            .expect("remove materialized attachment");
        tx.tombstone_item(
            &materialized_item,
            2_000,
            &MetadataVersion::new("attachment-83-removed").expect("metadata version"),
        )
        .expect("tombstone materialized projection");
        tx.replace_message_attachments(&metadata_only.key.message, &[], 2_100)
            .expect("remove metadata-only attachment");
        tx.tombstone_item(
            &metadata_item,
            2_100,
            &MetadataVersion::new("attachment-84-removed").expect("metadata version"),
        )
        .expect("tombstone metadata-only projection");
        tx.commit().expect("commit removals");
        drop(audit);

        let mut audit = StateStore::open(&db.path).expect("relaunch Audit store");
        let read = audit.read_txn().expect("read retained removals");
        assert!(
            read.attachment(&materialized.key)
                .expect("materialized attachment")
                .is_none()
        );
        assert!(
            read.attachment(&metadata_only.key)
                .expect("metadata attachment")
                .is_none()
        );
        let retained_materialized = read
            .retained_attachment_versions(&materialized.key)
            .expect("materialized retained version");
        assert_eq!(retained_materialized.len(), 1);
        assert_eq!(
            retained_materialized[0].source_name.as_deref(),
            Some("materialized.bin")
        );
        assert_eq!(retained_materialized[0].blob_hash, Some(hash));
        assert_eq!(
            retained_materialized[0].materialization_ref.as_deref(),
            Some("blobs/sha256/audit-removed-83")
        );
        let retained_metadata = read
            .retained_attachment_versions(&metadata_only.key)
            .expect("metadata retained version");
        assert_eq!(retained_metadata.len(), 1);
        assert_eq!(
            retained_metadata[0].source_name.as_deref(),
            Some("metadata-only.bin")
        );
        assert_eq!(retained_metadata[0].blob_hash, None);
        assert_eq!(retained_metadata[0].materialization_ref, None);
        assert!(
            read.cache_entry(&materialized_item)
                .expect("materialized cache")
                .is_none()
        );
        assert!(
            read.archive_backfill_candidates(scope().account, 10)
                .expect("Archive worklist")
                .is_empty(),
            "locator-free retained metadata must never create download demand"
        );
        assert!(
            read.eviction_candidates_after(None, 10)
                .expect("eviction candidates")
                .is_empty(),
            "Audit-retained bytes remain policy-owned after projection tombstones"
        );
        assert_eq!(read.cache_totals().expect("cache totals").pinned_bytes, 64);
        assert!(
            read.materialization_ref_referenced("blobs/sha256/audit-removed-83")
                .expect("materialization ownership")
        );
        assert!(
            read.pin(&materialized_item)
                .expect("materialized pin")
                .is_none()
        );
        assert!(read.pin(&metadata_item).expect("metadata pin").is_none());
        assert_eq!(
            read.retained_attachment_keys(scope().account)
                .expect("retained attachment keys"),
            vec![materialized.key, metadata_only.key]
        );
        drop(read);

        let tx = audit.write_txn().expect("disable Archive independently");
        tx.set_archive_mode(scope().account, false, 2_500)
            .expect("disable Archive");
        tx.commit().expect("commit Archive change");
        assert_eq!(
            audit
                .read_txn()
                .expect("read retained after Archive change")
                .retained_attachment_versions(&materialized.key)
                .expect("retained version")
                .len(),
            1,
            "Archive Mode changes must not release Audit-owned bytes"
        );

        let confirmation = AuditToMirrorConfirmation::parse(
            scope().account,
            &AuditToMirrorConfirmation::expected_phrase(scope().account),
        )
        .expect("typed confirmation");
        let tx = audit.write_txn().expect("destructive transition");
        let report = tx
            .set_retention_mode(
                scope().account,
                RetentionMode::Mirror,
                Some(confirmation),
                3_000,
            )
            .expect("Audit to Mirror");
        tx.commit().expect("commit destructive transition");
        assert_eq!(report.purged_attachment_versions, 2);
        assert_eq!(report.queued_file_purges, 1);
        let read = audit.read_txn().expect("read destructive purge");
        assert!(
            read.retained_attachment_versions(&materialized.key)
                .expect("materialized retained versions")
                .is_empty()
        );
        assert!(
            read.retained_attachment_versions(&metadata_only.key)
                .expect("metadata retained versions")
                .is_empty()
        );
        assert!(
            read.retained_attachment_keys(scope().account)
                .expect("retained attachment keys")
                .is_empty()
        );
        assert_eq!(
            read.retention_purge_queue(scope().account, 10)
                .expect("purge queue")
                .iter()
                .map(|entry| entry.materialization_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["blobs/sha256/audit-removed-83"]
        );
    }
}

// ---------------------------------------------------------------------------
// Inspection
// ---------------------------------------------------------------------------

/// Every event of one message as `(kind, payload bytes)`, in sequence order.
fn message_events(
    store: &mut StateStore,
    message: i64,
) -> Vec<(MessageEventKind, Option<Vec<u8>>)> {
    store
        .read_txn()
        .expect("read")
        .events_after(&chat_key(), 0, 10_000)
        .expect("events")
        .into_iter()
        .filter(|event| event.message_id == MessageId(message))
        .map(|event| (event.kind, event.payload.map(|payload| payload.bytes)))
        .collect()
}

fn state(store: &mut StateStore, message: i64) -> MessageState {
    store
        .read_txn()
        .expect("read")
        .message(&MessageKey {
            chat: chat_key(),
            message_id: MessageId(message),
        })
        .expect("message")
        .expect("some")
}

fn is_dirty(store: &mut StateStore, doc: &ItemId) -> bool {
    store
        .read_txn()
        .expect("read")
        .render_state(doc)
        .expect("state")
        .expect("some")
        .dirty
}

fn payload(bytes: &str) -> Option<Vec<u8>> {
    Some(bytes.as_bytes().to_vec())
}

// ---------------------------------------------------------------------------
// Audit mode: everything observed is retained
// ---------------------------------------------------------------------------

#[test]
fn audit_retains_every_revision_and_a_deleted_message_content() {
    let mut store = store(RetentionMode::Audit);
    apply(
        &mut store,
        &[
            observe(1, 1_000),
            edit(1, 1_000, 5_000, "b"),
            edit(1, 1_000, 6_000, "c"),
        ],
    );

    // The whole edit chain survives as distinct revisions.
    assert_eq!(
        message_events(&mut store, 1),
        vec![
            (MessageEventKind::Observed, payload("payload-1")),
            (MessageEventKind::Edited, payload("payload-1-b")),
            (MessageEventKind::Edited, payload("payload-1-c")),
        ]
    );

    // Deleting it appends a content-free tombstone but preserves the
    // revisions behind it (POL-3 content-preserving tombstone).
    apply(&mut store, &[deletion(1, 7_000)]);
    assert_eq!(
        message_events(&mut store, 1),
        vec![
            (MessageEventKind::Observed, payload("payload-1")),
            (MessageEventKind::Edited, payload("payload-1-b")),
            (MessageEventKind::Edited, payload("payload-1-c")),
            (MessageEventKind::Deleted, None),
        ]
    );
    assert!(state(&mut store, 1).is_deleted);
}

// ---------------------------------------------------------------------------
// Mirror mode: only current Telegram state
// ---------------------------------------------------------------------------

#[test]
fn mirror_edit_chain_keeps_only_the_current_revision() {
    let mut store = store(RetentionMode::Mirror);
    apply(
        &mut store,
        &[
            observe(1, 1_000),
            edit(1, 1_000, 5_000, "b"),
            edit(1, 1_000, 6_000, "c"),
        ],
    );

    // Prior revisions are purged to markers; only the latest keeps content.
    // The event rows themselves remain, so watermarks never rewind.
    assert_eq!(
        message_events(&mut store, 1),
        vec![
            (MessageEventKind::Observed, None),
            (MessageEventKind::Edited, None),
            (MessageEventKind::Edited, payload("payload-1-c")),
        ]
    );

    // The projection still reflects the current revision — the replay guard
    // reads its retained payload, so a stale re-observation is still caught.
    let current = state(&mut store, 1);
    assert_eq!(current.edited_at_ms, Some(6_000));
    apply(&mut store, &[observe(1, 1_000)]);
    assert_eq!(
        message_events(&mut store, 1).len(),
        3,
        "a stale re-observation must not append or resurrect content"
    );
}

#[test]
fn mirror_deletion_purges_all_of_a_messages_content() {
    let mut store = store(RetentionMode::Mirror);
    apply(
        &mut store,
        &[
            observe(1, 1_000),
            edit(1, 1_000, 5_000, "b"),
            deletion(1, 6_000),
        ],
    );

    // Nothing of the message's content remains anywhere — only the markers.
    assert_eq!(
        message_events(&mut store, 1),
        vec![
            (MessageEventKind::Observed, None),
            (MessageEventKind::Edited, None),
            (MessageEventKind::Deleted, None),
        ]
    );
    let current = state(&mut store, 1);
    assert!(current.is_deleted);
    // The projection points at the tombstone — the current state is "deleted".
    let read = store.read_txn().expect("read");
    assert_eq!(
        current.latest_event_seq,
        read.latest_event_seq(&chat_key()).expect("seq")
    );
}

// ---------------------------------------------------------------------------
// delete-for-everyone vs delete-for-me: one observation, one mapping
// ---------------------------------------------------------------------------

#[test]
fn observed_deletion_maps_identically_whatever_telegram_delete_scope_was() {
    // A delete-for-everyone (revoke) and a delete-for-me both reach the engine
    // as `updateDeleteMessages` with is_permanent set and from_cache clear
    // (gramdrive-source-tdjson::live) — TDLib exposes no scope flag, and the
    // archive mirrors this account's own view, in which both are permanent
    // removals. So both normalize to one `MessageChange::Deleted` and map
    // through exactly the same path. Here message 1 stands for the
    // delete-for-everyone case and message 2 for delete-for-me; the retention
    // mapping cannot and does not tell them apart.
    let mut mirror = store(RetentionMode::Mirror);
    apply(&mut mirror, &[observe(1, 1_000), observe(2, 2_000)]);
    apply(&mut mirror, &[deletion(1, 3_000), deletion(2, 3_000)]);
    for message in [1, 2] {
        assert!(state(&mut mirror, message).is_deleted, "both tombstoned");
        assert_eq!(
            message_events(&mut mirror, message),
            vec![
                (MessageEventKind::Observed, None),
                (MessageEventKind::Deleted, None),
            ],
            "both purge content in Mirror, with no per-scope distinction"
        );
    }

    // Audit records the same single observation as a content-preserving
    // tombstone for each — again identical between the two delete scopes.
    let mut audit = store(RetentionMode::Audit);
    apply(&mut audit, &[observe(1, 1_000), observe(2, 2_000)]);
    apply(&mut audit, &[deletion(1, 3_000), deletion(2, 3_000)]);
    for message in [1, 2] {
        assert_eq!(
            message_events(&mut audit, message),
            vec![
                (
                    MessageEventKind::Observed,
                    payload(&format!("payload-{message}"))
                ),
                (MessageEventKind::Deleted, None),
            ]
        );
    }
}

#[test]
fn mirror_deletion_purges_attachment_metadata_while_audit_retains_it() {
    for (mode, retained) in [(RetentionMode::Mirror, false), (RetentionMode::Audit, true)] {
        let mut store = store(mode);
        apply(&mut store, &[observe(41, 1_000)]);
        let facts = attachment(41);
        let tx = store.write_txn().expect("write attachment");
        tx.upsert_attachment(&facts).expect("attachment");
        tx.commit().expect("commit attachment");

        apply(&mut store, &[deletion(41, 2_000)]);
        assert_eq!(
            store
                .read_txn()
                .expect("read")
                .attachment(&facts.key)
                .expect("attachment")
                .is_some(),
            retained,
            "{mode:?} attachment retention"
        );
    }
}

// ---------------------------------------------------------------------------
// Mid-life mode switch
// ---------------------------------------------------------------------------

#[test]
fn switching_to_mirror_purges_retained_history_and_invalidates_documents() {
    let (mut store, doc) = store_with_clean_doc(RetentionMode::Audit);
    // A live edited message, a deleted message with retained content, and a
    // pristine live message — everything Audit keeps.
    apply(
        &mut store,
        &[
            observe(1, 1_000),
            edit(1, 1_000, 5_000, "b"),
            observe(2, 2_000),
            deletion(2, 6_000),
            observe(3, 3_000),
        ],
    );
    publish_clean(&mut store, &doc);
    assert!(!is_dirty(&mut store, &doc));

    let change = {
        let tx = store.write_txn().expect("write");
        let change = tx
            .set_retention_mode(
                scope().account,
                RetentionMode::Mirror,
                Some(
                    AuditToMirrorConfirmation::parse(
                        scope().account,
                        &AuditToMirrorConfirmation::expected_phrase(scope().account),
                    )
                    .expect("confirmation"),
                ),
                10_000,
            )
            .expect("switch");
        tx.commit().expect("commit");
        change
    };

    assert!(change.changed());
    assert_eq!(change.previous, RetentionMode::Audit);
    assert_eq!(change.current, RetentionMode::Mirror);
    // Two payloads purged: message 1's superseded first revision, and
    // message 2's now-deleted content. Message 3's single live revision and
    // message 1's current revision are kept.
    assert_eq!(change.purged_events, 2);
    assert!(change.invalidated_docs >= 1);

    // Message 1: only the current revision survives.
    assert_eq!(
        message_events(&mut store, 1),
        vec![
            (MessageEventKind::Observed, None),
            (MessageEventKind::Edited, payload("payload-1-b")),
        ]
    );
    // Message 2: deleted, all content gone.
    assert_eq!(
        message_events(&mut store, 2),
        vec![
            (MessageEventKind::Observed, None),
            (MessageEventKind::Deleted, None),
        ]
    );
    // Message 3: live and untouched.
    assert_eq!(
        message_events(&mut store, 3),
        vec![(MessageEventKind::Observed, payload("payload-3"))]
    );
    // The published document is stale — it rendered content now purged.
    assert!(is_dirty(&mut store, &doc));
}

#[test]
fn audit_to_mirror_requires_typed_account_confirmation_without_partial_mutation() {
    let mut store = store(RetentionMode::Audit);
    apply(&mut store, &[observe(71, 1_000), deletion(71, 2_000)]);

    let tx = store.write_txn().expect("write unconfirmed");
    assert!(matches!(
        tx.set_retention_mode(scope().account, RetentionMode::Mirror, None, 3_000),
        Err(StateError::InvalidArgument { .. })
    ));
    drop(tx);

    let wrong_account = AccountKey {
        account_id: AccountId(8),
    };
    let wrong_confirmation = AuditToMirrorConfirmation::parse(
        wrong_account,
        &AuditToMirrorConfirmation::expected_phrase(wrong_account),
    )
    .expect("well-formed wrong-account confirmation");
    let tx = store.write_txn().expect("write wrong account");
    assert!(matches!(
        tx.set_retention_mode(
            scope().account,
            RetentionMode::Mirror,
            Some(wrong_confirmation),
            3_000,
        ),
        Err(StateError::InvalidArgument { .. })
    ));
    drop(tx);

    assert_eq!(
        store
            .read_txn()
            .expect("read mode")
            .retention_mode(scope().account)
            .expect("mode"),
        Some(RetentionMode::Audit)
    );
    assert_eq!(
        message_events(&mut store, 71),
        vec![
            (MessageEventKind::Observed, payload("payload-71")),
            (MessageEventKind::Deleted, None),
        ],
        "rejected confirmations must leave Audit content untouched"
    );
}

#[test]
fn destructive_retention_transition_isolated_to_the_confirmed_account() {
    let mut store = StateStore::open_in_memory().expect("open");
    let account_two = AccountKey {
        account_id: AccountId(8),
    };
    let mut second_account = account_in(RetentionMode::Audit);
    second_account.account = account_two;
    second_account.display_name = "Second Account".to_owned();
    let second_chat = ChatKey {
        scope: AccountScope {
            account: account_two,
            namespace_version: scope().namespace_version,
        },
        chat_id: ChatId(200),
    };
    let mut second_chat_record = chat_record(200);
    second_chat_record.key = second_chat;
    let tx = store.write_txn().expect("seed accounts");
    tx.upsert_account(&account_in(RetentionMode::Audit))
        .expect("first account");
    tx.upsert_account(&second_account).expect("second account");
    tx.upsert_chat(&chat_record(CHAT)).expect("first chat");
    tx.upsert_chat(&second_chat_record).expect("second chat");
    tx.commit().expect("commit accounts");

    let tx = store.write_txn().expect("observe both");
    tx.apply_message_changes(&chat_key(), &[observe(72, 1_000), deletion(72, 2_000)])
        .expect("first changes");
    tx.apply_message_changes(&second_chat, &[observe(73, 1_000), deletion(73, 2_000)])
        .expect("second changes");
    tx.commit().expect("commit changes");

    let confirmation = AuditToMirrorConfirmation::parse(
        scope().account,
        &AuditToMirrorConfirmation::expected_phrase(scope().account),
    )
    .expect("confirmation");
    let tx = store.write_txn().expect("purge first");
    tx.set_retention_mode(
        scope().account,
        RetentionMode::Mirror,
        Some(confirmation),
        3_000,
    )
    .expect("transition");
    tx.commit().expect("commit transition");

    let read = store.read_txn().expect("read isolation");
    assert_eq!(
        read.retention_mode(account_two).expect("second mode"),
        Some(RetentionMode::Audit)
    );
    let second_payloads = read
        .events_after(&second_chat, 0, 10)
        .expect("second events")
        .into_iter()
        .map(|event| event.payload.map(|payload| payload.bytes))
        .collect::<Vec<_>>();
    assert_eq!(second_payloads, vec![payload("payload-73"), None]);
}

#[test]
fn audit_to_mirror_purge_queues_files_and_is_crash_idempotent() {
    let db = common::TempDb::new();
    let mut store = StateStore::open(&db.path).expect("open file store");
    let tx = store.write_txn().expect("seed");
    tx.upsert_account(&account_in(RetentionMode::Audit))
        .expect("account");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: common::account_root_id(),
        parent: None,
        display_name: "Root".to_owned(),
        safe_name: "Root".to_owned(),
        metadata_version: MetadataVersion::new("root-v1").expect("version"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("root");
    tx.commit().expect("commit seed");
    apply(&mut store, &[observe(81, 1_000)]);

    let facts = attachment(81);
    let item = ItemKey::Canonical(CanonicalKey::Attachment(facts.key)).id();
    let hash = ContentHash::Sha256([0x81; 32]);
    let tx = store.write_txn().expect("materialize");
    tx.upsert_attachment(&facts).expect("attachment");
    tx.record_blob(scope().account, &hash, 64, 1_100)
        .expect("blob");
    tx.link_attachment_blob(&facts.key, &hash, 1_200)
        .expect("link");
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: item.clone(),
        parent: Some(common::account_root_id()),
        display_name: "observed.bin".to_owned(),
        safe_name: "observed.bin".to_owned(),
        metadata_version: MetadataVersion::new("attachment-meta-v1").expect("version"),
        content: Some(FileFacts {
            mime_type: facts.mime_type.clone(),
            logical_size: facts.exact_size,
            content_version: Some(facts.content_version.clone()),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: Some(1_000),
        modified_at_ms: Some(1_000),
        deleted_at_ms: None,
    })
    .expect("item");
    tx.upsert_cache_entry(&CacheEntryRecord {
        item: item.clone(),
        account: scope().account,
        content_version: facts.content_version.clone(),
        kind: CacheKind::Blob,
        size: 64,
        blob_hash: Some(hash),
        verification: CacheVerification::Verified,
        pin: Some(PinOrigin::ArchiveMode),
        last_access_at_ms: 1_200,
        materialized_at_ms: 1_200,
        materialization_ref: Some("blobs/sha256/81".to_owned()),
    })
    .expect("cache");
    tx.pin_item(&item, PinOrigin::ArchiveMode, 1_200)
        .expect("pin");
    tx.commit().expect("commit materialization");
    apply(&mut store, &[deletion(81, 2_000)]);

    let confirmation = AuditToMirrorConfirmation::parse(
        scope().account,
        &AuditToMirrorConfirmation::expected_phrase(scope().account),
    )
    .expect("confirmation");
    let tx = store.write_txn().expect("purge");
    let change = tx
        .set_retention_mode(
            scope().account,
            RetentionMode::Mirror,
            Some(confirmation),
            3_000,
        )
        .expect("transition");
    tx.commit().expect("commit purge");
    assert_eq!(change.purged_attachments, 1);
    assert_eq!(change.purged_cache_entries, 1);
    assert_eq!(change.released_pins, 1);
    assert_eq!(change.queued_file_purges, 1);
    assert_eq!(change.invalidated_items, 1);
    drop(store);

    let mut reopened = StateStore::open(&db.path).expect("reopen after crash boundary");
    let pending = reopened
        .read_txn()
        .expect("read queue")
        .retention_purge_queue(scope().account, 10)
        .expect("queue");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].materialization_ref, "blobs/sha256/81");
    let read = reopened.read_txn().expect("read purged state");
    assert!(read.attachment(&facts.key).expect("attachment").is_none());
    assert!(read.cache_entry(&item).expect("cache").is_none());
    assert!(read.pin(&item).expect("pin").is_none());
    assert!(
        read.item(&item)
            .expect("item")
            .expect("tombstone")
            .deleted_at_ms
            .is_some()
    );
    drop(read);

    let tx = reopened.write_txn().expect("ack");
    assert!(
        tx.acknowledge_retention_purge(scope().account, "blobs/sha256/81")
            .expect("first ack")
    );
    assert!(
        !tx.acknowledge_retention_purge(scope().account, "blobs/sha256/81")
            .expect("second ack")
    );
    tx.commit().expect("commit ack");
    assert!(
        reopened
            .read_txn()
            .expect("read empty queue")
            .retention_purge_queue(scope().account, 10)
            .expect("queue")
            .is_empty()
    );
}

#[test]
fn archive_mode_is_independent_and_pins_only_allowed_persistent_content() {
    let (mut store, _) = store_with_clean_doc(RetentionMode::Mirror);
    let allowed_attachment = ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
        message: MessageKey {
            chat: chat_key(),
            message_id: MessageId(91),
        },
        index: AttachmentIndex(0),
    }))
    .id();
    let restricted_attachment = ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
        message: MessageKey {
            chat: chat_key(),
            message_id: MessageId(92),
        },
        index: AttachmentIndex(0),
    }))
    .id();
    let story = StoryKey {
        poster: chat_key(),
        story_id: StoryId(93),
    };
    let active_story = ItemKey::StoryAppearance(StoryAppearanceKey {
        story,
        view: ChatListKind::Main,
        location: StoryAppearanceLocation::Active,
    })
    .id();
    let monthly_story = ItemKey::StoryAppearance(StoryAppearanceKey {
        story,
        view: ChatListKind::Main,
        location: StoryAppearanceLocation::Month {
            year: 2026,
            month: 7,
        },
    })
    .id();
    let tx = store.write_txn().expect("write items");
    for (index, item, availability) in [
        (1, &allowed_attachment, ItemAvailability::Fetchable),
        (2, &restricted_attachment, ItemAvailability::Unavailable),
        (3, &active_story, ItemAvailability::Fetchable),
        (4, &monthly_story, ItemAvailability::Fetchable),
    ] {
        tx.upsert_item(&ItemRecord {
            aggregate_size: None,
            id: item.clone(),
            parent: Some(common::account_root_id()),
            display_name: format!("item-{index}"),
            safe_name: format!("item-{index}"),
            metadata_version: MetadataVersion::new(format!("item-meta-{index}")).expect("version"),
            content: Some(FileFacts {
                mime_type: Some("application/octet-stream".to_owned()),
                logical_size: Some(64),
                content_version: Some(
                    ContentVersion::new(format!("item-content-{index}")).expect("version"),
                ),
            }),
            availability,
            created_at_ms: None,
            modified_at_ms: None,
            deleted_at_ms: None,
        })
        .expect("item");
    }
    tx.pin_item(&restricted_attachment, PinOrigin::User, 1_500)
        .expect("explicit pin fixture");
    tx.commit().expect("commit items");

    let mut replay = account_in(RetentionMode::Mirror);
    replay.archive_mode = true;
    replay.updated_at_ms = 2_000;
    let tx = store.write_txn().expect("generic refresh");
    tx.upsert_account(&replay).expect("account refresh");
    tx.commit().expect("commit refresh");
    assert!(
        !store
            .read_txn()
            .expect("read account")
            .account(scope().account)
            .expect("account")
            .expect("exists")
            .archive_mode,
        "generic source refresh cannot bypass the Archive lifecycle"
    );

    let tx = store.write_txn().expect("enable archive");
    let enabled = tx
        .set_archive_mode(scope().account, true, 3_000)
        .expect("enable");
    tx.commit().expect("commit enable");
    assert_eq!(enabled.pinned_items, 2);
    let read = store.read_txn().expect("read pins");
    assert_eq!(
        read.pin(&allowed_attachment)
            .expect("allowed pin")
            .map(|pin| pin.origin),
        Some(PinOrigin::ArchiveMode)
    );
    assert_eq!(
        read.pin(&monthly_story)
            .expect("month pin")
            .map(|pin| pin.origin),
        Some(PinOrigin::ArchiveMode)
    );
    assert!(read.pin(&active_story).expect("active pin").is_none());
    assert_eq!(
        read.pin(&restricted_attachment)
            .expect("explicit pin")
            .map(|pin| pin.origin),
        Some(PinOrigin::User),
        "Telegram restrictions outrank Archive Mode without deleting user intent"
    );
    drop(read);

    let late_attachment = ItemKey::Canonical(CanonicalKey::Attachment(AttachmentKey {
        message: MessageKey {
            chat: chat_key(),
            message_id: MessageId(94),
        },
        index: AttachmentIndex(0),
    }))
    .id();
    let mut late_record = ItemRecord {
        aggregate_size: None,
        id: late_attachment.clone(),
        parent: Some(common::account_root_id()),
        display_name: "late.bin".to_owned(),
        safe_name: "late.bin".to_owned(),
        metadata_version: MetadataVersion::new("late-meta-v1").expect("version"),
        content: Some(FileFacts {
            mime_type: Some("application/octet-stream".to_owned()),
            logical_size: Some(64),
            content_version: Some(ContentVersion::new("late-content-v1").expect("version")),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    };
    let tx = store
        .write_txn()
        .expect("observe after enabling Archive Mode");
    tx.upsert_item(&late_record).expect("late item");
    tx.commit().expect("commit late item");
    assert_eq!(
        store
            .read_txn()
            .expect("read late pin")
            .pin(&late_attachment)
            .expect("late pin")
            .map(|pin| pin.origin),
        Some(PinOrigin::ArchiveMode),
        "new allowed live content must join Archive coverage transactionally"
    );
    late_record.availability = ItemAvailability::Unavailable;
    late_record.metadata_version = MetadataVersion::new("late-meta-v2").expect("version");
    let tx = store.write_txn().expect("apply source restriction");
    tx.upsert_item(&late_record).expect("restricted item");
    tx.commit().expect("commit restriction");
    assert!(
        store
            .read_txn()
            .expect("read released pin")
            .pin(&late_attachment)
            .expect("released pin")
            .is_none(),
        "source restrictions must release Archive ownership immediately"
    );

    let tx = store.write_txn().expect("switch retention");
    tx.set_retention_mode(scope().account, RetentionMode::Audit, None, 4_000)
        .expect("Audit");
    tx.commit().expect("commit Audit");
    let read = store.read_txn().expect("read independent policy");
    let account = read
        .account(scope().account)
        .expect("account")
        .expect("exists");
    assert_eq!(account.retention_mode, RetentionMode::Audit);
    assert!(account.archive_mode);
    assert_eq!(
        read.archive_backfill_candidates(scope().account, 10)
            .expect("worklist"),
        vec![allowed_attachment.clone(), monthly_story.clone()]
    );
    drop(read);

    let tx = store.write_txn().expect("disable archive");
    let disabled = tx
        .set_archive_mode(scope().account, false, 5_000)
        .expect("disable");
    tx.commit().expect("commit disable");
    assert_eq!(disabled.released_items, 2);
    let read = store.read_txn().expect("read disabled");
    assert!(
        read.pin(&allowed_attachment)
            .expect("allowed pin")
            .is_none()
    );
    assert!(read.pin(&monthly_story).expect("month pin").is_none());
    assert_eq!(
        read.pin(&restricted_attachment)
            .expect("explicit pin")
            .map(|pin| pin.origin),
        Some(PinOrigin::User)
    );
    assert!(
        read.archive_backfill_candidates(scope().account, 10)
            .expect("worklist")
            .is_empty()
    );
}

#[test]
fn switching_to_audit_recovers_nothing_but_invalidates_and_retains_forward() {
    let (mut store, doc) = store_with_clean_doc(RetentionMode::Mirror);
    apply(&mut store, &[observe(1, 1_000), edit(1, 1_000, 5_000, "b")]);
    // Mirror already purged the first revision.
    assert_eq!(
        message_events(&mut store, 1),
        vec![
            (MessageEventKind::Observed, None),
            (MessageEventKind::Edited, payload("payload-1-b")),
        ]
    );
    publish_clean(&mut store, &doc);
    assert!(!is_dirty(&mut store, &doc));

    let change = {
        let tx = store.write_txn().expect("write");
        let change = tx
            .set_retention_mode(scope().account, RetentionMode::Audit, None, 10_000)
            .expect("switch");
        tx.commit().expect("commit");
        change
    };

    assert!(change.changed());
    assert_eq!(change.previous, RetentionMode::Mirror);
    assert_eq!(change.current, RetentionMode::Audit);
    // Switching to Audit purges nothing and recovers nothing.
    assert_eq!(change.purged_events, 0);
    assert!(change.invalidated_docs >= 1, "the header mode changed");
    assert!(is_dirty(&mut store, &doc));

    // The already-purged first revision stays gone — no recovery (POL-3
    // scope). But Audit history accumulates from here: a further edit is now
    // retained rather than purged.
    apply(&mut store, &[edit(1, 1_000, 6_000, "c")]);
    assert_eq!(
        message_events(&mut store, 1),
        vec![
            (MessageEventKind::Observed, None),
            (MessageEventKind::Edited, payload("payload-1-b")),
            (MessageEventKind::Edited, payload("payload-1-c")),
        ]
    );
}

#[test]
fn setting_the_same_mode_is_a_noop() {
    let (mut store, doc) = store_with_clean_doc(RetentionMode::Mirror);
    apply(&mut store, &[observe(1, 1_000)]);
    publish_clean(&mut store, &doc);

    let tx = store.write_txn().expect("write");
    let change = tx
        .set_retention_mode(scope().account, RetentionMode::Mirror, None, 10_000)
        .expect("noop");
    tx.commit().expect("commit");

    assert!(!change.changed());
    assert_eq!(change.purged_events, 0);
    assert_eq!(change.invalidated_docs, 0);
    // A no-op switch does not disturb a clean document.
    assert!(!is_dirty(&mut store, &doc));
}

#[test]
fn setting_retention_for_an_unconfigured_account_is_reported() {
    let mut store = store(RetentionMode::Mirror);
    let tx = store.write_txn().expect("write");
    let missing = AccountKey {
        account_id: AccountId(999),
    };
    match tx.set_retention_mode(missing, RetentionMode::Audit, None, 1_000) {
        Err(StateError::RowNotFound { entity: "account" }) => {}
        other => panic!("expected RowNotFound(account), got {other:?}"),
    }
}
