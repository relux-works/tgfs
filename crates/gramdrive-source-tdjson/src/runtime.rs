//! The tdjson runtime: one receive owner, correlated requests, bounded
//! update dispatch, coordinated shutdown.
//!
//! # Shape
//!
//! [`TdRuntime::start`] takes the two halves of a tdjson implementation
//! ([`TdSendApi`]/[`TdReceiveApi`]) and moves the receive half into the one
//! receive-loop thread — the single owner `td_receive` demands. Everything
//! else hangs off shared state: [`TdClient`] handles submit requests from
//! any thread, the loop resolves them.
//!
//! # Correlation
//!
//! The runtime injects a minted JSON number as `@extra` into every request
//! (a request already carrying `@extra` is rejected — correlation ids
//! belong to the runtime alone) and resolves the matching
//! [`PendingRequest`] when the response comes back. An `{"@type":"error"}`
//! response resolves it to the typed [`TdError::Td`]. A response whose id
//! is no longer pending — cancelled, or a duplicate — is discarded and
//! counted, never misdelivered.
//!
//! # Cancellation
//!
//! Dropping a [`PendingRequest`] (or calling [`PendingRequest::cancel`])
//! removes the correlation entry; the eventual response is discarded and
//! counted in [`RuntimeStats::discarded_responses`]. tdjson has no wire
//! cancellation for ordinary requests, so cancelled work still completes
//! inside TDLib — what the wrapper guarantees is that its answer can never
//! reach, block, or leak into a caller that gave up on it.
//!
//! # Updates and backpressure
//!
//! Updates (events without `@extra`) route by `@client_id` into a bounded
//! per-client queue ([`UpdateStream`]). A full queue blocks the receive
//! loop — deliberate backpressure: TDLib's update order is part of its
//! contract, so dropping mid-stream is not an option, and bounded memory
//! beats unbounded growth. The block is shutdown-aware (a closing queue
//! wakes it) and consumer-aware (a dropped stream disconnects it; further
//! updates for that client are counted, not delivered). One slow consumer
//! therefore stalls the shared loop — acceptable for the v1 single-account
//! shape and stated here so nobody discovers it in production.
//!
//! # Shutdown and drain
//!
//! [`TdRuntime::shutdown`] (also run on drop) is deterministic and cannot
//! deadlock: set the flag, close every update queue (waking a loop blocked
//! on backpressure), join the loop — which first drains every event tdjson
//! already has ready (zero-timeout receives), completing their pending
//! requests — then fail whatever is still pending with
//! [`TdError::Shutdown`]. Updates buffered in a stream stay readable after
//! shutdown; the stream then reports closed. Updates arriving during the
//! drain are delivered only if their queue has room — the drain never
//! blocks — and are counted as dropped otherwise.
//!
//! # Client close
//!
//! A client that reports `authorizationStateClosed` is closed at the
//! runtime level after that update is delivered: its pending requests fail
//! with [`TdError::ClientClosed`], its stream ends, and new requests on it
//! are rejected. The modern tdjson interface has no `td_json_client_destroy`
//! — TDLib frees a client's resources after the closed state — so close
//! *is* destroy at this layer; the deprecated per-client interface is
//! linked (proved by the tdlib link smoke) but never called, since TDLib
//! forbids mixing the two interfaces in one process.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::Value;

use crate::api::{TdReceiveApi, TdSendApi};
use crate::envelope::{self, Event};
use crate::error::TdError;
use crate::queue::{Queue, RecvError};
use crate::slot::Slot;

/// Tuning knobs for a [`TdRuntime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeConfig {
    /// How long one `receive` call waits when the stream is idle. This is
    /// poll granularity, not latency — events wake the call early — and it
    /// bounds how long shutdown waits for the loop to notice the flag.
    pub receive_timeout: Duration,
    /// Per-client update queue capacity. A full queue backpressures the
    /// receive loop (module docs); capacity 0 is raised to 1.
    pub update_queue_capacity: usize,
}

impl Default for RuntimeConfig {
    fn default() -> RuntimeConfig {
        RuntimeConfig {
            receive_timeout: Duration::from_millis(500),
            update_queue_capacity: 1024,
        }
    }
}

