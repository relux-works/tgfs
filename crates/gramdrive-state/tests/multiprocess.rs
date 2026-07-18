//! Multi-process stress and crash tests over one shared WAL database —
//! the TASK-260715-gnsa2s acceptance criteria, with real processes.
//!
//! `tests/repo_concurrency.rs` proves the locking primitives with two
//! connections in one process; SQLite's locks are file-based, so that
//! covers the mechanism. What it cannot cover is what this suite exists
//! for: a *process* dying mid-transaction (SIGKILL leaves no chance to
//! roll back — WAL recovery on the next open must do it) and several
//! writer processes contending with no shared memory at all
//! (`.spec/architecture.md`: "assume the app and extension are separate
//! processes; never rely on shared in-memory state").
//!
//! # How children run
//!
//! Each child is this same test binary re-executed with
//! `multiprocess_child_entry --exact` and a `GRAMDRIVE_MP_ROLE`
//! environment variable. Without the variable that entry is an empty
//! passing test, so a normal suite run is unaffected. A child that fails
//! its assertions fails its libtest run, and the parent asserts on the
//! exit status — except the crash child, which the parent SIGKILLs on
//! purpose.
//!
//! # The invariant under test
//!
//! Every batch commits a message and the cursor sealing it in one
//! transaction (SYNC-022). Whatever the interleaving and however a writer
//! dies, an observer must never find a cursor ahead of its messages, a
//! lost or duplicated serialized update, or a file that fails
//! `quick_check`.

// clippy.toml exempts test code from unwrap/expect/panic lints; the
// exemption keys on `#[test]` fns, and these helpers sit at module level
// in an integration-test binary that links into no product artifact. The
// stdout lints are workspace-denied because the *core* has no console; the
// crash child's stdout is this suite's parent-child protocol, and the
// binary ships nowhere.
#![allow(
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::explicit_write
)]

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{TempDb, account_record, chat_record, revision, scope};
use gramdrive_state::model::cursor::ChangeCursor;
use gramdrive_state::model::identity::{ItemKey, MessageId};
use gramdrive_state::model::version::{ContentVersion, MetadataVersion};
use gramdrive_state::repo::{FileFacts, ItemAvailability, ItemRecord, MessageChange};
use gramdrive_state::{ProbeOutcome, StateStore, probe_database};

const CHAT: i64 = 100;
/// Message-id lane per writer: writer `w` writes ids `w*LANE + 1 ..`.
const LANE: i64 = 100_000;
/// How long the parent waits on any child observation before declaring the
/// test wedged.
const DEADLINE: Duration = Duration::from_secs(120);

const ROLE_ENV: &str = "GRAMDRIVE_MP_ROLE";
const DB_ENV: &str = "GRAMDRIVE_MP_DB";
const WRITER_ENV: &str = "GRAMDRIVE_MP_WRITER";
const BATCHES_ENV: &str = "GRAMDRIVE_MP_BATCHES";

fn stream(writer: i64) -> String {
    format!("mp-writer-{writer}")
}

fn cursor_for(batch: u64) -> ChangeCursor {
    ChangeCursor::new(scope(), batch.to_be_bytes().to_vec()).expect("cursor")
}

fn batch_of(cursor: &ChangeCursor) -> u64 {
    let payload = cursor.payload();
    let bytes: [u8; 8] = payload.try_into().expect("8-byte cursor payload");
    u64::from_be_bytes(bytes)
}

/// The item all stress writers serialize read-modify-write updates on.
fn cas_item_id() -> gramdrive_state::model::identity::ItemId {
    ItemKey::Canonical(common::attachment_key(CHAT, 1, 0)).id()
}

/// Content-version tokens count updates: `v0`, `v1`, ... Every writer does
/// read-modify-write inside one IMMEDIATE transaction; a lost update would
/// leave the final counter short.
fn version_number(token: &ContentVersion) -> u64 {
    token
        .as_str()
        .strip_prefix('v')
        .expect("counter token")
        .parse()
        .expect("numeric counter token")
}

