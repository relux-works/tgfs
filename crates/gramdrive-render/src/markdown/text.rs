//! Deterministic, dependency-free text helpers for the Markdown renderer:
//! injection-safe escaping and media-link percent-encoding.
//!
//! Every function here is a pure transform of its input — no locale, no clock,
//! no allocator-order dependence — so equal inputs yield byte-identical output
//! (SYNC-031). The escaper is hand-rolled for the same reason the JSON writer
//! is (`crate::json`): it is one small, fully specified rule, and a dependency
//! would be more supply-chain surface (POL-6) than code. Civil-time conversion
//! moved to the shared [`crate::civil`] module, which the render planner reuses
//! so it never disagrees with the day grouping about a calendar boundary.

/// The Unicode replacement character, substituted for raw C0 controls that a
/// Markdown reader would otherwise swallow or mis-render.
const REPLACEMENT: char = '\u{fffd}';

/// Appends `text` to `out` with every Markdown- and HTML-significant character
/// neutralized, so untrusted message text can never change the document's
/// structure (the task's injection-safety criterion, SYNC-031).
///
/// The rule is total and position-independent, which is what makes it safe to
/// audit:
/// - `&`, `<`, `>` become HTML entities. A Markdown parser resolves block
///   structure over the raw bytes *before* entity substitution, so `&lt;`
///   starts no autolink or raw-HTML tag and `&gt;` starts no blockquote.
/// - every other CommonMark/GFM-active ASCII punctuation character is
///   backslash-escaped, which CommonMark defines as rendering the literal
///   character: this defuses headings (`#`), lists (`-` `+` `.`), thematic
///   breaks and setext underlines (`-` `=`), fenced code (`` ` `` `~`),
///   emphasis (`*` `_`), links/images (`[` `]` `(` `)` `!`), tables (`|`),
///   braces, and the escape character itself.
/// - a C0 control other than tab is replaced with U+FFFD. Newlines are handled
///   by the caller (which splits into lines first) and never reach here.
///
/// Characters left untouched — the remaining ASCII punctuation (`" $ % ' , : ;
/// ? @ / ^`) and every non-ASCII scalar — are inert in CommonMark and GFM, so
/// escaping them would only add noise. Emoji, RTL text, and combining marks
/// pass through unchanged.
pub(super) fn escape_inline(text: &str, out: &mut String) {
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\\' | '`' | '*' | '_' | '{' | '}' | '[' | ']' | '(' | ')' | '#' | '+' | '-' | '.'
            | '!' | '|' | '~' | '=' => {
                out.push('\\');
                out.push(character);
            }
            // Tab is layout, not a control that breaks a reader; keep it.
            '\t' => out.push('\t'),
            // Other C0 controls and DEL have no place in a rendered line.
            control if (control as u32) < 0x20 || control as u32 == 0x7f => {
                out.push(REPLACEMENT);
            }
            other => out.push(other),
        }
    }
}

/// Escapes `text` and joins its lines with CommonMark hard breaks, yielding one
/// paragraph whose visual line breaks match the source.
///
/// Line endings are normalized (`\r\n` and lone `\r` become `\n`) and each line
/// is [`escape_inline`]d, then joined with a trailing backslash + newline — the
/// CommonMark hard-line-break. Because no genuinely blank line survives (a
/// source blank line becomes a lone `\`), the whole body stays a single
/// paragraph: an indented line cannot open an indented code block (indented
/// code cannot interrupt a paragraph) and every block-starting character is
/// already escaped. Multi-line untrusted text is therefore structurally inert.
pub(super) fn escape_paragraph(text: &str) -> String {
    let normalized = normalize_newlines(text);
    let mut out = String::with_capacity(normalized.len());
    let mut first = true;
    for line in normalized.split('\n') {
        if !first {
            // Hard break: a backslash before the newline. The previous line's
            // content is already written; this closes it as its own visual row.
            out.push_str("\\\n");
        }
        first = false;
        escape_inline(line, &mut out);
    }
    out
}

/// Escapes `text` and flattens it to a single line, replacing every internal
/// line break with a space. Used where a value must occupy one physical line —
/// a list item or an inline annotation — and cannot carry hard breaks.
pub(super) fn escape_flattened(text: &str) -> String {
    let normalized = normalize_newlines(text);
    let mut collapsed = String::with_capacity(normalized.len());
    let mut first = true;
    for line in normalized.split('\n') {
        if !first {
            collapsed.push(' ');
        }
        first = false;
        collapsed.push_str(line);
    }
    let mut out = String::with_capacity(collapsed.len());
    escape_inline(&collapsed, &mut out);
    out
}

/// Normalizes `\r\n` and lone `\r` to `\n`. Cheap to skip when there is no `\r`.
fn normalize_newlines(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('\r') {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(character);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Appends the percent-encoded form of a single path component to `out`, for
/// use as the destination of a Markdown link to a sibling attachment.
///
/// RFC 3986 unreserved characters (`A-Za-z0-9-._~`) pass through; every other
/// byte — including spaces, parentheses, `/`, and all non-ASCII — is
/// percent-encoded with uppercase hex. Encoding `/` too keeps the value a
/// single component, so a crafted file name cannot walk out of its month.
pub(super) fn percent_encode_component(name: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for &byte in name.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[usize::from(byte >> 4)] as char);
                out.push(HEX[usize::from(byte & 0x0f)] as char);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escaped(text: &str) -> String {
        let mut out = String::new();
        escape_inline(text, &mut out);
        out
    }

    #[test]
    fn html_significant_chars_become_entities() {
        assert_eq!(escaped("<script>a & b</script>"), {
            let mut expected = String::new();
            // `<` `>` `&` are the only entity substitutions; the rest is plain.
            expected.push_str("&lt;script&gt;a &amp; b&lt;/script&gt;");
            expected
        });
    }

    #[test]
    fn markdown_structural_chars_are_backslash_escaped() {
        assert_eq!(escaped("# heading"), "\\# heading");
        assert_eq!(escaped("- item"), "\\- item");
        assert_eq!(escaped("a * b _ c ` d"), "a \\* b \\_ c \\` d");
        assert_eq!(escaped("a|b"), "a\\|b");
        assert_eq!(escaped("---"), "\\-\\-\\-");
        assert_eq!(escaped("[x](y)"), "\\[x\\]\\(y\\)");
        assert_eq!(escaped("back\\slash"), "back\\\\slash");
    }

    #[test]
    fn inert_punctuation_and_unicode_pass_through() {
        assert_eq!(escaped("a, b: c; d? e/f @g"), "a, b: c; d? e/f @g");
        // Emoji (incl. ZWJ sequence), Cyrillic, and RTL text are untouched.
        assert_eq!(escaped("Привет 👨‍👩‍👧 مرحبا"), "Привет 👨‍👩‍👧 مرحبا");
    }

    #[test]
    fn control_characters_are_replaced() {
        assert_eq!(
            escaped("a\u{0}b\u{1}c\u{7f}d"),
            "a\u{fffd}b\u{fffd}c\u{fffd}d"
        );
        // Tab survives as layout.
        assert_eq!(escaped("a\tb"), "a\tb");
    }

    #[test]
    fn paragraph_joins_lines_with_hard_breaks() {
        assert_eq!(escape_paragraph("one\ntwo"), "one\\\ntwo");
        // CRLF and a blank middle line normalize and stay one paragraph.
        assert_eq!(escape_paragraph("a\r\n\r\nb"), "a\\\n\\\nb");
    }

    #[test]
    fn flattened_collapses_newlines_to_spaces() {
        assert_eq!(escape_flattened("a\nb\r\nc"), "a b c");
    }

    #[test]
    fn percent_encoding_keeps_unreserved_and_escapes_the_rest() {
        let mut out = String::new();
        percent_encode_component("IMG 0001 (1).jpg", &mut out);
        assert_eq!(out, "IMG%200001%20%281%29.jpg");
        out.clear();
        // Path separators are encoded, so a name cannot escape its month.
        percent_encode_component("../secret", &mut out);
        assert_eq!(out, "..%2Fsecret");
        out.clear();
        percent_encode_component("файл.pdf", &mut out);
        assert_eq!(out, "%D1%84%D0%B0%D0%B9%D0%BB.pdf");
    }
}
