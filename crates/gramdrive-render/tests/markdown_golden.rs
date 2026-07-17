//! Golden fixtures freezing the monthly Markdown v1 output.
//!
//! Same policy as the NDJSON, identity, and cursor goldens: the rendered bytes
//! are a durable, versioned format (DOM-006, SYNC-031). A change that alters a
//! golden is either a bug or a schema evolution — and an evolution is a
//! [`gramdrive_render::markdown::SCHEMA_VERSION`] bump with a *new* golden
//! beside this one, never a silent rewrite of v1.
//!
//! Regenerate intentionally with `UPDATE_GOLDEN=1 cargo test -p gramdrive-render`
//! and review the diff.

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::path::Path;

use gramdrive_model::identity::DocPartition;
use gramdrive_render::markdown::{self, MarkdownInput, RetentionMode, UtcOffset};
use support::{corpus, fixture_chat};

/// The corpus falls entirely inside November 2023 (its reference instant,
/// 1_700_000_000_000 ms, is 2023-11-14T22:13:20Z), so a month partition renders
/// one populated day section.
fn november_2023() -> DocPartition {
    DocPartition::Month {
        year: 2023,
        month: 11,
    }
}

fn golden_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn assert_golden(name: &str, actual: &str) {
    let path = golden_path(name);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create golden dir");
        }
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "golden {} missing ({error}); regenerate with UPDATE_GOLDEN=1",
            path.display()
        )
    });
    assert_eq!(
        actual,
        expected,
        "rendered output drifted from {}; if intentional, bump SCHEMA_VERSION and regenerate",
        path.display()
    );
}

fn input<'a>(messages: &'a [markdown::MessageHistory], mode: RetentionMode) -> MarkdownInput<'a> {
    MarkdownInput {
        chat: fixture_chat(),
        partition: november_2023(),
        retention_mode: mode,
        timezone: UtcOffset::UTC,
        input_watermark_seq: 13,
        messages,
    }
}

#[test]
fn corpus_mirror_matches_golden() {
    let messages = corpus();
    let document = markdown::render_transcript(&input(&messages, RetentionMode::Mirror));
    assert_golden("corpus_mirror.md", &document);
}

#[test]
fn corpus_audit_matches_golden() {
    let messages = corpus();
    let document = markdown::render_transcript(&input(&messages, RetentionMode::Audit));
    assert_golden("corpus_audit.md", &document);
}

#[test]
fn golden_files_are_stable_under_rerender() {
    // The golden files, once written, are exactly what a fresh render produces:
    // guards against a golden captured from a nondeterministic run.
    for (name, mode) in [
        ("corpus_mirror.md", RetentionMode::Mirror),
        ("corpus_audit.md", RetentionMode::Audit),
    ] {
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            continue;
        }
        let messages = corpus();
        let expected = std::fs::read_to_string(golden_path(name)).expect("golden exists");
        assert_eq!(
            markdown::render_transcript(&input(&messages, mode)),
            expected
        );
    }
}
