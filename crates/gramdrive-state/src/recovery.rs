//! Corruption detection and quarantine for the shared database file
//! (TASK-260715-gnsa2s; `.spec/architecture.md` multi-process rules).
//!
//! On Apple platforms several processes — app, companion agent, File
//! Provider extension — share one database file. When SQLite reports that
//! file corrupt (`SQLITE_CORRUPT` / `SQLITE_NOTADB`), no transaction
//! against it can be trusted, and the durable way forward is to move the
//! damaged file aside and start fresh: the database is a projection of the
//! source of truth (Telegram history plus local cache), so an empty
//! database re-seeds through ordinary sync and reconciliation rather than
//! losing user data.
//!
//! Two rules keep this safe with multiple processes over one file:
//!
//! - **Detection is separate from destruction.** [`probe_database`] only
//!   reads. [`quarantine_if_corrupt`] re-probes and moves files only when
//!   the probe itself finds corruption — a caller cannot quarantine a
//!   healthy database by misclassifying some other failure.
//! - **One designated recovery owner.** Exactly one process role — the
//!   engine host; `gramdrive-ffi` names it the *coordinator* — may call
//!   [`quarantine_if_corrupt`]. Two processes recovering concurrently could
//!   quarantine each other's fresh files. This crate cannot see process
//!   identity, so the rule is enforced at the FFI boundary and stated here
//!   as contract.
//!
//! Move order matters: the sidecars (`-shm`, then `-wal`) move before the
//! main file. A crash mid-quarantine then leaves the main file in place —
//! still corrupt, still probe-detectable, so the next attempt finishes the
//! job — and a fresh database can never start life next to a stale `-wal`
//! carrying frames of its quarantined predecessor.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

use crate::error::StateError;
use crate::store::BUSY_TIMEOUT;

/// What [`probe_database`] found at the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// No file exists. Nothing to recover; an open would create it.
    Missing,
    /// The file opens and `PRAGMA quick_check` reports no fault.
    ///
    /// Deliberately *not* a claim that every row decodes or that the schema
    /// version matches this build — those are open-time and read-time
    /// concerns with their own typed errors.
    Healthy,
    /// SQLite reports the file damaged. [`quarantine_if_corrupt`] is the
    /// sanctioned reaction, in the designated recovery-owner process only.
    Corrupt {
        /// What SQLite reported, for diagnostics.
        detail: String,
    },
}

/// What [`quarantine_if_corrupt`] moved aside.
#[derive(Debug)]
pub struct QuarantineReport {
    /// The directory the damaged files now live in — a fresh, uniquely
    /// named directory under `quarantine/` next to the database.
    pub quarantine_dir: PathBuf,
    /// The original paths that were moved (main file and whichever of
    /// `-wal`/`-shm` existed).
    pub moved: Vec<PathBuf>,
    /// What the corruption probe reported, for diagnostics.
    pub detail: String,
}

/// Checks whether the database file at `path` is usable at the file level.
///
/// Read-only in effect: opens without `CREATE` and runs
/// `PRAGMA quick_check`, which verifies page and index structure without
/// the full content scan of `integrity_check`. Errors other than corruption
/// (permissions, locking beyond the busy timeout) surface as errors — an
/// unreachable file is not evidence of a corrupt one.
pub fn probe_database(path: impl AsRef<Path>) -> Result<ProbeOutcome, StateError> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(ProbeOutcome::Missing);
    }
    // READ_WRITE without CREATE: quick_check needs no writes, but a WAL
    // database may need crash recovery on open, which a read-only
    // connection is not always permitted to perform.
    let conn = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE) {
        Ok(conn) => conn,
        Err(error) => return corrupt_or_error(error.into()),
    };
    conn.busy_timeout(BUSY_TIMEOUT)?;
    match quick_check(&conn) {
        Ok(complaints) => {
            if complaints.len() == 1 && complaints[0].eq_ignore_ascii_case("ok") {
                Ok(ProbeOutcome::Healthy)
            } else {
                Ok(ProbeOutcome::Corrupt {
                    detail: complaints.join("; "),
                })
            }
        }
        Err(error) => corrupt_or_error(error),
    }
}

/// Moves a corrupt database and its sidecars into a quarantine directory,
/// clearing the path for a fresh database on the next open.
///
/// Probes first and touches nothing unless the probe reports
/// [`ProbeOutcome::Corrupt`]; a healthy or missing file answers `None`.
/// **Only the designated recovery-owner process may call this** (module
/// docs); other processes report corruption and wait for the owner.
///
/// The damaged files land in `quarantine/<millis>-<pid>/` next to the
/// database, preserved rather than deleted so a corruption bug remains
/// diagnosable (NFR-032); pruning old quarantines is host policy.
pub fn quarantine_if_corrupt(
    path: impl AsRef<Path>,
) -> Result<Option<QuarantineReport>, StateError> {
    let path = path.as_ref();
    let detail = match probe_database(path)? {
        ProbeOutcome::Missing | ProbeOutcome::Healthy => return Ok(None),
        ProbeOutcome::Corrupt { detail } => detail,
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or(StateError::InvalidArgument {
            what: "database path has no parent directory to quarantine under",
        })?;
    let file_name = path.file_name().ok_or(StateError::InvalidArgument {
        what: "database path has no file name",
    })?;

    let quarantine_root = parent.join("quarantine");
    fs::create_dir_all(&quarantine_root).map_err(|source| StateError::QuarantineIo {
        step: "create quarantine root",
        source,
    })?;

    // Unique destination directory: wall-clock millis plus pid, with a
    // counter suffix if two quarantines land in the same millisecond.
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    let base = format!("{millis}-{pid}", pid = std::process::id());
    let mut quarantine_dir = quarantine_root.join(&base);
    let mut counter: u32 = 0;
    while quarantine_dir.exists() {
        counter += 1;
        quarantine_dir = quarantine_root.join(format!("{base}-{counter}"));
    }
    fs::create_dir(&quarantine_dir).map_err(|source| StateError::QuarantineIo {
        step: "create quarantine directory",
        source,
    })?;

    // Sidecars first, main file last (module docs: crash mid-quarantine
    // must leave a re-probeable main file and never a stale -wal beside a
    // fresh database).
    let mut moved = Vec::new();
    for suffix in ["-shm", "-wal", ""] {
        let mut name = OsString::from(file_name);
        name.push(suffix);
        let source_path = parent.join(&name);
        if source_path.exists() {
            fs::rename(&source_path, quarantine_dir.join(&name)).map_err(|source| {
                StateError::QuarantineIo {
                    step: "move damaged file into quarantine",
                    source,
                }
            })?;
            moved.push(source_path);
        }
    }

    Ok(Some(QuarantineReport {
        quarantine_dir,
        moved,
        detail,
    }))
}

/// Splits an error into "the file is corrupt" (a probe *answer*) and
/// everything else (a probe *failure*).
fn corrupt_or_error(error: StateError) -> Result<ProbeOutcome, StateError> {
    if error.is_database_corruption() {
        Ok(ProbeOutcome::Corrupt {
            detail: error.to_string(),
        })
    } else {
        Err(error)
    }
}

fn quick_check(conn: &Connection) -> Result<Vec<String>, StateError> {
    let mut statement = conn.prepare("PRAGMA quick_check")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut complaints = Vec::new();
    for row in rows {
        complaints.push(row?);
    }
    Ok(complaints)
}
