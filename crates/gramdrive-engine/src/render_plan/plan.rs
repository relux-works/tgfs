//! Computing which documents a change touched, and turning that into a render
//! plan against the current watermarks (SYNC-024, SYNC-030..033).
//!
//! Two halves of one protocol:
//!
//! - [`affected_documents`] / [`dirty_affected`] — the *marking* half. From the
//!   send instants of the messages a change batch touched, the Markdown/NDJSON
//!   pair of each touched calendar month are the affected documents, and only
//!   those. [`dirty_affected`] records that on the durable
//!   dirty worklist in the caller's write transaction, so the worklist advances
//!   atomically with the normalized state it reflects (SYNC-022).
//! - [`plan_for_changes`] / [`plan_worklist`] — the *planning* half. Each
//!   affected (or already-dirty) document is checked against its `render_state`
//!   and the chat's current event watermark: a document already current at the
//!   target watermark and versions is skipped, and everything else becomes a
//!   [`RenderJob`] carrying the watermark to render up to and the content
//!   version the published bytes will bear.
//!
//! The planner never renders and never publishes. The monthly pipeline reads a
//! pinned partition snapshot, composes both files, and calls the state layer's
//! month-scoped publication check inside the transaction that advances both
//! appearances. A render interrupted before publication leaves the previous
//! pair visible, and a newer event in another month does not keep this
//! partition dirty (SYNC-024, SYNC-033).

use std::collections::BTreeSet;

use gramdrive_model::identity::{
    CanonicalKey, ChatKey, DocFormat, DocPartition, GeneratedDocKey, ItemId, ItemKey,
};
use gramdrive_model::version::ContentVersion;
use gramdrive_render::markdown::DisplayTimeZone;
use gramdrive_state::StateError;
use gramdrive_state::repo::{ReadTxn, RenderCatalogEntry, RenderStateRecord, WriteTxn};

use crate::render_plan::RenderPlanError;
use crate::render_plan::catalog::DocClass;

/// Why a document is in a render plan — the incremental trigger that made it
/// stale. Checked in this order, so the most fundamental reason wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderReason {
    /// No render state yet: the document has never been published.
    New,
    /// The renderer-implementation version moved; every document of the class
    /// re-renders (SYNC-030).
    RendererUpgrade,
    /// The record-schema version moved (DOM-023).
    SchemaUpgrade,
    /// The document was on the dirty worklist — a prior change marked it, or a
    /// publish stayed dirty because newer events raced the render (SYNC-024).
    Dirty,
    /// The published bytes reflect an older watermark than the chat's current
    /// one: newer events exist than the document has seen.
    WatermarkBehind,
}

/// One unit of re-rendering: which document, from which chat and partition, up
/// to which event watermark, and the content version the published bytes will
/// carry.
///
/// A job is a *plan*, not a mutation: the renderer reads the partition's
/// records up to [`RenderJob::target_watermark_seq`], renders, and publishes at
/// [`RenderJob::content_version`]. Publication re-checks the watermark, so a job
/// that raced newer events lands but leaves the document dirty for the next plan
/// (SYNC-024, SYNC-033).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderJob {
    /// Canonical logical document identity. Provider-visible render state is
    /// stored only on the live appearance rows in the state catalog.
    pub document: ItemId,
    /// The chat whose event log the document is rendered from.
    pub chat: ChatKey,
    /// The source range the document covers.
    pub partition: DocPartition,
    /// The document's output format.
    pub format: DocFormat,
    /// The catalog class the job renders as — its versions and content-version
    /// scheme.
    pub class: DocClass,
    /// Render inputs up to and including this event sequence; the watermark the
    /// publication is checked against (SYNC-024).
    pub target_watermark_seq: i64,
    /// The content version the published bytes will carry (DOM-006), composed
    /// from the class's current versions and the target watermark.
    pub content_version: ContentVersion,
    /// Why the document is stale.
    pub reason: RenderReason,
}

/// An ordered, de-duplicated set of render jobs.
///
/// Deterministic: a change batch yields the Markdown/NDJSON pair for each month
/// in ascending calendar order; for the worklist, jobs follow the worklist's
/// own item order. Equal inputs yield an equal plan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenderPlan {
    /// The jobs to run, in execution order.
    pub jobs: Vec<RenderJob>,
}

impl RenderPlan {
    /// Whether the plan has no jobs — every candidate document was already
    /// current.
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// How many documents the plan regenerates.
    pub fn len(&self) -> usize {
        self.jobs.len()
    }
}