/// Monotonic counters for events the runtime absorbed rather than
/// delivered. Deterministic tests assert on these; production reads them
/// as health data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuntimeStats {
    /// Responses whose request was no longer pending (cancelled or
    /// duplicate `@extra`).
    pub discarded_responses: u64,
    /// Updates lost to a disconnected consumer, a closed client's queue,
    /// or a full queue during the shutdown drain.
    pub dropped_updates: u64,
    /// Updates for a `@client_id` this runtime never registered.
    pub unroutable_updates: u64,
    /// Events the envelope rules could not classify.
    pub malformed_events: u64,
}

#[derive(Default)]
struct StatCounters {
    discarded_responses: AtomicU64,
    dropped_updates: AtomicU64,
    unroutable_updates: AtomicU64,
    malformed_events: AtomicU64,
}

struct PendingEntry {
    slot: Arc<Slot<Result<Value, TdError>>>,
    client_id: i32,
}

struct ClientEntry {
    updates: Arc<Queue<Value>>,
    closed: bool,
}

struct State {
    pending: HashMap<u64, PendingEntry>,
    clients: HashMap<i32, ClientEntry>,
}

struct Shared {
    sender: Box<dyn TdSendApi>,
    state: Mutex<State>,
    next_request: AtomicU64,
    shutdown: AtomicBool,
    config: RuntimeConfig,
    stats: StatCounters,
}

