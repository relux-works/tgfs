//! Date-first monthly render planning and invalidation.

#![allow(clippy::expect_used, clippy::panic)]

use gramdrive_engine::model::identity::{
    AccountId, AccountKey, AccountScope, AppearanceKey, CanonicalKey, ChatId, ChatKey, ChatListKey,
    ChatListKind, ContentHash, DocFormat, DocPartition, GeneratedDocKey, ItemId, ItemKey,
    MessageId, MonthDirKey, NamespaceVersion, SchemaFamily,
};
use gramdrive_engine::model::version::{ContentVersion, MetadataVersion};
use gramdrive_engine::render::markdown::{DisplayTimeZone, UtcOffset};
use gramdrive_engine::render_plan::{
    DocClass, RenderReason, affected_documents, dirty_affected, plan_for_changes, plan_worklist,
};
use gramdrive_engine::state::repo::{
    AccountRecord, ChatRecord, ChatType, FileFacts, ItemAvailability, ItemRecord, MessageChange,
    MessageRevision, RenderOutput, RetentionMode, SourceKind,
};
use gramdrive_engine::state::{StateStore, WriteTxn};

const JULY_15: i64 = 1_784_116_800_000;
const AUGUST_3: i64 = 1_785_758_400_000;
const JULY_END_2330Z: i64 = 1_785_540_600_000;

fn scope() -> AccountScope {
    AccountScope {
        account: AccountKey {
            account_id: AccountId(7),
        },
        namespace_version: NamespaceVersion(1),
    }
}

fn chat() -> ChatKey {
    ChatKey {
        scope: scope(),
        chat_id: ChatId(100),
    }
}

fn utc() -> DisplayTimeZone {
    DisplayTimeZone::fixed(UtcOffset::UTC)
}

fn partition(year: u16, month: u8) -> DocPartition {
    DocPartition::Month { year, month }
}

fn key(class: DocClass, year: u16, month: u8) -> GeneratedDocKey {
    class.document_key(chat(), partition(year, month))
}

fn id(class: DocClass, year: u16, month: u8) -> ItemId {
    class.document_id(chat(), partition(year, month))
}

fn appearance_id(view: ChatListKind, class: DocClass, year: u16, month: u8) -> ItemId {
    ItemKey::Appearance(AppearanceKey {
        view,
        item: CanonicalKey::GeneratedDoc(key(class, year, month)),
    })
    .id()
}

fn metadata(text: &str) -> MetadataVersion {
    MetadataVersion::new(text).expect("metadata")
}