/// The generated documents a batch of message changes affects, given the
/// timezone the account renders in.
///
/// `touched` is the send instant (ms since the Unix epoch) of every message the
/// batch observed, edited, or deleted — the values the change applier already
/// holds. Both bounded monthly files are affected only for the civil month of
/// each touched message, computed with the renderer's own IANA timezone rules
/// (`gramdrive_render::civil`), so the planner never files a message in a month
/// the renderer would not group it under (SYNC-024, SYNC-031).
///
/// The result is de-duplicated and deterministically ordered — month ascending,
/// then Markdown before NDJSON — so equal inputs yield an equal worklist. An
/// empty `touched` yields no documents: a batch that changed nothing
/// regenerates nothing.
pub fn affected_documents(
    chat: ChatKey,
    touched: &[i64],
    timezone: &DisplayTimeZone,
) -> Vec<GeneratedDocKey> {
    if touched.is_empty() {
        return Vec::new();
    }
    let mut months: BTreeSet<(i64, u32)> = BTreeSet::new();
    for &instant in touched {
        months.insert(gramdrive_render::civil::year_month_in_timezone(
            instant,
            timezone.timezone(),
        ));
    }
    let mut docs = Vec::with_capacity(months.len() * 2);
    // Both files of each touched month, in stable Markdown/NDJSON order.
    for (year, month) in months {
        let partition = month_partition(year, month);
        docs.push(DocClass::MarkdownMonth.document_key(chat, partition));
        docs.push(DocClass::NdjsonMonth.document_key(chat, partition));
    }
    docs
}

/// Records every document a change batch affects on the durable dirty worklist,
/// creating render state for a document whose partition is newly non-empty (a
/// partition change) and marking existing ones dirty.
///
/// Call it in the same [`WriteTxn`] as the `apply_message_changes` and
/// `put_cursor` that produced the batch: the worklist then advances atomically
/// with the normalized state and cursor it reflects (SYNC-022, SYNC-024). The
/// affected documents' appearance rows must already be projected into every
/// live chat-list view. Returns every marked appearance id, grouped in the
/// deterministic logical order of [`affected_documents`].
pub fn dirty_affected(
    write: &WriteTxn<'_>,
    chat: ChatKey,
    touched: &[i64],
    timezone: &DisplayTimeZone,
) -> Result<Vec<ItemId>, StateError> {
    let mut marked = Vec::new();
    for key in affected_documents(chat, touched, timezone) {
        let Some(class) = DocClass::for_key(&key) else {
            continue;
        };
        let catalog = catalog_for_key(write.read(), key)?;
        for entry in catalog {
            // ensure_render_state creates the row (dirty by default) or bumps
            // it on a version change; mark_render_dirty then forces the dirty
            // bit for the common same-version case where only content moved.
            write.ensure_render_state(
                &entry.item,
                class.renderer_version(),
                class.schema_version(),
            )?;
            write.mark_render_dirty(&entry.item)?;
            marked.push(entry.item);
        }
    }
    Ok(marked)
}

/// The render plan for a change batch: the affected documents that are not
/// already current, as jobs against the chat's current watermark.
///
/// This is the direct, worklist-independent path — it recomputes the affected
/// set from `touched` and checks each against `render_state`, so a document a
/// prior render already brought current at the target watermark and versions
/// produces no job (only affected partitions regenerate, and only when actually
/// stale). Reads only; the caller executes and publishes the jobs.
pub fn plan_for_changes(
    read: &ReadTxn<'_>,
    chat: ChatKey,
    touched: &[i64],
    timezone: &DisplayTimeZone,
) -> Result<RenderPlan, RenderPlanError> {
    let mut jobs = Vec::new();
    for key in affected_documents(chat, touched, timezone) {
        if let Some(job) = evaluate(read, key)? {
            jobs.push(job);
        }
    }
    Ok(RenderPlan { jobs })
}

/// The render plan drained from the durable dirty worklist, up to `limit`
/// documents (SYNC-024).
///
/// The dirty bit is the crash-durable record of outstanding render work: at
/// startup or on a periodic sweep, this turns each dirty generated document into
/// a job against its chat's current watermark. Documents whose format this
/// planner does not render (`.chat.json`) are left for their own
/// planner. Reads only.
pub fn plan_worklist(read: &ReadTxn<'_>, limit: u32) -> Result<RenderPlan, RenderPlanError> {
    let mut jobs = Vec::new();
    let mut seen = BTreeSet::new();
    for item in read.dirty_render_items(limit)? {
        let ItemKey::Appearance(appearance) = item.key() else {
            // render_state should only key generated docs; anything else is not
            // this planner's to render.
            continue;
        };
        let CanonicalKey::GeneratedDoc(key) = appearance.item else {
            continue;
        };
        let logical_id = document_id(key);
        if !seen.insert(logical_id.as_bytes().to_vec()) {
            continue;
        }
        if let Some(job) = evaluate(read, key)? {
            jobs.push(job);
        }
    }
    Ok(RenderPlan { jobs })
}

