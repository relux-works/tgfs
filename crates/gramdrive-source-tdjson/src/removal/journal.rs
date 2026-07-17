//! The durable removal marker: the crash-resume anchor and the concurrency
//! guard's backing state.
//!
//! One JSON file per in-progress removal, under a `.gramdrive-removal`
//! directory at the [`StorageLayout`](crate::config::StorageLayout) root —
//! deliberately *outside* the per-account `account-<id>/` subtree, so
//! [`RemovalStep::WipeDatabase`](super::RemovalStep::WipeDatabase) cannot
//! delete the record of its own progress. The file records the removal's
//! request and the set of completed stages; it is written atomically
//! (temp file, `fsync`, rename) so a crash never leaves a torn or partial
//! journal, and removed only when the removal finishes, after which the
//! account leaves no trace.
//!
//! The format is a small hand-built JSON object rather than a `serde`-derived
//! type: it keeps the crate's dependency surface exactly as it is (`serde_json`
//! only, no `serde` derive), matching the base64 helper next door, and the
//! schema is small enough that an explicit parser is clearer than a derive and
//! fails closed on every malformed shape.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gramdrive_model::identity::AccountId;
use serde_json::{Value, json};

use super::{ExportPolicy, RemovalError, RemovalMode, RemovalRequest, RemovalStep};

/// The on-disk schema version. Bumped only on an incompatible change; a
/// journal of any other version fails closed rather than being misread.
const JOURNAL_VERSION: i64 = 1;

/// The directory, at the storage root, that holds in-progress removal
/// journals. Dot-prefixed so it never collides with an `account-<id>` subtree.
const REMOVAL_DIR: &str = ".gramdrive-removal";

/// A persisted removal: its request and the stages completed so far.
pub(super) struct RemovalRecord {
    /// The removal's immutable parameters.
    pub request: RemovalRequest,
    /// The stages recorded complete, in completion order.
    pub completed: Vec<RemovalStep>,
}

/// The `.gramdrive-removal` directory under `root`.
fn removal_dir(root: &Path) -> PathBuf {
    root.join(REMOVAL_DIR)
}

/// The journal path for `account`. Injective over [`AccountId`] (an `i64`); a
/// negative id renders as `account--<n>.json`, a valid, distinct filename.
fn journal_path(root: &Path, account: AccountId) -> PathBuf {
    removal_dir(root).join(format!("account-{}.json", account.0))
}

/// Whether a removal journal exists for `account`.
pub(super) fn exists(root: &Path, account: AccountId) -> Result<bool, RemovalError> {
    journal_path(root, account)
        .try_exists()
        .map_err(|err| journal_error("probe", &err))
}

/// Read `account`'s journal, or `None` when there is none.
pub(super) fn read(root: &Path, account: AccountId) -> Result<Option<RemovalRecord>, RemovalError> {
    let path = journal_path(root, account);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(parse_record(&bytes)?)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(journal_error("read", &err)),
    }
}

/// Every journal under `root`, unordered. A missing removal directory is an
/// empty list, not an error.
pub(super) fn list(root: &Path) -> Result<Vec<RemovalRecord>, RemovalError> {
    let dir = removal_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(journal_error("list", &err)),
    };
    let mut records = Vec::new();
    for entry in entries {
        let path = entry.map_err(|err| journal_error("list", &err))?.path();
        // Only committed journals; skip the transient temp files a
        // concurrent write may have in flight.
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let bytes = fs::read(&path).map_err(|err| journal_error("read", &err))?;
        records.push(parse_record(&bytes)?);
    }
    Ok(records)
}

