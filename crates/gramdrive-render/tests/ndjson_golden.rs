//! Golden fixture freezing the `messages.ndjson` v1 output.
//!
//! Same policy as the model's identity and cursor goldens: the rendered bytes
//! are a durable, versioned format (DOM-006, SYNC-030). A change that alters the
//! golden is either a bug or a schema evolution — and a schema evolution is a
//! [`gramdrive_render::ndjson::SCHEMA_VERSION`] bump with a *new* golden beside
//! this one, never a silent rewrite of v1.
//!
//! Regenerate intentionally with `UPDATE_GOLDEN=1 cargo test -p gramdrive-render`
//! and review the diff.

#![allow(clippy::expect_used, clippy::panic)]

mod support;

use std::path::Path;

use gramdrive_model::identity::DocPartition;
use gramdrive_render::ndjson::{self, MessagesInput, RetentionMode};
use support::{corpus, fixture_chat, parse_lines};

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

#[test]
fn corpus_mirror_matches_golden() {
    let messages = corpus();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Mirror,
        input_watermark_seq: 13,
        messages: &messages,
    };
    let document = ndjson::render_messages(&input);
    // Independent proof it parses before it is frozen.
    let lines = parse_lines(&document);
    assert_eq!(
        lines[0].get("retention_mode").and_then(|v| v.as_str()),
        Some("mirror")
    );
    assert_golden("corpus_mirror.ndjson", &document);
}

#[test]
fn corpus_audit_matches_golden() {
    let messages = corpus();
    let input = MessagesInput {
        chat: fixture_chat(),
        partition: DocPartition::Chat,
        retention_mode: RetentionMode::Audit,
        input_watermark_seq: 13,
        messages: &messages,
    };
    let document = ndjson::render_messages(&input);
    let lines = parse_lines(&document);
    assert_eq!(
        lines[0].get("retention_mode").and_then(|v| v.as_str()),
        Some("audit")
    );
    assert_golden("corpus_audit.ndjson", &document);
}

#[test]
fn golden_files_are_stable_under_rerender() {
    // The golden files, once written, are exactly what a fresh render produces:
    // guards against a golden captured from a nondeterministic run.
    for (name, mode) in [
        ("corpus_mirror.ndjson", RetentionMode::Mirror),
        ("corpus_audit.ndjson", RetentionMode::Audit),
    ] {
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            continue;
        }
        let messages = corpus();
        let input = MessagesInput {
            chat: fixture_chat(),
            partition: DocPartition::Chat,
            retention_mode: mode,
            input_watermark_seq: 13,
            messages: &messages,
        };
        let expected = std::fs::read_to_string(golden_path(name)).expect("golden exists");
        assert_eq!(ndjson::render_messages(&input), expected);
    }
}
