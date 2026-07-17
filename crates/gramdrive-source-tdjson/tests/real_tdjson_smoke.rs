//! Real-linkage smoke: the same runtime the mock tests exercise, driven
//! against the staged `libtdjson.dylib` (TASK-260715-2ulon7).
//!
//! Compiled only under `cfg(real_tdjson)` — the env gate `build.rs` turns
//! on when `GRAMDRIVE_TDLIB_ARTIFACT_DIR` points at the artifact — so this
//! file is an empty test binary in every gate run and a linked one under
//! `make tdjson-smoke`. Like the tdlib link smoke, it needs no api_id and
//! no network: `getOption "version"` is answered before authorization.
#![cfg(real_tdjson)]
// clippy.toml exempts test code; restated for the module-level test bodies
// of this integration binary (matching the established test-suite pattern).
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::time::Duration;

use gramdrive_source_tdjson::real::RealTdJson;
use gramdrive_source_tdjson::{RuntimeConfig, TdRuntime, UpdateRecvError};
use serde_json::json;

/// TDLib answers the version probe from local state, but its client thread
/// has to start first; generous so slow CI cannot flake this.
const REAL_GUARD: Duration = Duration::from_secs(30);

#[test]
fn runtime_over_real_tdjson_correlates_and_shuts_down() {
    let (sender, receiver) = RealTdJson::claim().expect("first claim gets the receiver");
    assert!(
        RealTdJson::claim().is_none(),
        "the receive stream is single-owner"
    );

    let runtime =
        TdRuntime::start(sender, receiver, RuntimeConfig::default()).expect("runtime starts");

    // td_execute round-trip (and quiet logging for the smoke output).
    let answer = runtime
        .execute(&json!({"@type": "setLogVerbosityLevel", "new_verbosity_level": 1}))
        .expect("execute answers");
    assert_eq!(answer["@type"], "ok");

    // One correlated request against a live client: the version comes out
    // of the running library, through the runtime's @extra correlation.
    let (client, updates) = runtime.create_client().expect("client registers");
    let pending = client
        .request(json!({"@type": "getOption", "name": "version"}))
        .expect("request submits");
    let request_id = pending.request_id();
    let version = pending
        .wait_timeout(REAL_GUARD)
        .expect("TDLib answered the version probe")
        .expect("probe answer is not an error");
    assert_eq!(version["@extra"], request_id);
    let version_string = version["value"].as_str().expect("a version string");
    assert!(!version_string.is_empty());

    // Clean client close: ok response, then authorizationStateClosed on
    // the update stream, then a closed stream.
    let closing = client.close().expect("close submits");
    assert!(
        closing
            .wait_timeout(REAL_GUARD)
            .expect("close answered")
            .is_ok()
    );
    loop {
        let update = updates
            .recv_timeout(REAL_GUARD)
            .expect("updates flow until the closed state");
        if update["@type"] == "updateAuthorizationState"
            && update["authorization_state"]["@type"] == "authorizationStateClosed"
        {
            break;
        }
    }
    assert_eq!(
        updates.recv_timeout(Duration::ZERO),
        Err(UpdateRecvError::Closed)
    );

    runtime.shutdown();
}
