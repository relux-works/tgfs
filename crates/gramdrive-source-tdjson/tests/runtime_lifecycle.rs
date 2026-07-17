//! Client lifecycle and request correlation over the deterministic mock:
//! out-of-order responses land on their own requests, error objects become
//! typed errors, `td_execute` round-trips, close ends a client's lifecycle
//! and repeated create/close cycles stay clean (TASK-260715-2ulon7).

// clippy.toml exempts test code; restated for the module-level test bodies
// of this integration binary (matching the established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::time::Duration;

use common::{
    GUARD, closed_update, echo_ok_responder, ok_response, start_runtime, tagged_update, test_config,
};
use gramdrive_source_tdjson::{TdError, UpdateRecvError};
use serde_json::json;

#[test]
fn responses_correlate_by_request_id_even_out_of_order() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, _updates) = runtime.create_client().unwrap();

    let first = client
        .request(json!({"@type": "getOption", "name": "a"}))
        .unwrap();
    let second = client
        .request(json!({"@type": "getOption", "name": "b"}))
        .unwrap();
    let sent = mock.take_sent();
    assert_eq!(sent.len(), 2);
    let first_extra = sent[0].extra().unwrap();
    let second_extra = sent[1].extra().unwrap();
    assert_eq!(first_extra, first.request_id());
    assert_eq!(second_extra, second.request_id());

    // Answer in reverse order, with distinguishable payloads.
    mock.push_event(&format!(
        r#"{{"@type":"optionValueString","value":"B","@extra":{second_extra},"@client_id":1}}"#
    ));
    mock.push_event(&format!(
        r#"{{"@type":"optionValueString","value":"A","@extra":{first_extra},"@client_id":1}}"#
    ));

    let first = first.wait_timeout(GUARD).expect("resolves").unwrap();
    let second = second.wait_timeout(GUARD).expect("resolves").unwrap();
    assert_eq!(first["value"], "A");
    assert_eq!(second["value"], "B");
}

#[test]
fn error_responses_become_typed_errors() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, _updates) = runtime.create_client().unwrap();

    let pending = client.request(json!({"@type": "getChat"})).unwrap();
    let extra = mock.take_sent()[0].extra().unwrap();
    mock.push_event(&format!(
        r#"{{"@type":"error","code":404,"message":"Not Found","@extra":{extra},"@client_id":1}}"#
    ));

    assert_eq!(
        pending.wait_timeout(GUARD).expect("resolves"),
        Err(TdError::Td {
            code: 404,
            message: "Not Found".to_owned(),
        })
    );
}

#[test]
fn requests_are_validated_before_reaching_tdjson() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, _updates) = runtime.create_client().unwrap();

    assert!(matches!(
        client.request(json!("not an object")),
        Err(TdError::InvalidRequest { .. })
    ));
    assert!(matches!(
        client.request(json!({"@type": "ping", "@extra": 1})),
        Err(TdError::InvalidRequest { .. })
    ));
    assert!(mock.take_sent().is_empty(), "nothing reached the mock");
}

#[test]
fn execute_round_trips_and_converts_errors() {
    let (runtime, mock) = start_runtime(test_config());

    mock.set_execute_answer(Some(r#"{"@type":"ok"}"#));
    assert_eq!(
        runtime
            .execute(&json!({"@type": "setLogVerbosityLevel"}))
            .unwrap()["@type"],
        "ok"
    );

    mock.set_execute_answer(Some(
        r#"{"@type":"error","code":400,"message":"Unsupported in execute"}"#,
    ));
    assert_eq!(
        runtime.execute(&json!({"@type": "getChat"})),
        Err(TdError::Td {
            code: 400,
            message: "Unsupported in execute".to_owned(),
        })
    );

    mock.set_execute_answer(None);
    assert!(matches!(
        runtime.execute(&json!({"@type": "ping"})),
        Err(TdError::Protocol { .. })
    ));
}

#[test]
fn closed_update_fails_pending_and_ends_the_client() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();

    let stranded = client.request(json!({"@type": "getOption"})).unwrap();
    mock.push_event(&closed_update(client.client_id()));

    // The pending request fails with the lifecycle error, not a hang.
    assert_eq!(
        stranded.wait_timeout(GUARD).expect("resolves"),
        Err(TdError::ClientClosed)
    );
    // The closed update itself is delivered, then the stream ends.
    let last = updates.recv_timeout(GUARD).unwrap();
    assert_eq!(last["@type"], "updateAuthorizationState");
    assert_eq!(
        updates.recv_timeout(Duration::ZERO),
        Err(UpdateRecvError::Closed)
    );
    // New requests on the closed client are rejected locally.
    assert_eq!(
        client.request(json!({"@type": "ping"})).map(drop),
        Err(TdError::ClientClosed)
    );
}

#[test]
fn repeated_create_close_cycles_stay_clean() {
    let (runtime, mock) = start_runtime(test_config());
    mock.set_responder({
        let mut base = echo_ok_responder();
        move |sent| {
            let mut events = base(sent);
            if sent.request_type().as_deref() == Some("close") {
                events.push(closed_update(sent.client_id));
            }
            events
        }
    });

    let mut seen_ids = Vec::new();
    for _ in 0..3 {
        let (client, updates) = runtime.create_client().unwrap();
        seen_ids.push(client.client_id());

        // The client works…
        let answer = client
            .request(json!({"@type": "getOption", "name": "version"}))
            .unwrap()
            .wait_timeout(GUARD)
            .expect("resolves")
            .unwrap();
        assert_eq!(answer["@type"], "ok");

        // …then closes cleanly: ok response, closed update, dead handle.
        let closing = client.close().unwrap();
        assert!(closing.wait_timeout(GUARD).expect("resolves").is_ok());
        loop {
            let update = updates.recv_timeout(GUARD).unwrap();
            if update["@type"] == "updateAuthorizationState" {
                break;
            }
        }
        assert_eq!(
            updates.recv_timeout(Duration::ZERO),
            Err(UpdateRecvError::Closed)
        );
        assert_eq!(
            client.request(json!({"@type": "ping"})).map(drop),
            Err(TdError::ClientClosed)
        );
    }
    seen_ids.dedup();
    assert_eq!(seen_ids.len(), 3, "every cycle got its own client id");

    // The runtime absorbed nothing silently across the cycles.
    let stats = runtime.stats();
    assert_eq!(stats, gramdrive_source_tdjson::RuntimeStats::default());
}

#[test]
fn update_order_is_preserved_per_client() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();

    for tag in 0..5 {
        mock.push_event(&tagged_update(client.client_id(), tag));
    }
    for tag in 0..5 {
        assert_eq!(updates.recv_timeout(GUARD).unwrap()["tag"], tag);
    }
}

#[test]
fn wait_timeout_returns_the_handle_and_the_request_stays_pending() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, _updates) = runtime.create_client().unwrap();

    let pending = client.request(json!({"@type": "getOption"})).unwrap();
    let pending = pending
        .wait_timeout(Duration::from_millis(50))
        .expect_err("no answer yet — the handle comes back");

    let extra = mock.take_sent()[0].extra().unwrap();
    mock.push_event(&ok_response(extra, client.client_id()));
    assert!(pending.wait_timeout(GUARD).expect("resolves").is_ok());
    let _ = runtime;
}
