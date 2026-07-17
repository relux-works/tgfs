//! Shutdown coordination: ready responses drain into their pending
//! requests, the rest fail with `Shutdown` instead of hanging, a receive
//! loop blocked on update backpressure cannot deadlock the exit, and
//! handles that outlive the runtime degrade cleanly (TASK-260715-2ulon7).

// clippy.toml exempts test code; restated for the module-level test bodies
// of this integration binary (matching the established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

mod common;

use std::time::Duration;

use common::{GUARD, ok_response, start_runtime, tagged_update, test_config};
use gramdrive_source_tdjson::{RuntimeConfig, TdError, UpdateRecvError};
use serde_json::json;

#[test]
fn shutdown_drains_ready_responses_and_fails_the_rest() {
    let (runtime, mock) = start_runtime(test_config());
    let (client, _updates) = runtime.create_client().unwrap();

    let answered = client.request(json!({"@type": "getOption"})).unwrap();
    let stranded = client.request(json!({"@type": "getChats"})).unwrap();
    let answered_extra = mock.take_sent()[0].extra().unwrap();

    // One answer is already on the stream when shutdown begins…
    mock.push_event(&ok_response(answered_extra, client.client_id()));
    runtime.shutdown();

    // …so it resolves normally (the drain), while the unanswered request
    // fails with the lifecycle error instead of hanging.
    assert!(
        answered
            .wait_timeout(Duration::ZERO)
            .expect("resolved by the drain")
            .is_ok()
    );
    assert_eq!(
        stranded.wait_timeout(Duration::ZERO).expect("resolved"),
        Err(TdError::Shutdown)
    );
    // The runtime is gone: the surviving client handle degrades cleanly.
    assert_eq!(
        client.request(json!({"@type": "ping"})).map(drop),
        Err(TdError::Shutdown)
    );
}

#[test]
fn shutdown_cannot_deadlock_against_update_backpressure() {
    let config = RuntimeConfig {
        update_queue_capacity: 1,
        ..test_config()
    };
    let (runtime, mock) = start_runtime(config);
    let (client, updates) = runtime.create_client().unwrap();

    // Three updates against a one-slot queue nobody fully drains: taking
    // only the first guarantees the second was (or will be) delivered and
    // the third can never fit — the loop is blocked or about to be.
    for tag in 1..=3 {
        mock.push_event(&tagged_update(client.client_id(), tag));
    }
    assert_eq!(updates.recv_timeout(GUARD).unwrap()["tag"], 1);

    // Shutdown must complete anyway: closing the queue wakes the blocked
    // push. The watchdog turns a deadlock into a failure, not a hung CI.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let shutdown_thread = std::thread::spawn(move || {
        runtime.shutdown();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(GUARD)
        .expect("shutdown completed without deadlock");
    shutdown_thread.join().expect("shutdown thread exits");

    // Which of tags 2 and 3 made it into the buffer before the close is a
    // timing race shutdown is allowed to cut short; what is guaranteed is
    // order (a gap-free continuation from tag 2) and a closed stream at
    // the end — updates are dropped at the tail, never reordered or lost
    // from the middle.
    let mut expected_next = 2;
    loop {
        match updates.try_recv() {
            Ok(update) => {
                assert_eq!(update["tag"], expected_next);
                expected_next += 1;
            }
            Err(UpdateRecvError::Closed) => break,
            Err(UpdateRecvError::Timeout) => panic!("stream still open after shutdown"),
        }
    }
    assert!(expected_next <= 4, "no update was delivered twice");
}

#[test]
fn drop_shuts_the_runtime_down() {
    let (runtime, _mock) = start_runtime(test_config());
    let (client, updates) = runtime.create_client().unwrap();
    let stranded = client.request(json!({"@type": "getOption"})).unwrap();

    drop(runtime);

    assert_eq!(
        stranded.wait_timeout(Duration::ZERO).expect("resolved"),
        Err(TdError::Shutdown)
    );
    assert_eq!(
        updates.recv_timeout(Duration::ZERO),
        Err(UpdateRecvError::Closed)
    );
    assert_eq!(
        client.request(json!({"@type": "ping"})).map(drop),
        Err(TdError::Shutdown)
    );
}

#[test]
fn shutdown_with_no_clients_and_no_traffic_is_immediate_and_clean() {
    let (runtime, _mock) = start_runtime(test_config());
    let stats = runtime.stats();
    runtime.shutdown();
    assert_eq!(stats, gramdrive_source_tdjson::RuntimeStats::default());
}