fn store() -> StateStore {
    let mut store = StateStore::open_in_memory().expect("open");
    let tx = store.write_txn().expect("write");
    tx.upsert_account(&AccountRecord {
        account: scope().account,
        source_kind: SourceKind::LocalTdlib,
        display_name: "Account".to_owned(),
        auth_state: "authorized".to_owned(),
        namespace_version: scope().namespace_version,
        display_timezone: "UTC".to_owned(),
        retention_mode: RetentionMode::Mirror,
        archive_mode: false,
        secret_ref: None,
        created_at_ms: 1,
        updated_at_ms: 1,
    })
    .expect("account");
    tx.upsert_chat(&ChatRecord {
        key: chat(),
        chat_type: ChatType::Private,
        title: "Chat".to_owned(),
        username: None,
        is_protected: false,
        archive_mode: false,
        metadata_version: metadata("m1"),
        left_at_ms: None,
        deleted_at_ms: None,
        last_update_at_ms: None,
    })
    .expect("chat");
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: ItemKey::Canonical(CanonicalKey::Account(scope().account)).id(),
        parent: None,
        display_name: "Account".to_owned(),
        safe_name: "Account".to_owned(),
        metadata_version: metadata("m1"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("root");
    tx.commit().expect("commit");
    store
}

fn project(tx: &WriteTxn<'_>, view: ChatListKind, class: DocClass, year: u16, month: u8) {
    let month_id = ItemKey::Appearance(AppearanceKey {
        view,
        item: CanonicalKey::MonthDir(MonthDirKey {
            chat: chat(),
            year,
            month,
        }),
    })
    .id();
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: appearance_id(view, class, year, month),
        parent: Some(month_id),
        display_name: match class {
            DocClass::ChatJson => ".chat.json",
            DocClass::MarkdownMonth => "Messages.md",
            DocClass::NdjsonMonth => "Messages.ndjson",
        }
        .to_owned(),
        safe_name: match class {
            DocClass::ChatJson => ".chat.json",
            DocClass::MarkdownMonth => "Messages.md",
            DocClass::NdjsonMonth => "Messages.ndjson",
        }
        .to_owned(),
        metadata_version: metadata("m1"),
        content: Some(FileFacts::default()),
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("doc");
}

fn project_month_in(tx: &WriteTxn<'_>, view: ChatListKind, year: u16, month: u8) {
    let root = ItemKey::Canonical(CanonicalKey::Account(scope().account)).id();
    let list = ItemKey::Canonical(CanonicalKey::ChatList(ChatListKey {
        scope: scope(),
        kind: view,
    }))
    .id();
    let chat_item = ItemKey::Appearance(AppearanceKey {
        view,
        item: CanonicalKey::Chat(chat()),
    })
    .id();
    let month_id = ItemKey::Appearance(AppearanceKey {
        view,
        item: CanonicalKey::MonthDir(MonthDirKey {
            chat: chat(),
            year,
            month,
        }),
    })
    .id();
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: list.clone(),
        parent: Some(root),
        display_name: format!("{view:?}"),
        safe_name: format!("{view:?}"),
        metadata_version: metadata("m1"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("list");
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: chat_item.clone(),
        parent: Some(list),
        display_name: "Chat".to_owned(),
        safe_name: "Chat".to_owned(),
        metadata_version: metadata("m1"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("chat appearance");
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: month_id,
        parent: Some(chat_item),
        display_name: format!("{year:04}-{month:02}"),
        safe_name: format!("{year:04}-{month:02}"),
        metadata_version: metadata("m1"),
        content: None,
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("month");
    project(tx, view, DocClass::MarkdownMonth, year, month);
    project(tx, view, DocClass::NdjsonMonth, year, month);
}

fn project_month(tx: &WriteTxn<'_>, year: u16, month: u8) {
    project_month_in(tx, ChatListKind::Main, year, month);
}

fn project_chat_json(tx: &WriteTxn<'_>) -> ItemId {
    let view = ChatListKind::Main;
    let chat_item = ItemKey::Appearance(AppearanceKey {
        view,
        item: CanonicalKey::Chat(chat()),
    })
    .id();
    let document = DocClass::ChatJson.document_key(chat(), DocPartition::Chat);
    let item = ItemKey::Appearance(AppearanceKey {
        view,
        item: CanonicalKey::GeneratedDoc(document),
    })
    .id();
    tx.upsert_item(&ItemRecord {
        aggregate_size: None,
        id: item.clone(),
        parent: Some(chat_item),
        display_name: ".chat.json".to_owned(),
        safe_name: ".chat.json".to_owned(),
        metadata_version: metadata("m1"),
        content: Some(FileFacts::default()),
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("chat JSON");
    tx.ensure_render_state(
        &item,
        DocClass::ChatJson.renderer_version(),
        DocClass::ChatJson.schema_version(),
    )
    .expect("render state");
    item
}

fn observe(message_id: i64, sent_at_ms: i64, suffix: &str) -> MessageChange {
    MessageChange::Observed(MessageRevision {
        message_id: MessageId(message_id),
        sender_id: Some(500),
        sent_at_ms,
        edited_at_ms: (!suffix.is_empty()).then_some(sent_at_ms + 1_000),
        observed_at_ms: sent_at_ms + 2_000,
        payload_schema: SchemaFamily(1),
        payload: format!("message-{message_id}{suffix}").into_bytes(),
    })
}

fn apply(store: &mut StateStore, changes: &[MessageChange]) -> i64 {
    let tx = store.write_txn().expect("write");
    tx.apply_message_changes(&chat(), changes).expect("changes");
    let watermark = tx.read().latest_event_seq(&chat()).expect("watermark");
    tx.commit().expect("commit");
    watermark
}

fn publish(store: &mut StateStore, item: &ItemId, watermark: i64) {
    let tx = store.write_txn().expect("write");
    tx.publish_render(
        item,
        &chat(),
        watermark,
        &RenderOutput {
            content_version: ContentVersion::new(format!("render-w{watermark}")).expect("version"),
            content_hash: Some(ContentHash::Sha256([1; 32])),
            logical_size: 1,
        },
        watermark,
    )
    .expect("publish");
    tx.commit().expect("commit");
}

#[test]
fn changes_map_to_both_documents_of_only_their_months() {
    assert!(affected_documents(chat(), &[], &utc()).is_empty());
    assert_eq!(
        affected_documents(chat(), &[JULY_15], &utc()),
        vec![
            key(DocClass::MarkdownMonth, 2026, 7),
            key(DocClass::NdjsonMonth, 2026, 7),
        ]
    );
    assert_eq!(
        affected_documents(chat(), &[AUGUST_3, JULY_15, JULY_15], &utc()),
        vec![
            key(DocClass::MarkdownMonth, 2026, 7),
            key(DocClass::NdjsonMonth, 2026, 7),
            key(DocClass::MarkdownMonth, 2026, 8),
            key(DocClass::NdjsonMonth, 2026, 8),
        ]
    );
}

#[test]
fn persisted_iana_timezone_controls_the_partition_boundary() {
    let tbilisi = DisplayTimeZone::named("Asia/Tbilisi").expect("IANA zone");
    assert_eq!(
        affected_documents(chat(), &[JULY_END_2330Z], &utc()),
        vec![
            key(DocClass::MarkdownMonth, 2026, 7),
            key(DocClass::NdjsonMonth, 2026, 7),
        ]
    );
    assert_eq!(
        affected_documents(chat(), &[JULY_END_2330Z], &tbilisi),
        vec![
            key(DocClass::MarkdownMonth, 2026, 8),
            key(DocClass::NdjsonMonth, 2026, 8),
        ]
    );
}

#[test]
fn insert_edit_and_delete_dirty_only_the_affected_month_pair() {
    let mut state = store();
    apply(
        &mut state,
        &[observe(1, JULY_15, ""), observe(2, AUGUST_3, "")],
    );
    let tx = state.write_txn().expect("write");
    project_month(&tx, 2026, 7);
    project_month(&tx, 2026, 8);
    dirty_affected(&tx, chat(), &[JULY_15, AUGUST_3], &utc()).expect("dirty");
    tx.commit().expect("commit");
    let initial = {
        let tx = state.read_txn().expect("read");
        tx.latest_event_seq(&chat()).expect("watermark")
    };
    for month in [7, 8] {
        for class in [DocClass::MarkdownMonth, DocClass::NdjsonMonth] {
            publish(
                &mut state,
                &appearance_id(ChatListKind::Main, class, 2026, month),
                initial,
            );
        }
    }

    let before_edit = initial;
    let after_edit = apply(&mut state, &[observe(1, JULY_15, "-edit")]);
    let tx = state.write_txn().expect("write");
    let instants = tx
        .read()
        .affected_message_instants(&chat(), before_edit, after_edit)
        .expect("affected");
    let dirty = dirty_affected(&tx, chat(), &instants, &utc()).expect("dirty");
    tx.commit().expect("commit");
    assert_eq!(
        dirty,
        vec![
            appearance_id(ChatListKind::Main, DocClass::MarkdownMonth, 2026, 7),
            appearance_id(ChatListKind::Main, DocClass::NdjsonMonth, 2026, 7),
        ]
    );

    for class in [DocClass::MarkdownMonth, DocClass::NdjsonMonth] {
        publish(
            &mut state,
            &appearance_id(ChatListKind::Main, class, 2026, 7),
            after_edit,
        );
    }
    let before_delete = after_edit;
    let after_delete = apply(
        &mut state,
        &[MessageChange::Deleted {
            message_id: MessageId(2),
            observed_at_ms: AUGUST_3 + 3_000,
        }],
    );
    let tx = state.write_txn().expect("write");
    let instants = tx
        .read()
        .affected_message_instants(&chat(), before_delete, after_delete)
        .expect("affected");
    let dirty = dirty_affected(&tx, chat(), &instants, &utc()).expect("dirty");
    tx.commit().expect("commit");
    assert_eq!(
        dirty,
        vec![
            appearance_id(ChatListKind::Main, DocClass::MarkdownMonth, 2026, 8),
            appearance_id(ChatListKind::Main, DocClass::NdjsonMonth, 2026, 8),
        ]
    );
}

#[test]
fn planning_is_deterministic_and_converges_after_publication() {
    let mut state = store();
    let watermark = apply(&mut state, &[observe(1, JULY_15, "")]);
    let tx = state.write_txn().expect("write");
    project_month(&tx, 2026, 7);
    let marked = dirty_affected(&tx, chat(), &[JULY_15], &utc()).expect("dirty");
    tx.commit().expect("commit");
    assert_eq!(
        marked,
        vec![
            appearance_id(ChatListKind::Main, DocClass::MarkdownMonth, 2026, 7),
            appearance_id(ChatListKind::Main, DocClass::NdjsonMonth, 2026, 7),
        ]
    );
    let tx = state.read_txn().expect("read");
    let first = plan_worklist(&tx, 10).expect("plan");
    let replay = plan_worklist(&tx, 10).expect("plan replay");
    drop(tx);
    assert_eq!(first, replay);
    assert_eq!(first.len(), 2);
    assert!(
        first
            .jobs
            .iter()
            .all(|job| job.reason == RenderReason::Dirty)
    );
    for job in first.jobs {
        publish(
            &mut state,
            &appearance_id(ChatListKind::Main, job.class, 2026, 7),
            watermark,
        );
    }
    let tx = state.read_txn().expect("read");
    assert!(plan_worklist(&tx, 10).expect("worklist").is_empty());
    assert!(
        plan_for_changes(&tx, chat(), &[JULY_15], &utc())
            .expect("plan")
            .is_empty()
    );
}

#[test]
fn chat_json_is_planned_from_metadata_without_following_message_watermarks() {
    let mut state = store();
    let tx = state.write_txn().expect("write");
    project_month(&tx, 2026, 7);
    let item = project_chat_json(&tx);
    tx.commit().expect("commit");

    let plan = plan_worklist(&state.read_txn().expect("read"), 10).expect("plan");
    assert_eq!(plan.jobs.len(), 1);
    let job = &plan.jobs[0];
    assert_eq!(job.class, DocClass::ChatJson);
    assert_eq!(job.partition, DocPartition::Chat);
    assert_eq!(job.target_watermark_seq, 0);
    assert_eq!(job.reason, RenderReason::Dirty);

    let tx = state.write_txn().expect("write");
    tx.publish_static_render(
        &item,
        &RenderOutput {
            content_version: job.content_version.clone(),
            content_hash: Some(ContentHash::Sha256([2; 32])),
            logical_size: 2,
        },
        5,
    )
    .expect("publish");
    tx.commit().expect("commit");
    apply(&mut state, &[observe(1, JULY_15, "")]);
    assert!(
        plan_worklist(&state.read_txn().expect("read"), 10)
            .expect("replan")
            .is_empty(),
        "message events do not invalidate metadata-only JSON"
    );
}

#[test]
fn catalog_ids_match_bounded_renderers() {
    let month = partition(2026, 7);
    assert_eq!(
        id(DocClass::MarkdownMonth, 2026, 7).as_bytes(),
        gramdrive_engine::render::markdown::document_id(chat(), month).as_bytes()
    );
    assert_eq!(
        id(DocClass::NdjsonMonth, 2026, 7).as_bytes(),
        gramdrive_engine::render::ndjson::document_id(chat(), month).as_bytes()
    );
    let legacy = GeneratedDocKey {
        chat: chat(),
        partition: DocPartition::Chat,
        format: DocFormat::Ndjson,
        schema_family: SchemaFamily(1),
    };
    assert!(DocClass::for_key(&legacy).is_none());
}
