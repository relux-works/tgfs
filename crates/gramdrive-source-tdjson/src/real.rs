//! The real tdjson implementation over the staged `libtdjson.dylib`.
//!
//! Compiled only under `cfg(real_tdjson)` — the env gate `build.rs` turns
//! on when `GRAMDRIVE_TDLIB_ARTIFACT_DIR` points at the artifact
//! `build_tdlib.py` staged. Exercised by the `real_tdjson_smoke`
//! integration test (`make tdjson-smoke`).
//!
//! # Why this is sound (the ownership justification)
//!
//! The crate's safety claim — no C pointer outlives its validity — holds
//! here by construction, not by audit of callers:
//!
//! - **Returned pointers never escape.** `td_receive` and `td_execute`
//!   return a `const char*` valid only until the next call of either
//!   function on the same thread. Both wrappers copy the bytes into an
//!   owned `String` *before returning* (`copy_c_string`), so by the time
//!   any caller sees data, no C pointer exists anymore. The [`TdSendApi`]/
//!   [`TdReceiveApi`] traits traffic exclusively in owned `String`s.
//! - **Passed pointers outlive their call.** Requests go through a
//!   [`CString`] local that lives across the `td_send`/`td_execute` call;
//!   tdjson documents that it copies the request before returning. The
//!   local drops after the call returns.
//! - **One receiver, enforced twice.** `td_receive` must never run
//!   concurrently. [`RealTdJson::claim`] hands out the process's single
//!   [`RealReceiver`] exactly once (an atomic claim), and
//!   `TdReceiveApi::receive` takes `&mut self`, so two concurrent calls
//!   would need two receivers that cannot exist.
//! - **`td_execute` result validity is per-thread.** TDLib stores the
//!   answer in thread-local storage, so a concurrent `td_receive` on the
//!   receive-loop thread cannot invalidate an execute answer being copied
//!   on another thread. The copy still happens before the wrapper returns,
//!   on the calling thread, before that thread could call either function
//!   again.
//! - **No teardown hazard.** The modern interface has no destroy function
//!   whose misuse could dangle a client pointer: clients are integer ids,
//!   closed via the `close` request, and TDLib reclaims them internally
//!   after `authorizationStateClosed`. The deprecated pointer-based
//!   interface is never called (mixing the two is forbidden by TDLib).
//!
//! Miri cannot execute foreign functions, so this justification plus the
//! runtime's mock-driven lifecycle tests stand in for it; the real-linkage
//! smoke runs the same runtime against the actual library.

use std::ffi::{CStr, CString, c_char, c_double, c_int};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::api::{TdReceiveApi, TdSendApi};

// The C JSON interface (td/telegram/td_json_client.h). Linkage comes from
// build.rs (`-l tdjson` plus the artifact's search path and rpath).
unsafe extern "C" {
    fn td_create_client_id() -> c_int;
    fn td_send(client_id: c_int, request: *const c_char);
    fn td_receive(timeout: c_double) -> *const c_char;
    fn td_execute(request: *const c_char) -> *const c_char;
}

/// Copy a tdjson-owned C string into an owned `String` immediately.
///
/// # Safety
///
/// `ptr` must be null or a valid NUL-terminated string that stays valid for
/// the duration of this call (tdjson guarantees validity until the *next*
/// `td_receive`/`td_execute` call on this thread, and this function makes
/// no such call).
unsafe fn copy_c_string(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: non-null per the check above; valid per this function's
    // contract. The bytes are copied into the owned String before return.
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

static RECEIVER_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Entry point to the real tdjson.
#[derive(Debug)]
pub struct RealTdJson;

impl RealTdJson {
    /// Claim the process's tdjson halves. The receive stream is global to
    /// the process, so the receiver exists exactly once: the first call
    /// gets it, every later call gets `None`.
    pub fn claim() -> Option<(RealSender, RealReceiver)> {
        if RECEIVER_CLAIMED.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some((RealSender, RealReceiver { _private: () }))
    }
}

/// The thread-safe send half over the real tdjson.
#[derive(Debug)]
pub struct RealSender;

impl TdSendApi for RealSender {
    fn create_client_id(&self) -> i32 {
        // SAFETY: no arguments, no pointers; returns a plain integer id.
        unsafe { td_create_client_id() }
    }

    fn send(&self, client_id: i32, request: &str) {
        // The runtime serializes requests itself, so a raw NUL cannot occur;
        // if one somehow does, dropping the request is the only non-panic
        // option and the pending request surfaces it as a timeout.
        let Ok(request) = CString::new(request) else {
            return;
        };
        // SAFETY: `request` outlives the call; tdjson copies the bytes it
        // needs before returning.
        unsafe { td_send(client_id, request.as_ptr()) };
    }

    fn execute(&self, request: &str) -> Option<String> {
        let request = CString::new(request).ok()?;
        // SAFETY: `request` outlives the call. The returned pointer is
        // copied before this thread can call td_receive/td_execute again
        // (validity is per-thread; module docs).
        let answer = unsafe { td_execute(request.as_ptr()) };
        // SAFETY: `answer` is null or valid until this thread's next tdjson
        // call, which cannot happen before copy_c_string returns.
        unsafe { copy_c_string(answer) }
    }
}

/// The single-owner receive half over the real tdjson. Constructible only
/// through [`RealTdJson::claim`], which hands it out once per process.
pub struct RealReceiver {
    _private: (),
}

impl std::fmt::Debug for RealReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealReceiver").finish_non_exhaustive()
    }
}

impl TdReceiveApi for RealReceiver {
    fn receive(&mut self, timeout: Duration) -> Option<String> {
        // SAFETY: `&mut self` on the process's only receiver means no
        // concurrent td_receive exists. The returned pointer is copied
        // before this (the only receiving) thread calls td_receive again.
        let event = unsafe { td_receive(timeout.as_secs_f64()) };
        // SAFETY: `event` is null or valid until this thread's next tdjson
        // call, which cannot happen before copy_c_string returns.
        unsafe { copy_c_string(event) }
    }
}
