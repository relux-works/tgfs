//! Cancellation mid-flight: dropped or cancelled handles remove the
//! correlation entry, late and duplicate responses are discarded and
//! counted — never delivered, never blocking — and the async path of
//! `PendingRequest` resolves like the blocking one (TASK-260715-2ulon7).

// clippy.toml exempts test code; restated for the module-level test bodies
// of this integration binary (matching the established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use common::{GUARD, block_on, echo_ok_responder, ok_response, start_runtime, test_config};
use serde_json::json;

#[test]
fn dropped_handle_discards_the_late_response() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, _updates) = runtime.create_client().unwrap();

    let cancelled = client.request(json!({"@type": "getChats"})).unwrap();
    let extra = mock.take_sent()[0].extra().unwrap();
    drop(cancelled);

    // The response arrives after cancellation…
    mock.push_event(&ok_response(extra, client.client_id()));

    // …and a follow-up round-trip proves the loop processed it: events are
    // handled in order, so by the time the probe resolves, the cancelled
    // response has been classified.
    mock.set_responder(echo_ok_responder());
    let probe = client.request(json!({"@type": "ping"})).unwrap();
    assert!(probe.wait_timeout(GUARD).expect("resolves").is_ok());

    assert_eq!(runtime.stats().discarded_responses, 1);
}

#[test]
fn explicit_cancel_behaves_like_drop() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, _updates) = runtime.create_client().unwrap();

    let cancelled = client.request(json!({"@type": "getChats"})).unwrap();
    let extra = mock.take_sent()[0].extra().unwrap();
    cancelled.cancel();

    mock.push_event(&ok_response(extra, client.client_id()));
    mock.set_responder(echo_ok_responder());
    let probe = client.request(json!({"@type": "ping"})).unwrap();
    assert!(probe.wait_timeout(GUARD).expect("resolves").is_ok());

    assert_eq!(runtime.stats().discarded_responses, 1);
}

#[test]
fn duplicate_response_is_discarded_after_the_first_wins() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, _updates) = runtime.create_client().unwrap();

    let pending = client.request(json!({"@type": "getOption"})).unwrap();
    let extra = mock.take_sent()[0].extra().unwrap();
    mock.push_event(&ok_response(extra, client.client_id()));
    assert!(pending.wait_timeout(GUARD).expect("resolves").is_ok());

    // The same @extra again: no pending entry anymore — discarded.
    mock.push_event(&ok_response(extra, client.client_id()));
    mock.set_responder(echo_ok_responder());
    let probe = client.request(json!({"@type": "ping"})).unwrap();
    assert!(probe.wait_timeout(GUARD).expect("resolves").is_ok());

    assert_eq!(runtime.stats().discarded_responses, 1);
}

#[test]
fn pending_request_resolves_through_the_async_path() {
    let (runtime, mock) = start_runtime(test_config());
    mock.set_responder(echo_ok_responder());
    let (client, _updates) = runtime.create_client().unwrap();

    let answer = block_on(client.request(json!({"@type": "ping"})).unwrap()).unwrap();
    assert_eq!(answer["@type"], "ok");
    let _ = runtime;
}

#[test]
fn cancellation_does_not_disturb_other_pending_requests() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, _updates) = runtime.create_client().unwrap();

    let cancelled = client.request(json!({"@type": "getChats"})).unwrap();
    let kept = client.request(json!({"@type": "getOption"})).unwrap();
    let sent = mock.take_sent();
    let cancelled_extra = sent[0].extra().unwrap();
    let kept_extra = sent[1].extra().unwrap();
    drop(cancelled);

    mock.push_event(&ok_response(cancelled_extra, client.client_id()));
    mock.push_event(&ok_response(kept_extra, client.client_id()));

    assert!(kept.wait_timeout(GUARD).expect("resolves").is_ok());
    assert_eq!(runtime.stats().discarded_responses, 1);
}