/// Write `record` atomically: a fresh temp file, flushed and `fsync`ed, then
/// renamed over the journal path, so a reader (or a crash) ever sees only the
/// old file or the whole new one.
pub(super) fn write(root: &Path, record: &RemovalRecord) -> Result<(), RemovalError> {
    let dir = removal_dir(root);
    fs::create_dir_all(&dir).map_err(|err| journal_error("create-dir", &err))?;
    let path = journal_path(root, record.request.account);
    let bytes = serialize_record(record)?;

    let tmp = temp_path(&dir, record.request.account);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|err| journal_error("open-temp", &err))?;
    if let Err(err) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&tmp);
        return Err(journal_error("write-temp", &err));
    }
    drop(file);
    if let Err(err) = fs::rename(&tmp, &path) {
        let _ = fs::remove_file(&tmp);
        return Err(journal_error("commit", &err));
    }
    // Best-effort directory fsync so the rename itself is durable. Ignored on
    // platforms or filesystems that refuse to open a directory as a file: the
    // file's own fsync plus the atomic rename already carry the journal's
    // integrity, and v1 runs on APFS where the directory sync succeeds.
    if let Ok(handle) = File::open(&dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// Remove `account`'s journal, and the removal directory when it empties.
/// Idempotent — an absent journal is success, so a re-run of a finished
/// removal converges.
pub(super) fn remove(root: &Path, account: AccountId) -> Result<(), RemovalError> {
    let path = journal_path(root, account);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(journal_error("remove", &err)),
    }
    // Leave no trace: drop the removal directory once the last journal is
    // gone. `remove_dir` fails when it is non-empty (another removal is still
    // in flight), which is exactly the case where it must stay — ignored.
    let _ = fs::remove_dir(removal_dir(root));
    Ok(())
}

/// A unique temp path in `dir` for one write. The process id plus a
/// monotonic counter make it unique across threads and processes without a
/// clock or randomness (neither of which this core links), so `create_new`
/// never spuriously collides even under a concurrent write of the same
/// account.
fn temp_path(dir: &Path, account: AccountId) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(
        ".account-{}.{}.{sequence}.tmp",
        account.0,
        std::process::id()
    ))
}

/// Serialize a record to the on-disk JSON bytes.
fn serialize_record(record: &RemovalRecord) -> Result<Vec<u8>, RemovalError> {
    let value = json!({
        "version": JOURNAL_VERSION,
        "account": record.request.account.0,
        "mode": record.request.mode.as_str(),
        "exports": record.request.exports.as_str(),
        "export_dirs": record
            .request
            .export_dirs
            .iter()
            .map(|dir| dir.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        "completed": record
            .completed
            .iter()
            .map(|step| step.as_str())
            .collect::<Vec<_>>(),
    });
    serde_json::to_vec(&value).map_err(|err| RemovalError::Journal {
        detail: format!("serialize failed: {err}"),
    })
}

/// Parse on-disk JSON bytes back into a record, failing closed on any
/// malformed shape rather than guessing.
fn parse_record(bytes: &[u8]) -> Result<RemovalRecord, RemovalError> {
    let value: Value = serde_json::from_slice(bytes).map_err(|err| RemovalError::Journal {
        detail: format!("unparseable journal: {err}"),
    })?;

    let version = value.get("version").and_then(Value::as_i64);
    if version != Some(JOURNAL_VERSION) {
        return Err(RemovalError::Journal {
            detail: format!("unsupported journal version: {version:?}"),
        });
    }

    let account = AccountId(field_i64(&value, "account")?);
    let mode =
        RemovalMode::parse(field_str(&value, "mode")?).ok_or_else(|| RemovalError::Journal {
            detail: "unknown removal mode".to_owned(),
        })?;
    let exports = ExportPolicy::parse(field_str(&value, "exports")?).ok_or_else(|| {
        RemovalError::Journal {
            detail: "unknown export policy".to_owned(),
        }
    })?;

    let export_dirs = string_array(&value, "export_dirs")?
        .into_iter()
        .map(PathBuf::from)
        .collect();

    let mut completed = Vec::new();
    for token in string_array(&value, "completed")? {
        let step = RemovalStep::parse(&token).ok_or_else(|| RemovalError::Journal {
            detail: format!("unknown completed stage: {token}"),
        })?;
        completed.push(step);
    }

    Ok(RemovalRecord {
        request: RemovalRequest {
            account,
            mode,
            exports,
            export_dirs,
        },
        completed,
    })
}

/// Read a required integer member.
fn field_i64(value: &Value, key: &'static str) -> Result<i64, RemovalError> {
    value
        .get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| RemovalError::Journal {
            detail: format!("missing or non-integer field '{key}'"),
        })
}

/// Read a required string member.
fn field_str<'a>(value: &'a Value, key: &'static str) -> Result<&'a str, RemovalError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RemovalError::Journal {
            detail: format!("missing or non-string field '{key}'"),
        })
}