impl Shared {
    // Poison recovery as in `Slot`/`Queue`: the maps stay valid at every
    // unlock point, and shutdown must keep working after a panic elsewhere.
    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The safe tdjson runtime; one per process for the real implementation
/// (the receive stream is process-global), any number over mocks.
pub struct TdRuntime {
    shared: Arc<Shared>,
    receive_loop: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for TdRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TdRuntime")
            .field("shutdown", &self.shared.shutdown.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl TdRuntime {
    /// Start the runtime: spawn the receive loop and take ownership of the
    /// receive half. Fails only if the loop thread cannot be spawned.
    pub fn start(
        sender: impl TdSendApi + 'static,
        receiver: impl TdReceiveApi + 'static,
        config: RuntimeConfig,
    ) -> Result<TdRuntime, TdError> {
        let shared = Arc::new(Shared {
            sender: Box::new(sender),
            state: Mutex::new(State {
                pending: HashMap::new(),
                clients: HashMap::new(),
            }),
            next_request: AtomicU64::new(1),
            shutdown: AtomicBool::new(false),
            config,
            stats: StatCounters::default(),
        });
        let loop_shared = Arc::clone(&shared);
        let mut receiver = receiver;
        let receive_loop = std::thread::Builder::new()
            .name("gramdrive-tdjson-receive".to_owned())
            .spawn(move || receive_loop(&loop_shared, &mut receiver))
            .map_err(|err| TdError::Protocol {
                detail: format!("failed to spawn the receive loop: {err}"),
            })?;
        Ok(TdRuntime {
            shared,
            receive_loop: Some(receive_loop),
        })
    }

    /// Register a new client. The pair is the client's request handle and
    /// its ordered update stream; TDLib starts the client's own thread on
    /// its first request, not here.
    pub fn create_client(&self) -> Result<(TdClient, UpdateStream), TdError> {
        let queue = Arc::new(Queue::new(self.shared.config.update_queue_capacity));
        let client_id = self.shared.sender.create_client_id();
        {
            let mut state = self.shared.lock_state();
            if self.shared.shutdown.load(Ordering::SeqCst) {
                return Err(TdError::Shutdown);
            }
            state.clients.insert(
                client_id,
                ClientEntry {
                    updates: Arc::clone(&queue),
                    closed: false,
                },
            );
        }
        Ok((
            TdClient {
                client_id,
                shared: Arc::clone(&self.shared),
            },
            UpdateStream { queue },
        ))
    }

    /// Run a synchronous static request (`td_execute`), with the same typed
    /// error conversion responses get.
    pub fn execute(&self, request: &Value) -> Result<Value, TdError> {
        if self.shared.shutdown.load(Ordering::SeqCst) {
            return Err(TdError::Shutdown);
        }
        let json = serde_json::to_string(request).map_err(|err| TdError::InvalidRequest {
            detail: format!("unserializable request: {err}"),
        })?;
        let Some(raw) = self.shared.sender.execute(&json) else {
            return Err(TdError::Protocol {
                detail: "td_execute returned no answer".to_owned(),
            });
        };
        let value: Value = serde_json::from_str(&raw).map_err(|err| TdError::Protocol {
            detail: format!("unparseable td_execute answer: {err}"),
        })?;
        if value.get("@type").and_then(Value::as_str) == Some("error") {
            return Err(TdError::from_error_object(&value));
        }
        Ok(value)
    }

    /// A snapshot of the absorbed-event counters.
    pub fn stats(&self) -> RuntimeStats {
        let stats = &self.shared.stats;
        RuntimeStats {
            discarded_responses: stats.discarded_responses.load(Ordering::SeqCst),
            dropped_updates: stats.dropped_updates.load(Ordering::SeqCst),
            unroutable_updates: stats.unroutable_updates.load(Ordering::SeqCst),
            malformed_events: stats.malformed_events.load(Ordering::SeqCst),
        }
    }

    /// Shut the runtime down: drain what tdjson already delivered, fail the
    /// rest with [`TdError::Shutdown`], stop and join the receive loop.
    /// Sequencing in the module docs; also runs on drop, and is idempotent.
    pub fn shutdown(mut self) {
        self.shutdown_impl();
    }

    fn shutdown_impl(&mut self) {
        let Some(receive_loop) = self.receive_loop.take() else {
            return;
        };
        // Flag under the state lock: any `request` that got past the flag
        // check holds an entry the final drain below will see.
        let queues: Vec<Arc<Queue<Value>>> = {
            let state = self.shared.lock_state();
            self.shared.shutdown.store(true, Ordering::SeqCst);
            state
                .clients
                .values()
                .map(|client| Arc::clone(&client.updates))
                .collect()
        };
        // Closing the queues wakes a loop blocked on backpressure — the
        // join below would otherwise wait forever on it.
        for queue in &queues {
            queue.close();
        }
        // The loop observes the flag within one receive timeout, drains
        // ready events, and exits. A panic in the loop is a bug, but must
        // not turn shutdown into a second panic: the pending drain below
        // still runs.
        let _ = receive_loop.join();
        let slots: Vec<Arc<Slot<Result<Value, TdError>>>> = {
            let mut state = self.shared.lock_state();
            state.pending.drain().map(|(_, entry)| entry.slot).collect()
        };
        for slot in slots {
            let _ = slot.complete(Err(TdError::Shutdown));
        }
    }
}

impl Drop for TdRuntime {
    fn drop(&mut self) {
        self.shutdown_impl();
    }
}

/// A request handle for one tdjson client. Cheap to clone; all clones speak
/// for the same client.
#[derive(Clone)]
pub struct TdClient {
    client_id: i32,
    shared: Arc<Shared>,
}

impl std::fmt::Debug for TdClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TdClient")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl TdClient {
    /// The tdjson client id this handle speaks for.
    pub fn client_id(&self) -> i32 {
        self.client_id
    }

    /// Submit `request` — a JSON object without `@extra` — and return the
    /// handle its response resolves. Rejected without touching tdjson when
    /// the request is malformed, the client is closed, or the runtime is
    /// shut down.
    pub fn request(&self, mut request: Value) -> Result<PendingRequest, TdError> {
        let Some(object) = request.as_object_mut() else {
            return Err(TdError::InvalidRequest {
                detail: "request must be a JSON object".to_owned(),
            });
        };
        if object.contains_key("@extra") {
            return Err(TdError::InvalidRequest {
                detail: "@extra is the runtime's correlation member; requests must not set it"
                    .to_owned(),
            });
        }
        let id = self.shared.next_request.fetch_add(1, Ordering::SeqCst);
        object.insert("@extra".to_owned(), Value::from(id));
        let json = serde_json::to_string(&request).map_err(|err| TdError::InvalidRequest {
            detail: format!("unserializable request: {err}"),
        })?;

        let slot = Arc::new(Slot::new());
        {
            let mut state = self.shared.lock_state();
            if self.shared.shutdown.load(Ordering::SeqCst) {
                return Err(TdError::Shutdown);
            }
            match state.clients.get(&self.client_id) {
                None => {
                    return Err(TdError::Protocol {
                        detail: "client is not registered with this runtime".to_owned(),
                    });
                }
                Some(entry) if entry.closed => return Err(TdError::ClientClosed),
                Some(_) => {}
            }
            state.pending.insert(
                id,
                PendingEntry {
                    slot: Arc::clone(&slot),
                    client_id: self.client_id,
                },
            );
        }
        self.shared.sender.send(self.client_id, &json);
        Ok(PendingRequest {
            id,
            slot,
            shared: Arc::clone(&self.shared),
            taken: false,
        })
    }

    /// Ask TDLib to close this client (`{"@type":"close"}`). The runtime
    /// marks the client closed when the resulting
    /// `authorizationStateClosed` update arrives — that update, not this
    /// response, is the end of the client's lifecycle.
    pub fn close(&self) -> Result<PendingRequest, TdError> {
        self.request(serde_json::json!({"@type": "close"}))
    }
}

/// A submitted request awaiting its response.
///
/// Consume it one of three ways: [`wait_timeout`](Self::wait_timeout)
/// (bounded blocking), `.await` (it is a `Future`), or
/// [`cancel`](Self::cancel)/drop — which removes the correlation entry so
/// the eventual response is discarded and counted instead of delivered.
pub struct PendingRequest {
    id: u64,
    slot: Arc<Slot<Result<Value, TdError>>>,
    shared: Arc<Shared>,
    taken: bool,
}

impl std::fmt::Debug for PendingRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingRequest")
            .field("id", &self.id)
            .field("taken", &self.taken)
            .finish_non_exhaustive()
    }
}

