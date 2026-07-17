//! Deterministic, compact JSON writer for the renderers.
//!
//! The core hand-rolls JSON for the same reasons `gramdrive-model`'s
//! `order.json` writer does (`gramdrive_model::ordering`): the requirement is a
//! small, fully specified escaping rule (RFC 8259 §7), and byte-stable output
//! is the whole point — a dependency would add supply-chain surface (POL-6)
//! and, with map-backed serializers, a field order that is not guaranteed
//! stable across versions. [`Json`] makes field order a property of the value:
//! an [`Json::Object`] is an ordered list of `(key, value)` pairs, so the order
//! written is exactly the order built (SYNC-030).
//!
//! There are deliberately no floats. Every number the message schema carries is
//! a Telegram identifier, an index, a count, or a millisecond timestamp — an
//! integer. A JSON float would reintroduce the IEEE-754 rounding `order.json`
//! avoids, where two distinct int64 values can collide after a round trip
//! through an f64 parser.

use std::borrow::Cow;

/// A JSON value the renderers can serialize deterministically.
///
/// Strings borrow where they can ([`Json::str`]) so a large message body is
/// written straight from the input without a copy; owned strings
/// ([`Json::owned`]) cover values the renderer computes, such as an item-id
/// text form or a hex digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Json<'a> {
    /// JSON `null`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed integer — Telegram ids and millisecond timestamps.
    I64(i64),
    /// An unsigned integer — indices, counts, and byte sizes.
    U64(u64),
    /// A string, borrowed or owned.
    Str(Cow<'a, str>),
    /// An array, written in element order.
    Array(Vec<Json<'a>>),
    /// An object, written in the exact field order given here.
    Object(Vec<(&'static str, Json<'a>)>),
}

impl<'a> Json<'a> {
    /// A string value borrowed from the input.
    pub(crate) fn str(value: &'a str) -> Self {
        Json::Str(Cow::Borrowed(value))
    }

    /// A string value the renderer computed and owns.
    pub(crate) fn owned(value: String) -> Self {
        Json::Str(Cow::Owned(value))
    }

    /// Writes the compact, single-line encoding of this value.
    ///
    /// Compact means no insignificant whitespace: objects are
    /// `{"k":v,"k2":v2}` and arrays `[v,v2]`. One value renders to one NDJSON
    /// line, and equal values render byte-identical output.
    pub(crate) fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::I64(value) => out.push_str(&value.to_string()),
            Json::U64(value) => out.push_str(&value.to_string()),
            Json::Str(value) => write_json_string(out, value),
            Json::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(fields) => {
                out.push('{');
                for (index, (key, value)) in fields.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_json_string(out, key);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Writes `value` as a JSON string literal (RFC 8259 §7).
///
/// Hand-rolled for the reason `gramdrive_model::ordering` gives: the whole
/// requirement is one escaping rule, and a dependency would be more
/// supply-chain surface (POL-6) than code. Rust's `&str` is valid UTF-8, so
/// the lone-surrogate case that makes JSON escaping genuinely hard cannot
/// arise; what is left is the two mandatory escapes and the C0 controls.
fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // Every other C0 control has no short escape and must not appear
            // raw. \u00xx is the general form; DEL (0x7f) is legal raw and is
            // deliberately not escaped.
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(value: &Json<'_>) -> String {
        let mut out = String::new();
        value.write(&mut out);
        out
    }

    #[test]
    fn writes_scalars() {
        assert_eq!(render(&Json::Null), "null");
        assert_eq!(render(&Json::Bool(true)), "true");
        assert_eq!(render(&Json::Bool(false)), "false");
        assert_eq!(render(&Json::I64(-42)), "-42");
        assert_eq!(render(&Json::U64(42)), "42");
        assert_eq!(render(&Json::str("hi")), "\"hi\"");
    }

    #[test]
    fn writes_objects_in_field_order() {
        let value = Json::Object(vec![
            ("b", Json::U64(2)),
            ("a", Json::U64(1)),
            ("nested", Json::Array(vec![Json::Null, Json::Bool(true)])),
        ]);
        // Field order is the order given, not sorted: "b" before "a".
        assert_eq!(render(&value), "{\"b\":2,\"a\":1,\"nested\":[null,true]}");
    }

    #[test]
    fn escapes_mandatory_and_control_characters() {
        // Build the input from the raw characters so the test does not depend
        // on how this file's own escapes are read: quote, backslash, newline,
        // tab, then U+0001 (a control with no short escape).
        let input: String = [
            'a', '"', 'b', '\\', 'c', '\n', 'd', '\t', 'e', '\u{01}', 'f',
        ]
        .into_iter()
        .collect();
        // Expected, assembled the same way: \" \\ \n \t and the general .
        let expected: String = [
            "\"", "a", "\\\"", "b", "\\\\", "c", "\\n", "d", "\\t", "e", "\\u0001", "f", "\"",
        ]
        .concat();
        assert_eq!(render(&Json::str(&input)), expected);
    }

    #[test]
    fn leaves_unicode_and_del_raw() {
        let input = "Привет 👨‍👩‍👧 \u{7f}";
        let mut expected = String::from("\"");
        expected.push_str(input);
        expected.push('"');
        assert_eq!(render(&Json::str(input)), expected);
    }

    #[test]
    fn large_int64_stays_exact() {
        // The value an f64 round trip would round; a string parser reading our
        // output sees the exact integer.
        assert_eq!(
            render(&Json::I64(9_007_199_254_740_993)),
            "9007199254740993"
        );
    }
}