/// Read a required array-of-strings member into owned strings.
fn string_array(value: &Value, key: &'static str) -> Result<Vec<String>, RemovalError> {
    let array = value
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| RemovalError::Journal {
            detail: format!("missing or non-array field '{key}'"),
        })?;
    let mut out = Vec::with_capacity(array.len());
    for element in array {
        let text = element.as_str().ok_or_else(|| RemovalError::Journal {
            detail: format!("non-string element in field '{key}'"),
        })?;
        out.push(text.to_owned());
    }
    Ok(out)
}

/// Build a [`RemovalError::Journal`] from a filesystem error at `stage`.
fn journal_error(stage: &str, err: &std::io::Error) -> RemovalError {
    RemovalError::Journal {
        detail: format!("{stage}: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// A unique-per-call temp directory, process id plus counter so parallel
    /// test binaries cannot collide — the crate's established fixture pattern.
    fn temp_root() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gramdrive-removal-journal-test-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        path
    }

    fn record(account: AccountId) -> RemovalRecord {
        RemovalRecord {
            request: RemovalRequest {
                account,
                mode: RemovalMode::RevokeSession,
                exports: ExportPolicy::Discard,
                export_dirs: vec![PathBuf::from("/exports/account-7")],
            },
            completed: vec![RemovalStep::SignalQuiesce, RemovalStep::TerminateSession],
        }
    }

    #[test]
    fn write_read_round_trips_the_record() {
        let root = temp_root();
        let account = AccountId(7);
        assert!(read(&root, account).unwrap().is_none());

        write(&root, &record(account)).unwrap();
        let back = read(&root, account).unwrap().expect("journal present");
        assert_eq!(back.request.account, account);
        assert_eq!(back.request.mode, RemovalMode::RevokeSession);
        assert_eq!(back.request.exports, ExportPolicy::Discard);
        assert_eq!(
            back.request.export_dirs,
            vec![PathBuf::from("/exports/account-7")]
        );
        assert_eq!(
            back.completed,
            vec![RemovalStep::SignalQuiesce, RemovalStep::TerminateSession]
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn write_leaves_no_temp_file_behind() {
        let root = temp_root();
        write(&root, &record(AccountId(7))).unwrap();
        let leftovers: Vec<_> = fs::read_dir(removal_dir(&root))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|e| e.to_str()) != Some("json"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn remove_is_idempotent_and_clears_the_empty_dir() {
        let root = temp_root();
        let account = AccountId(7);
        write(&root, &record(account)).unwrap();
        assert!(exists(&root, account).unwrap());

        remove(&root, account).unwrap();
        assert!(!exists(&root, account).unwrap());
        // The now-empty removal directory is gone: no trace left.
        assert!(!removal_dir(&root).exists());
        // Removing again converges.
        remove(&root, account).unwrap();

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_returns_every_journal_and_skips_a_missing_dir() {
        let root = temp_root();
        assert!(list(&root).unwrap().is_empty());

        write(&root, &record(AccountId(7))).unwrap();
        write(&root, &record(AccountId(8))).unwrap();
        let mut accounts: Vec<i64> = list(&root)
            .unwrap()
            .into_iter()
            .map(|record| record.request.account.0)
            .collect();
        accounts.sort_unstable();
        assert_eq!(accounts, vec![7, 8]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn malformed_journals_fail_closed() {
        let root = temp_root();
        let dir = removal_dir(&root);
        fs::create_dir_all(&dir).unwrap();

        // Not JSON at all.
        fs::write(journal_path(&root, AccountId(1)), b"not json").unwrap();
        assert!(matches!(
            read(&root, AccountId(1)),
            Err(RemovalError::Journal { .. })
        ));

        // Wrong version.
        fs::write(
            journal_path(&root, AccountId(2)),
            br#"{"version":99,"account":2,"mode":"local_only","exports":"discard","export_dirs":[],"completed":[]}"#,
        )
        .unwrap();
        assert!(matches!(
            read(&root, AccountId(2)),
            Err(RemovalError::Journal { .. })
        ));

        // Unknown stage token.
        fs::write(
            journal_path(&root, AccountId(3)),
            br#"{"version":1,"account":3,"mode":"local_only","exports":"discard","export_dirs":[],"completed":["nope"]}"#,
        )
        .unwrap();
        assert!(matches!(
            read(&root, AccountId(3)),
            Err(RemovalError::Journal { .. })
        ));

        let _ = fs::remove_dir_all(&root);
    }
}
