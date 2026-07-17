//! The deterministic in-process tdjson double.
//!
//! [`MockTdJson::new`] hands back the same two halves the real
//! implementation does — a [`TdSendApi`] and a [`TdReceiveApi`] — plus a
//! [`MockHandle`] the test drives. Nothing here is timed or threaded:
//! events appear on the receive stream exactly when the test (or its
//! responder) puts them there, which is what makes the runtime's
//! concurrency and shutdown tests deterministic. This module also carries
//! the crate's ability to compile and test with no TDLib artifact staged —
//! the mock is always compiled; the real FFI only exists under the
//! `real_tdjson` env gate.
//!
//! # Scripting
//!
//! - [`MockHandle::push_event`] — put one event on the receive stream
//!   (a response with a chosen `@extra`, an update, malformed junk).
//! - [`MockHandle::set_responder`] — a hook run synchronously inside
//!   `send`; whatever events it returns are queued immediately. This is
//!   how a test answers requests whose `@extra` the runtime minted —
//!   [`SentRequest::extra`] exposes it. The hook runs under the mock's
//!   lock: it must not call back into the handle.
//! - [`MockHandle::take_sent`] — drain the record of everything sent, in
//!   order, for asserting on outgoing traffic or answering manually.
//! - [`MockHandle::set_execute_answer`] — the canned `td_execute` answer
//!   (`None`, the default, models tdjson returning nothing).

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::api::{TdReceiveApi, TdSendApi};

/// One request recorded by the mock's `send`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentRequest {
    /// The client the request was addressed to.
    pub client_id: i32,
    /// The serialized request exactly as the runtime sent it.
    pub json: String,
}

impl SentRequest {
    /// The `@extra` correlation id the runtime injected, when the request
    /// parses and carries one.
    pub fn extra(&self) -> Option<u64> {
        let value: Value = serde_json::from_str(&self.json).ok()?;
        value.get("@extra").and_then(Value::as_u64)
    }

    /// The request's `@type`, when the request parses and carries one.
    pub fn request_type(&self) -> Option<String> {
        let value: Value = serde_json::from_str(&self.json).ok()?;
        Some(value.get("@type")?.as_str()?.to_owned())
    }
}

type Responder = Box<dyn FnMut(&SentRequest) -> Vec<String> + Send>;

struct MockState {
    events: VecDeque<String>,
    sent: Vec<SentRequest>,
    responder: Option<Responder>,
    execute_answer: Option<String>,
    next_client_id: i32,
}

struct MockShared {
    state: Mutex<MockState>,
    event_ready: Condvar,
}

impl MockShared {
    fn lock(&self) -> MutexGuard<'_, MockState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Factory for one connected mock: a send half, a receive half, and the
/// test's control handle, all over the same event stream.
#[derive(Debug)]
pub struct MockTdJson;

impl MockTdJson {
    /// Build a connected `(sender, receiver, handle)` triple.
    #[allow(clippy::new_ret_no_self)] // A factory for the connected triple.
    pub fn new() -> (MockSender, MockReceiver, MockHandle) {
        let shared = Arc::new(MockShared {
            state: Mutex::new(MockState {
                events: VecDeque::new(),
                sent: Vec::new(),
                responder: None,
                execute_answer: None,
                next_client_id: 1,
            }),
            event_ready: Condvar::new(),
        });
        (
            MockSender {
                shared: Arc::clone(&shared),
            },
            MockReceiver {
                shared: Arc::clone(&shared),
            },
            MockHandle { shared },
        )
    }
}

/// The mock's [`TdSendApi`] half.
pub struct MockSender {
    shared: Arc<MockShared>,
}

impl std::fmt::Debug for MockSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockSender").finish_non_exhaustive()
    }
}

impl TdSendApi for MockSender {
    fn create_client_id(&self) -> i32 {
        let mut state = self.shared.lock();
        let id = state.next_client_id;
        state.next_client_id += 1;
        id
    }

    fn send(&self, client_id: i32, request: &str) {
        let mut state = self.shared.lock();
        let sent = SentRequest {
            client_id,
            json: request.to_owned(),
        };
        // The responder runs synchronously under the lock: its answers are
        // on the stream before `send` returns, so tests never race their
        // own scripting.
        if let Some(mut responder) = state.responder.take() {
            let events = responder(&sent);
            state.responder = Some(responder);
            state.events.extend(events);
        }
        state.sent.push(sent);
        drop(state);
        self.shared.event_ready.notify_all();
    }

    fn execute(&self, _request: &str) -> Option<String> {
        self.shared.lock().execute_answer.clone()
    }
}

/// The mock's single-owner [`TdReceiveApi`] half.
pub struct MockReceiver {
    shared: Arc<MockShared>,
}

impl std::fmt::Debug for MockReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockReceiver").finish_non_exhaustive()
    }
}

impl TdReceiveApi for MockReceiver {
    fn receive(&mut self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.shared.lock();
        loop {
            if let Some(event) = state.events.pop_front() {
                return Some(event);
            }
            let now = Instant::now();
            let remaining = match deadline {
                None => Duration::MAX,
                Some(deadline) => {
                    if now >= deadline {
                        return None;
                    }
                    deadline - now
                }
            };
            let (guard, _timed_out) = self
                .shared
                .event_ready
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner);
            state = guard;
        }
    }
}

/// The test-side control surface.
pub struct MockHandle {
    shared: Arc<MockShared>,
}

impl std::fmt::Debug for MockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockHandle").finish_non_exhaustive()
    }
}

impl MockHandle {
    /// Put one raw event on the receive stream.
    pub fn push_event(&self, event: &str) {
        {
            let mut state = self.shared.lock();
            state.events.push_back(event.to_owned());
        }
        self.shared.event_ready.notify_all();
    }

    /// Install the send hook (module docs). Replaces any previous hook.
    pub fn set_responder(
        &self,
        responder: impl FnMut(&SentRequest) -> Vec<String> + Send + 'static,
    ) {
        self.shared.lock().responder = Some(Box::new(responder));
    }

    /// Set the canned `td_execute` answer; `None` models no answer.
    pub fn set_execute_answer(&self, answer: Option<&str>) {
        self.shared.lock().execute_answer = answer.map(str::to_owned);
    }

    /// Drain the record of sent requests, in send order.
    pub fn take_sent(&self) -> Vec<SentRequest> {
        std::mem::take(&mut self.shared.lock().sent)
    }
}