/// Builds a job for one generated document if it is stale, or `None` if it is
/// already current. Shared by both planning entry points so they agree on
/// staleness and on the job they emit.
fn evaluate(
    read: &ReadTxn<'_>,
    key: GeneratedDocKey,
) -> Result<Option<RenderJob>, RenderPlanError> {
    let Some(class) = DocClass::for_key(&key) else {
        return Ok(None);
    };
    let chat = key.chat;
    let partition = key.partition;
    let document = document_id(key);
    let target = if class == DocClass::ChatJson {
        0
    } else {
        read.latest_event_seq(&chat)?
    };
    let catalog = catalog_for_key(read, key)?;
    let mut reason = None;
    for entry in &catalog {
        let state = read.render_state(&entry.item)?;
        if let Some(candidate) = staleness(class, target, state.as_ref()) {
            reason = strongest_reason(reason, candidate);
        }
    }
    let Some(reason) = reason else {
        return Ok(None);
    };
    let account = read
        .account(chat.scope.account)?
        .ok_or(StateError::RowNotFound { entity: "account" })?;
    let chat_record = read
        .chat(&chat)?
        .ok_or(StateError::RowNotFound { entity: "chat" })?;
    let render_generation = read
        .render_generation(chat.scope.account)?
        .ok_or(StateError::RowNotFound { entity: "account" })?;
    let content_version = ContentVersion::new(
        class
            .content_version_token(
                target,
                render_generation,
                account.retention_mode,
                &account.display_timezone,
                Some(&chat_record),
            )
            .ok_or(StateError::InvalidArgument {
                what: "render class has no content-version scheme",
            })?,
    )
    .map_err(RenderPlanError::Version)?;
    Ok(Some(RenderJob {
        document,
        chat,
        partition,
        format: class.format(),
        class,
        target_watermark_seq: target,
        content_version,
        reason,
    }))
}

fn catalog_for_key(
    read: &ReadTxn<'_>,
    key: GeneratedDocKey,
) -> Result<Vec<RenderCatalogEntry>, StateError> {
    let catalog = match key.partition {
        DocPartition::Chat => read.chat_render_catalog(key.chat)?,
        DocPartition::Month { year, month } => read.month_render_catalog(key.chat, year, month)?,
        DocPartition::Year { .. } => Vec::new(),
    };
    Ok(catalog
        .into_iter()
        .filter(|entry| entry.format == key.format && entry.schema_family == key.schema_family)
        .collect())
}

fn strongest_reason(
    current: Option<RenderReason>,
    candidate: RenderReason,
) -> Option<RenderReason> {
    fn rank(reason: RenderReason) -> u8 {
        match reason {
            RenderReason::New => 0,
            RenderReason::RendererUpgrade => 1,
            RenderReason::SchemaUpgrade => 2,
            RenderReason::Dirty => 3,
            RenderReason::WatermarkBehind => 4,
        }
    }
    Some(match current {
        Some(current) if rank(current) <= rank(candidate) => current,
        _ => candidate,
    })
}

/// The staleness verdict for a document at `target` watermark, given its current
/// render state (`None` when never rendered). The checks are ordered most- to
/// least-fundamental, so an upgrade reason is reported even when the document is
/// also behind on the watermark.
fn staleness(
    class: DocClass,
    target: i64,
    state: Option<&RenderStateRecord>,
) -> Option<RenderReason> {
    let Some(state) = state else {
        return Some(RenderReason::New);
    };
    if state.renderer_version != class.renderer_version() {
        Some(RenderReason::RendererUpgrade)
    } else if state.schema_version != class.schema_version() {
        Some(RenderReason::SchemaUpgrade)
    } else if state.dirty {
        Some(RenderReason::Dirty)
    } else if class != DocClass::ChatJson && state.input_watermark_seq < target {
        Some(RenderReason::WatermarkBehind)
    } else {
        None
    }
}

/// The opaque item id of a generated-document key.
fn document_id(key: GeneratedDocKey) -> ItemId {
    ItemKey::Canonical(CanonicalKey::GeneratedDoc(key)).id()
}

/// The month partition for a civil `(year, month)`, narrowing the full
/// proleptic year to the partition type's `u16` range (a corrupt far-future or
/// pre-year-0 timestamp is clamped rather than silently wrapped; real Telegram
/// timestamps are well inside it). Month is 1–12 from the civil calendar.
fn month_partition(year: i64, month: u32) -> DocPartition {
    let year = year.clamp(0, i64::from(u16::MAX));
    let year = u16::try_from(year).unwrap_or(u16::MAX);
    // civil year_month yields a month in 1..=12, which always fits u8.
    let month = u8::try_from(month).unwrap_or(u8::MAX);
    DocPartition::Month { year, month }
}
