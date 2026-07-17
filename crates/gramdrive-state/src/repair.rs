//! Repair markers — durable notes that a file needs a repair pass
//! (SYNC-071, NFR-034).
//!
//! A marker is a handoff between the code that *notices* work and the code
//! that *does* it. The noticing side is often the wrong place to do it: a
//! schema migration that changes the shape of a rebuildable projection
//! cannot also rebuild every row of it without turning an upgrade into an
//! unbounded job, and the runner that interrupts a resumable migration has
//! nothing to fix, only something to record. Both raise a marker; startup
//! reconciliation (TASK-260715-21clwh) and the user-triggered repair
//! entrypoint (TASK-260715-1nuhxj) are what clear them.
//!
//! Markers live in the migration journal (`schema/journal.sql`), outside the
//! numbered schema, so they survive the migrations that raise them.

use rusqlite::{Connection, params};

use crate::error::StateError;

/// What kind of repair a marker asks for.
///
/// Stored as the stable strings this enum maps to, never as an ordinal: a
/// marker outlives the build that raised it, and a reordered enum must not
/// silently rename what a file already says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairKind {
    /// A resumable migration has a durable checkpoint: it is either running
    /// right now or was interrupted by a crash (SYNC-072).
    ///
    /// The runner raises this with the first checkpoint it commits and
    /// clears it in the transaction that completes the migration, so a
    /// marker of this kind observed at open time means the previous run did
    /// not finish. It is not damage — `migration_progress` says exactly
    /// where to resume — it is the durable record that the file spent time
    /// mid-upgrade.
    MigrationInterrupted,
    /// A projection no longer matches the canonical tables and must be
    /// rebuilt from them before it is trusted (SYNC-071).
    ///
    /// `items` and render output are derived state: a migration that changes
    /// their shape raises this instead of rebuilding inline, and
    /// reconciliation does the rebuild on its own schedule.
    RebuildProjection,
}

impl RepairKind {
    /// The stable text stored in `repair_markers.kind`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::MigrationInterrupted => "migration_interrupted",
            Self::RebuildProjection => "rebuild_projection",
        }
    }

    /// Reads back what [`RepairKind::as_str`] wrote.
    ///
    /// An unrecognized kind is a marker from a newer build. It is reported
    /// ([`StateError::UnknownRepairKind`]), never skipped: silently dropping
    /// a repair request this build does not understand is exactly how a file
    /// gets used while something in it is known to be broken.
    fn parse(text: &str) -> Result<Self, StateError> {
        match text {
            "migration_interrupted" => Ok(Self::MigrationInterrupted),
            "rebuild_projection" => Ok(Self::RebuildProjection),
            other => Err(StateError::UnknownRepairKind {
                kind: other.to_owned(),
            }),
        }
    }
}

/// One open repair marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairMarker {
    /// Row identity, stable for the life of the marker.
    pub id: i64,
    /// What needs doing.
    pub kind: RepairKind,
    /// What needs it done — free text chosen by whatever raised the marker,
    /// and half of its identity: raising (kind, detail) twice leaves one
    /// marker.
    pub detail: String,
    /// When the marker was *first* raised, in milliseconds since the Unix
    /// epoch.
    pub raised_at_ms: i64,
}

/// Raises a marker, or leaves the existing one alone.
///
/// Idempotent by (kind, detail), which is what makes it safe on a migration
/// resume: re-raising keeps the first `raised_at_ms` — the moment the
/// problem started, not the moment it was noticed again.
pub(crate) fn raise(conn: &Connection, kind: RepairKind, detail: &str) -> Result<(), StateError> {
    conn.execute(
        "INSERT INTO repair_markers (kind, detail, raised_at_ms)
         VALUES (?1, ?2, unixepoch() * 1000)
         ON CONFLICT (kind, detail) DO NOTHING",
        params![kind.as_str(), detail],
    )?;
    Ok(())
}

/// Clears a marker. Clearing one that is not raised is not an error — the
/// caller wanted it gone, and it is.
pub(crate) fn clear(conn: &Connection, kind: RepairKind, detail: &str) -> Result<(), StateError> {
    conn.execute(
        "DELETE FROM repair_markers WHERE kind = ?1 AND detail = ?2",
        params![kind.as_str(), detail],
    )?;
    Ok(())
}

/// Every open marker, oldest first.
pub(crate) fn list(conn: &Connection) -> Result<Vec<RepairMarker>, StateError> {
    let mut statement = conn.prepare(
        "SELECT marker_id, kind, detail, raised_at_ms
         FROM repair_markers
         ORDER BY marker_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    let mut markers = Vec::new();
    for row in rows {
        let (id, kind, detail, raised_at_ms) = row?;
        markers.push(RepairMarker {
            id,
            kind: RepairKind::parse(&kind)?,
            detail,
            raised_at_ms,
        });
    }
    Ok(markers)
}
