//! Shared fixtures for the runtime's integration tests: a runtime over a
//! fresh mock, canned event builders, and a dependency-free `block_on` for
//! the `Future` path of `PendingRequest`.

// clippy.toml exempts test code on the grounds that a panicking test is
// just a failing test. That exemption keys on `#[test]` functions, and the
// shared fixture helpers below sit at module level in an integration-test
// binary. The rationale still applies in full — this file links into no
// product artifact — so the exemption is restated here, matching the
// established test-suite pattern.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]
// Each test binary uses the subset of fixtures it needs.
#![allow(dead_code)]

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use gramdrive_source_tdjson::mock::{MockHandle, MockTdJson, SentRequest};
use gramdrive_source_tdjson::{RuntimeConfig, TdRuntime};

/// Upper bound on any wait: generous enough to never fire on a healthy
/// run, so no assertion depends on timing — a fired guard is a failure.
pub(crate) const GUARD: Duration = Duration::from_secs(5);

/// A config with a short idle poll so shutdown paths resolve quickly in
/// tests; assertions never depend on the value.
pub(crate) fn test_config() -> RuntimeConfig {
    RuntimeConfig {
        receive_timeout: Duration::from_millis(20),
        ..RuntimeConfig::default()
    }
}

/// A runtime over a fresh mock.
pub(crate) fn start_runtime(config: RuntimeConfig) -> (TdRuntime, MockHandle) {
    let (sender, receiver, handle) = MockTdJson::new();
    let runtime = TdRuntime::start(sender, receiver, config).expect("receive loop spawns");
    (runtime, handle)
}

/// A canned `{"@type":"ok"}` response for request `extra` of `client_id`.
pub(crate) fn ok_response(extra: u64, client_id: i32) -> String {
    format!(r#"{{"@type":"ok","@extra":{extra},"@client_id":{client_id}}}"#)
}

/// A canned update for `client_id` carrying a recognizable `tag`.
pub(crate) fn tagged_update(client_id: i32, tag: u32) -> String {
    format!(r#"{{"@type":"updateOption","tag":{tag},"@client_id":{client_id}}}"#)
}

/// The `updateAuthorizationState` → `authorizationStateClosed` update that
/// ends `client_id`'s lifecycle.
pub(crate) fn closed_update(client_id: i32) -> String {
    format!(
        concat!(
            r#"{{"@type":"updateAuthorizationState","#,
            r#""authorization_state":{{"@type":"authorizationStateClosed"}},"#,
            r#""@client_id":{}}}"#
        ),
        client_id
    )
}

/// A responder answering every request with `{"@type":"ok"}` under the
/// request's own `@extra` — the mock-side mirror of tdjson answering.
pub(crate) fn echo_ok_responder() -> impl FnMut(&SentRequest) -> Vec<String> + Send + 'static {
    |sent: &SentRequest| {
        let extra = sent.extra().expect("runtime injects @extra");
        vec![ok_response(extra, sent.client_id)]
    }
}

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drive a future to completion on this thread, guarded by [`GUARD`].
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    let deadline = Instant::now() + GUARD;
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => {
                assert!(
                    Instant::now() < deadline,
                    "future did not resolve within the guard"
                );
                std::thread::park_timeout(Duration::from_millis(50));
            }
        }
    }
}