impl PendingRequest {
    /// The correlation id the runtime injected as `@extra`.
    pub fn request_id(&self) -> u64 {
        self.id
    }

    /// Block for the response at most `timeout`. On timeout the handle
    /// comes back in `Err` — the request is still pending and can be
    /// waited again, awaited, or cancelled.
    #[allow(clippy::result_large_err)] // Err *is* the handle, by design.
    pub fn wait_timeout(
        mut self,
        timeout: Duration,
    ) -> Result<Result<Value, TdError>, PendingRequest> {
        match self.slot.wait_timeout(timeout) {
            Some(result) => {
                self.taken = true;
                Ok(result)
            }
            None => Err(self),
        }
    }

    /// Cancel the request: the correlation entry is removed now, the
    /// eventual response is discarded and counted. Equivalent to dropping
    /// the handle, spelled out.
    pub fn cancel(self) {}
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if self.taken {
            return;
        }
        // Cancellation point: after this removal the response can only be
        // counted as discarded, never delivered.
        let entry = {
            let mut state = self.shared.lock_state();
            state.pending.remove(&self.id)
        };
        drop(entry);
    }
}

impl Future for PendingRequest {
    type Output = Result<Value, TdError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match this.slot.poll_take(cx) {
            Poll::Ready(Some(result)) => {
                this.taken = true;
                Poll::Ready(result)
            }
            // The value was already taken — polling a finished future.
            Poll::Ready(None) => {
                this.taken = true;
                Poll::Ready(Err(TdError::Protocol {
                    detail: "response was already taken from this handle".to_owned(),
                }))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Why an [`UpdateStream`] receive returned no update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateRecvError {
    /// Nothing arrived within the wait; the stream is still live.
    Timeout,
    /// The stream is over — client closed or runtime shut down — and every
    /// buffered update was already taken.
    Closed,
}

impl std::fmt::Display for UpdateRecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateRecvError::Timeout => write!(f, "no update within the wait"),
            UpdateRecvError::Closed => write!(f, "update stream is closed"),
        }
    }
}

impl std::error::Error for UpdateRecvError {}

/// One client's updates, in tdjson delivery order. Dropping the stream
/// disconnects it: later updates for the client are counted as dropped
/// rather than delivered, and a receive loop blocked on this queue's
/// backpressure is released.
pub struct UpdateStream {
    queue: Arc<Queue<Value>>,
}

impl std::fmt::Debug for UpdateStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateStream").finish_non_exhaustive()
    }
}

impl UpdateStream {
    /// The next update, waiting at most `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Value, UpdateRecvError> {
        self.queue.recv_timeout(timeout).map_err(|err| match err {
            RecvError::Timeout => UpdateRecvError::Timeout,
            RecvError::Closed => UpdateRecvError::Closed,
        })
    }

    /// The next update, without waiting.
    pub fn try_recv(&self) -> Result<Value, UpdateRecvError> {
        self.queue.try_recv().map_err(|err| match err {
            RecvError::Timeout => UpdateRecvError::Timeout,
            RecvError::Closed => UpdateRecvError::Closed,
        })
    }
}

