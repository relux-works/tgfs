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
