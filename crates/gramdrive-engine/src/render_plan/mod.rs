//! Incremental render planning (TASK-260715-22l8zy; SYNC-024, SYNC-030..033,
//! DOM-006, DOM-023).
//!
//! Rendering a whole account on every change does not scale, and rendering
//! nothing loses edits. This module computes the middle: from the messages a
//! normalized-change batch touched and the renderer/schema versions the core
//! ships, *which* generated documents are now stale, and a plan to regenerate
//! exactly those against the current event watermark.
//!
//! # What a chat produces
//!
//! The [`catalog`] fixes the documents a chat has (the tree layout in
//! `.spec/sync-and-filesystem-semantics.md`): one lossless whole-chat
//! `messages.ndjson` and one human-readable `YYYY/MM.md` transcript per calendar
//! month. A change therefore regenerates the NDJSON (always) and the transcript
//! of each touched month (only) — the "only affected partitions regenerate"
//! criterion (SYNC-024). Months are computed with the renderer's own calendar
//! (`gramdrive_render::civil`), so the planner and the renderer never disagree
//! about which month a message near a boundary belongs to.
//!
//! # Marking and planning
//!
//! [`affected_documents`] is the pure mapping from a change batch to its stale
//! documents; [`dirty_affected`] records that on the durable dirty worklist in
//! the same write transaction as the change (SYNC-022). [`plan_for_changes`]
//! and [`plan_worklist`] turn stale documents into [`RenderJob`]s — each
//! carrying the watermark to render up to and the content version the bytes will
//! carry — skipping any document already current (idempotent re-planning).
//!
//! # Atomic, resumable publication
//!
//! The planner never writes rendered bytes. A [`RenderJob`] is executed by
//! rendering the partition up to its watermark and calling `gramdrive_state`'s
//! `publish_render`, whose in-transaction watermark re-check is what makes an
//! interrupted regeneration safe: a render that never publishes leaves the prior
//! version readable, one that raced newer events publishes but stays dirty, and
//! a partial file is never promoted into place (SYNC-033). The planner's role is
//! to name the work and converge — after a clean publish, the same document
//! yields no further job.

mod catalog;
mod plan;

pub use catalog::{DOCUMENT_CLASSES, DocClass};
pub use plan::{
    RenderJob, RenderPlan, RenderReason, affected_documents, dirty_affected, plan_for_changes,
    plan_worklist,
};

use std::fmt;

use gramdrive_model::version::InvalidVersionToken;
use gramdrive_state::StateError;

/// Why render planning failed.
///
/// Structured for the NFR-030 discipline: a category the caller can act on,
/// never a panic. State-layer failures pass through with their category intact.
#[derive(Debug)]
pub enum RenderPlanError {
    /// The state store refused or failed a read the plan depends on.
    State(StateError),
    /// A composed content-version token was not well-formed. The tokens are
    /// built from a fixed ASCII scheme and integers, so this is unreachable in
    /// practice; it exists because the core never unwraps a fallible
    /// construction (`unwrap_used`/`expect_used` are denied).
    Version(InvalidVersionToken),
}

impl From<StateError> for RenderPlanError {
    fn from(error: StateError) -> Self {
        Self::State(error)
    }
}

impl fmt::Display for RenderPlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::State(error) => write!(f, "render plan state error: {error}"),
            Self::Version(error) => write!(f, "render plan content-version error: {error}"),
        }
    }
}

impl std::error::Error for RenderPlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::State(error) => Some(error),
            Self::Version(error) => Some(error),
        }
    }
}
