//! The typed failure surface of the tdjson runtime.
//!
//! Two families share one enum. [`TdError::Td`] is the typed conversion of
//! a TDLib `{"@type":"error","code":…,"message":…}` object — the answer
//! TDLib gives to a request it rejects. Every other variant is minted by
//! the runtime itself: lifecycle states (client closed, runtime shut down),
//! caller mistakes caught before anything reaches tdjson, and protocol
//! breakage on the receive stream.
//!
//! Normalizing these into the provider-neutral `SourceError` taxonomy
//! (DEC-003 — no TDLib error type crosses the source boundary) is the
//! `DriveSource` adapter's job in the follow-up tasks; nothing here leaks
//! past this crate.

use serde_json::Value;

/// Why a tdjson request or runtime operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TdError {
    /// TDLib answered the request with an `{"@type":"error"}` object.
    Td {
        /// TDLib's numeric error code (`code`).
        code: i64,
        /// TDLib's human-readable message (`message`); diagnostic, not
        /// contractual.
        message: String,
    },
    /// The request was rejected before reaching tdjson: not a JSON object,
    /// or it already carries an `@extra` member (correlation ids belong to
    /// the runtime alone).
    InvalidRequest {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The target client reached `authorizationStateClosed`; no further
    /// request on it can succeed.
    ClientClosed,
    /// The runtime is shut down (or shutting down); the request was failed
    /// rather than left pending forever.
    Shutdown,
    /// The receive stream or an implementation violated the tdjson
    /// protocol — unparseable event, a null `td_execute` answer, a
    /// response handle consumed twice.
    Protocol {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
}

impl TdError {
    /// Convert a parsed `{"@type":"error"}` object into [`TdError::Td`].
    ///
    /// Missing members degrade to `code: 0` / an empty message rather than
    /// failing: an error answer with a mangled shape is still an error
    /// answer, and the request must still resolve.
    pub(crate) fn from_error_object(value: &Value) -> TdError {
        TdError::Td {
            code: value.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

impl std::fmt::Display for TdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TdError::Td { code, message } => write!(f, "TDLib error {code}: {message}"),
            TdError::InvalidRequest { detail } => write!(f, "invalid tdjson request: {detail}"),
            TdError::ClientClosed => write!(f, "tdjson client is closed"),
            TdError::Shutdown => write!(f, "tdjson runtime is shut down"),
            TdError::Protocol { detail } => write!(f, "tdjson protocol violation: {detail}"),
        }
    }
}

impl std::error::Error for TdError {}

/// Retryable-failure classification (SYNC-044): `Some(stated delay)` for
/// flood control (code 429 / `FLOOD_WAIT`), `Some(None)` for TDLib's
/// transport failures (code 500). Everything else is not retry advice —
/// what it *is* instead is each machine's call (`snapshot` fails its run,
/// `history` fails the one chat).
pub(crate) fn retryable_after(error: &TdError) -> Option<Option<u64>> {
    match error {
        TdError::Td { code, message } => {
            if *code == 429
                || message.starts_with("Too Many Requests")
                || message.starts_with("FLOOD_WAIT")
            {
                Some(trailing_integer(message))
            } else if *code == 500 {
                Some(None)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The trailing decimal integer of `message`, if it ends with one — how
/// both flood-wait message shapes ("Too Many Requests: retry after 17",
/// "FLOOD_WAIT_17") state their delay. Shared by every flow that
/// classifies TDLib rejections (`auth`, `snapshot`; SYNC-044).
pub(crate) fn trailing_integer(message: &str) -> Option<u64> {
    let trimmed = message.trim_end();
    let digits = trimmed.len() - trimmed.bytes().rev().take_while(u8::is_ascii_digit).count();
    trimmed.get(digits..)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{TdError, retryable_after, trailing_integer};

    #[test]
    fn retryable_classification_matches_flood_and_transport_only() {
        let flood = TdError::Td {
            code: 429,
            message: "Too Many Requests: retry after 17".to_owned(),
        };
        assert_eq!(retryable_after(&flood), Some(Some(17)));
        let flood_bare = TdError::Td {
            code: 420,
            message: "FLOOD_WAIT_120".to_owned(),
        };
        assert_eq!(retryable_after(&flood_bare), Some(Some(120)));
        let transport = TdError::Td {
            code: 500,
            message: "Failed to connect".to_owned(),
        };
        assert_eq!(retryable_after(&transport), Some(None));
        let fatal = TdError::Td {
            code: 400,
            message: "CHAT_ID_INVALID".to_owned(),
        };
        assert_eq!(retryable_after(&fatal), None);
        assert_eq!(retryable_after(&TdError::ClientClosed), None);
        assert_eq!(retryable_after(&TdError::Shutdown), None);
    }

    #[test]
    fn trailing_integer_parses_both_flood_message_shapes() {
        assert_eq!(
            trailing_integer("Too Many Requests: retry after 17"),
            Some(17)
        );
        assert_eq!(trailing_integer("FLOOD_WAIT_120"), Some(120));
        assert_eq!(trailing_integer("FLOOD_WAIT_3 "), Some(3));
        assert_eq!(trailing_integer("Too Many Requests"), None);
        assert_eq!(trailing_integer(""), None);
        // An integer too large for u64 is no advice at all.
        assert_eq!(trailing_integer("wait 99999999999999999999999999"), None);
    }
}
