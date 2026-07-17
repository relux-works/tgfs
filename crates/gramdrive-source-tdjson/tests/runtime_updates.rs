//! Update dispatch: routing by client, per-client order, bounded-queue
//! backpressure, dropped consumers, unroutable and malformed events —
//! everything counted, nothing silently lost (TASK-260715-2ulon7).

// clippy.toml exempts test code; restated for the module-level test bodies
// of this integration binary (matching the established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use common::{GUARD, echo_ok_responder, start_runtime, tagged_update, test_config};
use gramdrive_source_tdjson::RuntimeConfig;
use serde_json::json;

#[test]
fn updates_route_to_their_client_in_order() {
    let (runtime, mock) = start_runtime(test_config());
    let (client_a, updates_a) = runtime.create_client().unwrap();
    let (client_b, updates_b) = runtime.create_client().unwrap();

    mock.push_event(&tagged_update(client_a.client_id(), 1));
    mock.push_event(&tagged_update(client_b.client_id(), 10));
    mock.push_event(&tagged_update(client_a.client_id(), 2));
    mock.push_event(&tagged_update(client_b.client_id(), 20));

    assert_eq!(updates_a.recv_timeout(GUARD).unwrap()["tag"], 1);
    assert_eq!(updates_a.recv_timeout(GUARD).unwrap()["tag"], 2);
    assert_eq!(updates_b.recv_timeout(GUARD).unwrap()["tag"], 10);
    assert_eq!(updates_b.recv_timeout(GUARD).unwrap()["tag"], 20);
}

#[test]
fn full_queue_backpressures_without_losing_or_reordering() {
    let config = RuntimeConfig {
        update_queue_capacity: 1,
        ..test_config()
    };
    let (runtime, mock) = start_runtime(config);
    let (client, updates) = runtime.create_client().unwrap();

    // Five updates against a one-slot queue: the receive loop must block
    // and resume as the consumer drains, never dropping or reordering.
    for tag in 0..5 {
        mock.push_event(&tagged_update(client.client_id(), tag));
    }
    for tag in 0..5 {
        assert_eq!(updates.recv_timeout(GUARD).unwrap()["tag"], tag);
    }
    assert_eq!(runtime.stats().dropped_updates, 0);
}

#[test]
fn dropped_stream_disconnects_and_counts_later_updates() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();
    drop(updates);

    mock.push_event(&tagged_update(client.client_id(), 1));

    // Round-trip to prove the update was processed (in-order loop).
    mock.set_responder(echo_ok_responder());
    let probe = client.request(json!({"@type": "ping"})).unwrap();
    assert!(probe.wait_timeout(GUARD).expect("resolves").is_ok());

    assert_eq!(runtime.stats().dropped_updates, 1);
}

#[test]
fn unroutable_and_malformed_events_are_counted_not_fatal() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, _updates) = runtime.create_client().unwrap();

    mock.push_event(&tagged_update(9999, 1)); // no such client
    mock.push_event("this is not json");
    mock.push_event(r#"{"@type":"updateOption"}"#); // no routing members

    // The loop survives all three and still serves requests.
    mock.set_responder(echo_ok_responder());
    let probe = client.request(json!({"@type": "ping"})).unwrap();
    assert!(probe.wait_timeout(GUARD).expect("resolves").is_ok());

    let stats = runtime.stats();
    assert_eq!(stats.unroutable_updates, 1);
    assert_eq!(stats.malformed_events, 2);
    assert_eq!(stats.dropped_updates, 0);
}