impl Drop for UpdateStream {
    fn drop(&mut self) {
        self.queue.disconnect_receiver();
    }
}

// ---------------------------------------------------------------------------
// The receive loop
// ---------------------------------------------------------------------------

fn receive_loop(shared: &Arc<Shared>, receiver: &mut dyn TdReceiveApi) {
    let timeout = shared.config.receive_timeout;
    loop {
        let event = receiver.receive(timeout);
        if shared.shutdown.load(Ordering::SeqCst) {
            // Drain phase: everything tdjson already has ready is processed
            // with zero-timeout receives and non-blocking update delivery,
            // so pending requests whose answers arrived resolve normally
            // and nothing can stall the exit.
            if let Some(raw) = event {
                process_event(shared, &raw, DeliveryMode::Drain);
            }
            while let Some(raw) = receiver.receive(Duration::ZERO) {
                process_event(shared, &raw, DeliveryMode::Drain);
            }
            return;
        }
        if let Some(raw) = event {
            process_event(shared, &raw, DeliveryMode::Blocking);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeliveryMode {
    /// Normal operation: a full update queue backpressures the loop.
    Blocking,
    /// Shutdown drain: never block; a full queue drops and counts.
    Drain,
}

fn process_event(shared: &Arc<Shared>, raw: &str, mode: DeliveryMode) {
    match envelope::classify(raw) {
        Event::Malformed { detail: _detail } => {
            shared.stats.malformed_events.fetch_add(1, Ordering::SeqCst);
        }
        Event::Response { extra, payload } => {
            let entry = {
                let mut state = shared.lock_state();
                state.pending.remove(&extra)
            };
            // Completion happens outside the state lock (slot wakes a
            // waker; waker code may re-enter the runtime).
            let delivered = match entry {
                Some(entry) => entry.slot.complete(payload),
                None => false,
            };
            if !delivered {
                shared
                    .stats
                    .discarded_responses
                    .fetch_add(1, Ordering::SeqCst);
            }
        }
        Event::Update { client_id, payload } => {
            let closes_client = update_closes_client(&payload);
            let queue = {
                let state = shared.lock_state();
                state
                    .clients
                    .get(&client_id)
                    .map(|client| Arc::clone(&client.updates))
            };
            let Some(queue) = queue else {
                shared
                    .stats
                    .unroutable_updates
                    .fetch_add(1, Ordering::SeqCst);
                return;
            };
            // Delivery happens outside the state lock: a blocking push must
            // never hold the lock shutdown and request submission need.
            let delivery = match mode {
                DeliveryMode::Blocking => queue.push_blocking(payload),
                DeliveryMode::Drain => queue.try_push(payload),
            };
            if delivery.is_err() {
                shared.stats.dropped_updates.fetch_add(1, Ordering::SeqCst);
            }
            // The closed update is delivered first (the consumer sees the
            // terminal state), then the client's lifecycle ends.
            if closes_client {
                close_client(shared, client_id);
            }
        }
    }
}

fn update_closes_client(payload: &Value) -> bool {
    payload.get("@type").and_then(Value::as_str) == Some("updateAuthorizationState")
        && payload
            .get("authorization_state")
            .and_then(|state| state.get("@type"))
            .and_then(Value::as_str)
            == Some("authorizationStateClosed")
}

fn close_client(shared: &Arc<Shared>, client_id: i32) {
    let (queue, slots) = {
        let mut state = shared.lock_state();
        let Some(entry) = state.clients.get_mut(&client_id) else {
            return;
        };
        if entry.closed {
            return;
        }
        entry.closed = true;
        let queue = Arc::clone(&entry.updates);
        let slots: Vec<Arc<Slot<Result<Value, TdError>>>> = state
            .pending
            .extract_if(|_, pending| pending.client_id == client_id)
            .map(|(_, pending)| pending.slot)
            .collect();
        (queue, slots)
    };
    queue.close();
    for slot in slots {
        let _ = slot.complete(Err(TdError::ClientClosed));
    }
}
