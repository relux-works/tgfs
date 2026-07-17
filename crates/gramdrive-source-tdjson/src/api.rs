//! The seam between the runtime and a tdjson implementation.
//!
//! TDLib's C JSON interface has two thread-safety regimes, and the split
//! into two traits is that fact made structural:
//!
//! - `td_send`, `td_execute` and `td_create_client_id` may be called from
//!   any thread concurrently — [`TdSendApi`] is `Send + Sync` and taken by
//!   shared reference.
//! - `td_receive` must never be called from two threads at once; there is
//!   exactly one receive stream per process. [`TdReceiveApi::receive`]
//!   takes `&mut self`, and the runtime moves the receiver into its one
//!   receive-loop thread, so a second caller is a compile error rather than
//!   undefined behavior.
//!
//! # Pointer ownership contract
//!
//! No C pointer crosses these traits. tdjson returns `const char*` values
//! that stay valid only until the next `td_receive`/`td_execute` call on
//! the same thread; an implementation must copy the bytes into an owned
//! `String` before returning (the `real` module does exactly that, and the
//! mock never has a C pointer to begin with). Requests are `&str` for the
//! duration of the call only — tdjson copies what it needs before
//! `td_send`/`td_execute` return.

use std::time::Duration;

/// The thread-safe half of a tdjson implementation: client creation,
/// request submission, and synchronous static requests.
pub trait TdSendApi: Send + Sync {
    /// Mint a fresh client identifier (`td_create_client_id`). The client's
    /// TDLib thread starts on its first request, not here.
    fn create_client_id(&self) -> i32;

    /// Submit `request` — a serialized JSON object — to `client_id`
    /// (`td_send`). Asynchronous: the answer arrives on the receive stream,
    /// correlated by the `@extra` member the runtime injected.
    fn send(&self, client_id: i32, request: &str);

    /// Run a synchronous static request (`td_execute`): only requests
    /// documented as executable without a client. `None` when tdjson
    /// returned no answer.
    fn execute(&self, request: &str) -> Option<String>;
}

/// The single-owner receive half of a tdjson implementation.
///
/// The runtime moves the receiver into its receive-loop thread and never
/// hands it back; `&mut self` makes concurrent receive calls unrepresentable.
pub trait TdReceiveApi: Send {
    /// The next event from the shared receive stream, waiting at most
    /// `timeout` (`td_receive`). `None` when the wait elapsed with nothing
    /// to deliver; `Duration::ZERO` is a non-blocking poll.
    fn receive(&mut self, timeout: Duration) -> Option<String>;
}
