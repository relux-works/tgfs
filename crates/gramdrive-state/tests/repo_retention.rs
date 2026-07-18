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
    AccountId, AccountKey, AppearanceKey, ChatKey, ChatListKind, ContentHash, DocFormat,
    DocPartition, ItemId, ItemKey, MessageId, MessageKey,
};
use gramdrive_state::model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::repo::{
    AccountRecord, FileFacts, ItemAvailability, ItemRecord, MessageChange, MessageEventKind,
    MessageState, RenderOutput, RetentionMode,
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

fn apply(store: &mut StateStore, changes: &[MessageChange]) {
    let tx = store.write_txn().expect("write");
    tx.apply_message_changes(&chat_key(), changes)
        .expect("apply");
    tx.commit().expect("commit");
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
            .set_retention_mode(scope().account, RetentionMode::Mirror, 10_000)
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
            .set_retention_mode(scope().account, RetentionMode::Audit, 10_000)
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
        .set_retention_mode(scope().account, RetentionMode::Mirror, 10_000)
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
    match tx.set_retention_mode(missing, RetentionMode::Audit, 1_000) {
        Err(StateError::RowNotFound { entity: "account" }) => {}
        other => panic!("expected RowNotFound(account), got {other:?}"),
    }
}
