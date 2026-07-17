//! Classification of raw receive-stream events.
//!
//! Every string `td_receive` hands back is one JSON object. Its envelope
//! members decide where it goes:
//!
//! - `@extra` present → a response to a request this runtime sent. The
//!   runtime injects `@extra` as a JSON number it minted, and rejects
//!   caller-supplied `@extra` at submission, so on this stream an `@extra`
//!   that is not one of our numbers is a protocol violation, not someone
//!   else's traffic.
//! - no `@extra`, `@client_id` present → an update for that client.
//! - anything else → malformed; counted, never fatal to the loop.
//!
//! An `{"@type":"error"}` payload under a known `@extra` becomes the typed
//! [`TdError::Td`] here — the single point of error conversion, so a
//! response future resolves to `Result` without every caller re-parsing.

use serde_json::Value;

use crate::error::TdError;

/// A classified receive-stream event.
pub(crate) enum Event {
    /// A response to request `extra`, already converted to the typed result.
    Response {
        /// The correlation id the runtime injected as `@extra`.
        extra: u64,
        /// The response payload, or the typed TDLib error.
        payload: Result<Value, TdError>,
    },
    /// An update belonging to `client_id`.
    Update {
        /// The client the update belongs to (`@client_id`).
        client_id: i32,
        /// The update object, unmodified.
        payload: Value,
    },
    /// An event the envelope rules cannot place; `detail` says why.
    Malformed {
        /// Diagnostic detail for the stats counter path.
        detail: String,
    },
}

pub(crate) fn classify(raw: &str) -> Event {
    let value: Value = match serde_json::from_str(raw) {
        Ok(value) => value,
        Err(err) => {
            return Event::Malformed {
                detail: format!("unparseable event: {err}"),
            };
        }
    };
    if !value.is_object() {
        return Event::Malformed {
            detail: "event is not a JSON object".to_owned(),
        };
    }

    if let Some(extra) = value.get("@extra") {
        let Some(extra) = extra.as_u64() else {
            return Event::Malformed {
                detail: "@extra is not a runtime-minted number".to_owned(),
            };
        };
        let payload = if value.get("@type").and_then(Value::as_str) == Some("error") {
            Err(TdError::from_error_object(&value))
        } else {
            Ok(value)
        };
        return Event::Response { extra, payload };
    }

    let client_id = value
        .get("@client_id")
        .and_then(Value::as_i64)
        .and_then(|id| i32::try_from(id).ok());
    match client_id {
        Some(client_id) => Event::Update {
            client_id,
            payload: value,
        },
        None => Event::Malformed {
            detail: "event carries neither @extra nor a usable @client_id".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_with_extra_is_correlated() {
        let event = classify(r#"{"@type":"ok","@extra":7,"@client_id":1}"#);
        match event {
            Event::Response { extra, payload } => {
                assert_eq!(extra, 7);
                assert_eq!(payload.unwrap()["@type"], "ok");
            }
            _ => panic!("expected a response"),
        }
    }

    #[test]
    fn error_response_converts_to_typed_error() {
        let event = classify(r#"{"@type":"error","code":404,"message":"Not Found","@extra":3}"#);
        match event {
            Event::Response { extra, payload } => {
                assert_eq!(extra, 3);
                assert_eq!(
                    payload.unwrap_err(),
                    TdError::Td {
                        code: 404,
                        message: "Not Found".to_owned(),
                    }
                );
            }
            _ => panic!("expected a response"),
        }
    }

    #[test]
    fn update_routes_by_client_id() {
        let event = classify(r#"{"@type":"updateOption","@client_id":2}"#);
        match event {
            Event::Update { client_id, payload } => {
                assert_eq!(client_id, 2);
                assert_eq!(payload["@type"], "updateOption");
            }
            _ => panic!("expected an update"),
        }
    }

    #[test]
    fn malformed_events_are_classified_not_fatal() {
        for raw in [
            "not json",
            "[1,2,3]",
            r#"{"@type":"updateOption"}"#,
            r#"{"@type":"ok","@extra":"foreign"}"#,
            r#"{"@type":"updateOption","@client_id":99999999999}"#,
        ] {
            assert!(matches!(classify(raw), Event::Malformed { .. }), "{raw}");
        }
    }
}
