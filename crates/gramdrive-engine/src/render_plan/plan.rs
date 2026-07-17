//! Computing which documents a change touched, and turning that into a render
//! plan against the current watermarks (SYNC-024, SYNC-030..033).
//!
//! Two halves of one protocol:
//!
//! - [`affected_documents`] / [`dirty_affected`] — the *marking* half. From the
//!   send instants of the messages a change batch touched, the whole-chat
//!   NDJSON plus the transcript of each touched calendar month are the affected
//!   documents, and only those. [`dirty_affected`] records that on the durable
//!   dirty worklist in the caller's write transaction, so the worklist advances
//!   atomically with the normalized state it reflects (SYNC-022).
//! - [`plan_for_changes`] / [`plan_worklist`] — the *planning* half. Each
//!   affected (or already-dirty) document is checked against its `render_state`
//!   and the chat's current event watermark: a document already current at the
//!   target watermark and versions is skipped, and everything else becomes a
//!   [`RenderJob`] carrying the watermark to render up to and the content
//!   version the published bytes will bear.
//!
//! The planner never renders and never publishes. A job is executed by reading
//! the partition's records up to `target_watermark_seq`, rendering, and calling
//! `gramdrive_state`'s `publish_render`, which re-checks the watermark inside
//! the publishing transaction: a render interrupted before it publishes leaves
//! the previous version in place, and one that raced newer events publishes but
//! stays dirty, so a partially regenerated document is never the visible file
//! (SYNC-024, SYNC-033). The planner is what decides *which* documents that
//! machinery runs for, and it converges — re-planning after a clean publish
//! yields no job for that document.

use std::collections::BTreeSet;

use gramdrive_model::identity::{
    CanonicalKey, ChatKey, DocFormat, DocPartition, GeneratedDocKey, ItemId, ItemKey,
};
use gramdrive_model::version::ContentVersion;
use gramdrive_render::markdown::UtcOffset;
use gramdrive_state::StateError;
use gramdrive_state::repo::{ReadTxn, RenderStateRecord, WriteTxn};

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
    /// The generated-document item to regenerate.
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
/// Deterministic: for a change batch the whole-chat NDJSON comes first, then one
/// job per affected month in ascending calendar order; for the worklist, jobs
/// follow the worklist's own item order. Equal inputs yield an equal plan.
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
/// holds. The whole-chat NDJSON is affected by any change; a monthly transcript
/// is affected only for the civil month of each touched message, computed with
/// the renderer's own calendar (`gramdrive_render::civil`), so the planner never
/// files a message in a month the renderer would not group it under (SYNC-024,
/// SYNC-031).
///
/// The result is de-duplicated and deterministically ordered — NDJSON first,
/// then months ascending — so equal inputs yield an equal worklist. An empty
/// `touched` yields no documents: a batch that changed nothing regenerates
/// nothing.
pub fn affected_documents(
    chat: ChatKey,
    touched: &[i64],
    timezone: UtcOffset,
) -> Vec<GeneratedDocKey> {
    if touched.is_empty() {
        return Vec::new();
    }
    let mut months: BTreeSet<(i64, u32)> = BTreeSet::new();
    for &instant in touched {
        months.insert(gramdrive_render::civil::year_month(
            instant,
            timezone.seconds(),
        ));
    }
    let mut docs = Vec::with_capacity(months.len() + 1);
    // Whole-chat NDJSON: any change regenerates the single lossless file.
    docs.push(DocClass::Ndjson.document_key(chat, DocPartition::Chat));
    // Only the transcript of each touched month, in ascending order.
    for (year, month) in months {
        docs.push(DocClass::MarkdownMonth.document_key(chat, month_partition(year, month)));
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
/// affected documents' item rows must already be projected into the tree; a
/// document with no item row fails loudly on its foreign key rather than
/// marking phantom work. Returns the marked document ids, in the deterministic
/// order of [`affected_documents`].
pub fn dirty_affected(
    write: &WriteTxn<'_>,
    chat: ChatKey,
    touched: &[i64],
    timezone: UtcOffset,
) -> Result<Vec<ItemId>, StateError> {
    let mut marked = Vec::new();
    for key in affected_documents(chat, touched, timezone) {
        let Some(class) = DocClass::for_key(&key) else {
            continue;
        };
        let document = document_id(key);
        // ensure_render_state creates the row (dirty by default) or bumps it on
        // a version change; mark_render_dirty then forces the dirty bit for the
        // common same-version case where only the content moved.
        write.ensure_render_state(&document, class.renderer_version(), class.schema_version())?;
        write.mark_render_dirty(&document)?;
        marked.push(document);
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
    timezone: UtcOffset,
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
/// planner does not render (a future `chat.json`) are left for their own
/// planner. Reads only.
pub fn plan_worklist(read: &ReadTxn<'_>, limit: u32) -> Result<RenderPlan, RenderPlanError> {
    let mut jobs = Vec::new();
    for item in read.dirty_render_items(limit)? {
        let ItemKey::Canonical(CanonicalKey::GeneratedDoc(key)) = item.key() else {
            // render_state should only key generated docs; anything else is not
            // this planner's to render.
            continue;
        };
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
    let target = read.latest_event_seq(&chat)?;
    let state = read.render_state(&document)?;
    let Some(reason) = staleness(class, target, state.as_ref()) else {
        return Ok(None);
    };
    let content_version = ContentVersion::new(class.content_version_token(target))
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
    } else if state.input_watermark_seq < target {
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
