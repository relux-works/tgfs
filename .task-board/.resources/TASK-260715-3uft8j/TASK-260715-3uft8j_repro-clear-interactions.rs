//! Reproduction: `FakeSource::clear_interactions()` misattributes the outcome
//! of an unrelated call when any call is still in flight across the clear.
//!
//! Run as a standalone binary with `gramdrive-testkit` as a path dependency
//! (public API only — no access to crate internals).
//!
//! Observed output:
//!   == after a SUCCESSFUL root() call, log says ==
//!     Interaction { seq: 0, call: Root, outcome: Ok }
//!   == after dropping the unrelated in-flight fetch, the SAME root() entry says ==
//!     Interaction { seq: 0, call: Root, outcome: Cancelled { delivered: 0 } }
//!   root() actually returned: Ok(Account)
//!
//! Cause: `Recorder::begin` assigns `seq = log.len()` and `Recorder::settle`
//! writes to `log[seq]`. `clear()` empties the log without invalidating the
//! `seq` held by live `CallGuard`s, so indices are reused. A guard outstanding
//! across a clear then settles whatever entry now occupies its old index.
//!
//! Two silent failure modes:
//!   1. Misattribution (below): a completed call's outcome is overwritten by an
//!      unrelated dropped future.
//!   2. Silent loss: if the log is shorter than the stale `seq`, `settle`'s
//!      `log.get_mut(seq)` returns `None` and the outcome is dropped — a
//!      cancellation test that cleared first records nothing at all.

use gramdrive_testkit::model::ByteRange;
use gramdrive_testkit::model::version::ContentVersion;
use gramdrive_testkit::source::{DirectoryKind, DriveSource, FetchRequest, FileKind};
use gramdrive_testkit::{FakeSource, RecordingSink, SourceScript, exec, fixture};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let scope = fixture::scope();
    let root = fixture::account_root_id(scope);
    let chat = fixture::chat_id(scope, 100);
    let photo = fixture::attachment_id(scope, 100, 5, 0);

    let script = SourceScript::builder(scope)
        .item(fixture::directory(root.clone(), None, "Account", "m1", DirectoryKind::Root)?)
        .item(fixture::directory(chat.clone(), Some(root), "Team", "m2", DirectoryKind::Chat)?)
        .item(fixture::file(photo.clone(), chat, "photo.jpg", "m3", "c1", 11, FileKind::Attachment)?)
        .content(&photo, ContentVersion::new("c1")?, *b"hello world")
        .build()?;

    let source = FakeSource::new(script);
    let range = ByteRange::new(0, 11)?;
    let mut sink = RecordingSink::new(range);

    // A fetch is created (recorded at seq 0) but never polled to completion.
    let pending_fetch = source.fetch(
        FetchRequest { item: photo, version: ContentVersion::new("c1")?, range },
        &mut sink,
    );

    // The documented use: "tests that set up through the source and then
    // assert only on what follows".
    source.clear_interactions();

    // A subsequent call that fully succeeds.
    let item = exec::drive(source.root())?;
    println!("== after a SUCCESSFUL root() call, log says ==");
    for interaction in source.interactions() {
        println!("  {interaction:?}");
    }

    // The still-in-flight fetch is dropped. Its guard settles seq 0, which now
    // indexes the root() entry.
    drop(pending_fetch);

    println!("\n== after dropping the unrelated in-flight fetch, the SAME root() entry says ==");
    for interaction in source.interactions() {
        println!("  {interaction:?}");
    }
    println!("\nroot() actually returned: Ok({})", item.display_name);
    Ok(())
}