fn seeded(path: &Path) -> StateStore {
    let mut store = StateStore::open(path).expect("open");
    let tx = store.write_txn().expect("write txn");
    tx.upsert_account(&account_record()).expect("account");
    tx.upsert_chat(&chat_record(CHAT)).expect("chat");
    tx.upsert_item(&ItemRecord {
        id: common::account_root_id(),
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
    .expect("root item");
    tx.upsert_item(&ItemRecord {
        id: cas_item_id(),
        parent: Some(common::account_root_id()),
        display_name: "counter.bin".to_owned(),
        safe_name: "counter.bin".to_owned(),
        metadata_version: MetadataVersion::new("m1").expect("version"),
        content: Some(FileFacts {
            mime_type: None,
            logical_size: Some(0),
            content_version: Some(ContentVersion::new("v0").expect("version")),
        }),
        availability: ItemAvailability::Fetchable,
        created_at_ms: None,
        modified_at_ms: None,
        deleted_at_ms: None,
    })
    .expect("cas item");
    tx.commit().expect("commit");
    store
}

fn spawn_child(role: &str, db: &Path, writer: i64, batches: u64) -> Child {
    Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "multiprocess_child_entry",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE_ENV, role)
        .env(DB_ENV, db.as_os_str())
        .env(WRITER_ENV, writer.to_string())
        .env(BATCHES_ENV, batches.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn child process")
}

fn env_i64(name: &str) -> i64 {
    std::env::var(name)
        .expect(name)
        .parse()
        .expect("numeric env var")
}

// ---------------------------------------------------------------------------
// Child entry point
// ---------------------------------------------------------------------------

/// Dispatch for re-executed children. A plain suite run (no role variable)
/// passes vacuously.
#[test]
fn multiprocess_child_entry() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let db = PathBuf::from(std::env::var(DB_ENV).expect("child db path"));
    match role.as_str() {
        "stress-writer" => stress_writer(&db, env_i64(WRITER_ENV), env_i64(BATCHES_ENV) as u64),
        "crash-writer" => crash_writer(&db, env_i64(WRITER_ENV)),
        other => panic!("unknown child role '{other}'"),
    }
}

/// Stress child: `batches` transactions, each committing one message and
/// the cursor sealing it (SYNC-022), then one serialized counter bump —
/// all through the same public API the product uses.
fn stress_writer(db: &Path, writer: i64, batches: u64) {
    let mut store = StateStore::open(db).expect("child open");
    let chat = common::chat_key(CHAT);
    let stream = stream(writer);
    for batch in 1..=batches {
        let message = writer * LANE + i64::try_from(batch).expect("fits");
        let tx = store.write_txn().expect("write txn");
        tx.apply_message_changes(
            &chat,
            &[MessageChange::Observed(revision(message, message))],
        )
        .expect("apply");
        tx.put_cursor(&stream, &cursor_for(batch), message)
            .expect("cursor");
        tx.commit().expect("commit");

        // Read-modify-write under the same IMMEDIATE lock: no update may
        // be lost however many processes contend.
        let tx = store.write_txn().expect("write txn");
        let current = tx
            .read()
            .item(&cas_item_id())
            .expect("read counter")
            .expect("counter exists")
            .content
            .expect("counter is a file")
            .content_version
            .expect("counter has a version");
        let next = version_number(&current) + 1;
        tx.update_item_content(
            &cas_item_id(),
            Some(&current),
            &FileFacts {
                mime_type: None,
                logical_size: Some(next),
                content_version: Some(ContentVersion::new(format!("v{next}")).expect("version")),
            },
            &MetadataVersion::new(format!("m{next}")).expect("version"),
            message,
        )
        .expect("bump counter");
        tx.commit().expect("commit");
    }
}

/// Crash child: resumes from its durable cursor and commits batches forever
/// — one message plus the sealing cursor per transaction — reporting each
/// commit on stdout. It never exits on its own; the parent SIGKILLs it.
fn crash_writer(db: &Path, writer: i64) {
    let mut store = StateStore::open(db).expect("child open");
    let chat = common::chat_key(CHAT);
    let stream = stream(writer);
    let start = {
        let read = store.read_txn().expect("read txn");
        read.cursor(scope(), &stream)
            .expect("cursor read")
            .map_or(0, |cursor| batch_of(&cursor))
    };
    let mut out = std::io::stdout();
    for batch in (start + 1).. {
        let message = writer * LANE + i64::try_from(batch).expect("fits");
        let tx = store.write_txn().expect("write txn");
        tx.apply_message_changes(
            &chat,
            &[MessageChange::Observed(revision(message, message))],
        )
        .expect("apply");
        tx.put_cursor(&stream, &cursor_for(batch), message)
            .expect("cursor");
        tx.commit().expect("commit");
        writeln!(out, "committed {batch}").expect("report");
        out.flush().expect("flush");
    }
}

// ---------------------------------------------------------------------------
// Parent tests
// ---------------------------------------------------------------------------

/// Three writer processes, no shared memory, one database file. While they
/// run, this process holds them to the cursor-behind-state invariant from
/// read snapshots; afterwards every batch must be present exactly once,
/// the serialized counter must equal the total number of bumps, and the
/// file must pass `quick_check`.
#[test]
fn concurrent_writer_processes_preserve_invariants_under_stress() {
    const WRITERS: i64 = 3;
    const BATCHES: u64 = 25;

    let db = TempDb::new();
    let mut observer = seeded(&db.path);
    let chat = common::chat_key(CHAT);

    let mut children: Vec<Child> = (1..=WRITERS)
        .map(|writer| spawn_child("stress-writer", &db.path, writer, BATCHES))
        .collect();

    // Observe while the writers race: within one snapshot, a cursor at
    // batch k proves at least k of that writer's messages.
    let deadline = Instant::now() + DEADLINE;
    loop {
        let read = observer.read_txn().expect("read txn");
        for writer in 1..=WRITERS {
            let sealed = read
                .cursor(scope(), &stream(writer))
                .expect("cursor")
                .map_or(0, |cursor| batch_of(&cursor));
            let present = read
                .messages_after(
                    &chat,
                    MessageId(writer * LANE),
                    u32::try_from(BATCHES).expect("fits"),
                )
                .expect("messages")
                .iter()
                .filter(|message| {
                    message.message_id.0 <= writer * LANE + i64::try_from(BATCHES).expect("fits")
                })
                .count() as u64;
            assert!(
                present >= sealed,
                "writer {writer}: cursor seals batch {sealed} but only {present} messages are visible"
            );
        }
        drop(read);

        let all_done = children
            .iter_mut()
            .all(|child| child.try_wait().expect("try_wait").is_some());
        if all_done {
            break;
        }
        assert!(Instant::now() < deadline, "stress children wedged");
        std::thread::sleep(Duration::from_millis(10));
    }

    for child in &mut children {
        let status = child.wait().expect("child exit");
        assert!(status.success(), "stress child failed: {status:?}");
    }

    // Terminal state: every batch exactly once, cursors sealed at the end,
    // no lost counter updates, healthy file.
    let read = observer.read_txn().expect("read txn");
    for writer in 1..=WRITERS {
        let sealed = read
            .cursor(scope(), &stream(writer))
            .expect("cursor")
            .map_or(0, |cursor| batch_of(&cursor));
        assert_eq!(sealed, BATCHES, "writer {writer} must seal every batch");
        let ids: Vec<i64> = read
            .messages_after(&chat, MessageId(writer * LANE), 10_000)
            .expect("messages")
            .iter()
            .map(|message| message.message_id.0)
            .filter(|id| *id <= writer * LANE + i64::try_from(BATCHES).expect("fits"))
            .collect();
        let expected: Vec<i64> = (1..=i64::try_from(BATCHES).expect("fits"))
            .map(|batch| writer * LANE + batch)
            .collect();
        assert_eq!(ids, expected, "writer {writer} lane must be gapless");
    }
    let counter = read
        .item(&cas_item_id())
        .expect("read counter")
        .expect("counter exists")
        .content
        .expect("file facts")
        .content_version
        .expect("version");
    assert_eq!(
        version_number(&counter),
        WRITERS as u64 * BATCHES,
        "every serialized counter bump must land exactly once"
    );
    drop(read);
    drop(observer);

    assert_eq!(
        probe_database(&db.path).expect("probe"),
        ProbeOutcome::Healthy
    );
}

/// A writer process is SIGKILLed mid-stream, repeatedly. SIGKILL allows no
/// rollback, no destructor, no flush — WAL recovery on the next open must
/// discard whatever the dead process had half-written. After every kill:
/// the file passes `quick_check`, and the cursor equals the exact set of
/// committed messages (nothing lost, nothing partial, SYNC-022).
#[test]
fn a_sigkilled_writer_never_corrupts_state_and_never_tears_a_batch() {
    const ROUNDS: u32 = 3;
    const WRITER: i64 = 9;

    let db = TempDb::new();
    drop(seeded(&db.path));

    let mut last_observed: u64 = 0;
    for round in 1..=ROUNDS {
        let mut child = spawn_child("crash-writer", &db.path, WRITER, 0);
        let stdout = child.stdout.take().expect("child stdout");
        let mut lines = BufReader::new(stdout).lines();

        // Wait until the child has committed a few batches past the last
        // round's survivors, then SIGKILL it wherever it happens to be —
        // most likely inside its next transaction.
        let target = last_observed + 3;
        let deadline = Instant::now() + DEADLINE;
        let mut newest = 0;
        while newest < target {
            assert!(Instant::now() < deadline, "crash child made no progress");
            let line = lines
                .next()
                .expect("child stdout closed early")
                .expect("read child stdout");
            if let Some(batch) = line.strip_prefix("committed ") {
                newest = batch.trim().parse().expect("batch number");
            }
        }
        child.kill().expect("SIGKILL child");
        let status = child.wait().expect("reap child");
        assert!(!status.success(), "round {round}: child must die by signal");

        // The dead process held the write lock arbitrarily recently; the
        // next open recovers the WAL and must find a consistent file.
        assert_eq!(
            probe_database(&db.path).expect("probe"),
            ProbeOutcome::Healthy,
            "round {round}: file must survive SIGKILL intact"
        );
        let mut store = StateStore::open(&db.path).expect("reopen after kill");
        let read = store.read_txn().expect("read txn");
        let sealed = read
            .cursor(scope(), &stream(WRITER))
            .expect("cursor")
            .map(|cursor| batch_of(&cursor))
            .expect("cursor must exist after commits");
        assert!(
            sealed >= newest,
            "round {round}: observed commit {newest} must be durable (WAL), found {sealed}"
        );
        let ids: Vec<i64> = read
            .messages_after(&common::chat_key(CHAT), MessageId(WRITER * LANE), 1_000_000)
            .expect("messages")
            .iter()
            .map(|message| message.message_id.0)
            .collect();
        let expected: Vec<i64> = (1..=i64::try_from(sealed).expect("fits"))
            .map(|batch| WRITER * LANE + batch)
            .collect();
        assert_eq!(
            ids, expected,
            "round {round}: messages must equal exactly the batches the cursor seals"
        );
        last_observed = sealed;
    }
}
