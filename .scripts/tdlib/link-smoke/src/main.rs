//! Prove the staged tdjson artifact links and runs, and read its version.
//!
//! This is the acceptance test for `build_tdlib.py`: if the artifact is missing
//! the library, a header, or a symbol, this binary fails to link or load here
//! rather than in the real tdjson wrapper (gramdrive-source-tdjson) months
//! later. It does two things:
//!
//!   1. Drives TDLib's modern C JSON interface end to end -- `td_execute` to
//!      silence logging, `td_create_client_id` to make a client, `td_send` to
//!      ask for the `version` option, and `td_receive` to read the answer --
//!      and prints `TDLib version: <v>`. The version comes out of the running
//!      library, never a number parsed from source, so a claimed version cannot
//!      outrun what the bytes actually implement. `getOption "version"` is
//!      answered before authorization, so this needs no api_id and no network.
//!
//!   2. Resolves the deprecated single-client symbols (`td_json_client_create`,
//!      `td_json_client_destroy`) by taking their addresses. TDLib forbids
//!      *mixing* the two client interfaces in one process, so these are proven
//!      to link without being called against the live modern scheduler -- the
//!      point of a link smoke test is that every symbol a consumer might use is
//!      present in the dylib.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_double, c_int, c_void};
use std::time::{Duration, Instant};

// The C JSON interface (td/telegram/td_json_client.h).
extern "C" {
    fn td_create_client_id() -> c_int;
    fn td_send(client_id: c_int, request: *const c_char);
    fn td_receive(timeout: c_double) -> *const c_char;
    fn td_execute(request: *const c_char) -> *const c_char;

    // Deprecated single-client interface -- referenced for link proof only.
    fn td_json_client_create() -> *mut c_void;
    fn td_json_client_destroy(client: *mut c_void);
}

/// Send a request string to a client. TDLib copies it, so the CString is free
/// to drop afterwards.
fn send(client_id: c_int, request: &str) {
    let c = CString::new(request).expect("request has no interior NUL");
    // SAFETY: `c` outlives the call; td_send copies the bytes it needs.
    unsafe { td_send(client_id, c.as_ptr()) };
}

/// Run a synchronous static request through `td_execute`. The returned pointer
/// is owned by TDLib and only valid until the next call on this thread, so the
/// result is copied into an owned String immediately.
fn execute(request: &str) -> Option<String> {
    let c = CString::new(request).expect("request has no interior NUL");
    // SAFETY: `c` outlives the call; the result is copied before any further
    // TDLib call could invalidate it.
    let ptr = unsafe { td_execute(c.as_ptr()) };
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}

/// Receive the next event, waiting up to `timeout` seconds. The returned
/// pointer is TDLib-owned and copied out immediately, same as `execute`.
fn receive(timeout: f64) -> Option<String> {
    // SAFETY: result is copied before the next TDLib call.
    let ptr = unsafe { td_receive(timeout as c_double) };
    if ptr.is_null() {
        return None;
    }
    Some(unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned())
}

/// Pull the string value out of the `getOption "version"` response, which is
/// `{"@type":"optionValueString","value":"1.8.x",...,"@extra":"version-probe"}`.
/// Matching on the probe's `@extra` avoids the nested `"value":{...}` of the
/// `updateOption` event that carries the same option.
fn version_from_response(message: &str) -> Option<String> {
    if !message.contains(PROBE_EXTRA) {
        return None;
    }
    let start = message.find("\"value\":\"")? + "\"value\":\"".len();
    let rest = &message[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

const PROBE_EXTRA: &str = "version-probe";

fn main() {
    // Prove the deprecated symbols resolve without calling them against the
    // modern scheduler (see module docs). Referencing the addresses forces the
    // linker to keep them.
    let legacy_create: unsafe extern "C" fn() -> *mut c_void = td_json_client_create;
    let legacy_destroy: unsafe extern "C" fn(*mut c_void) = td_json_client_destroy;
    println!(
        "linked deprecated symbols: td_json_client_create @ {:p}, td_json_client_destroy @ {:p}",
        legacy_create as *const (), legacy_destroy as *const ()
    );

    // Quiet TDLib's own logging to keep the smoke output readable. This also
    // exercises td_execute against a static request.
    let _ = execute(r#"{"@type":"setLogVerbosityLevel","new_verbosity_level":1}"#);

    // SAFETY: no arguments, no aliasing; returns a plain client id.
    let client_id = unsafe { td_create_client_id() };
    println!("created client id {client_id} via td_create_client_id");

    // The client's thread does not start until the first request is sent.
    send(
        client_id,
        r#"{"@type":"getOption","name":"version","@extra":"version-probe"}"#,
    );

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let Some(message) = receive(1.0) else { continue };
        if let Some(version) = version_from_response(&message) {
            println!("TDLib version: {version}");
            // This probe never authorizes the client. Asking that pre-auth
            // client to close can abort inside TDLib even though the runtime
            // linkage and version proof already succeeded. `process::exit`
            // terminates the probe and its private TDLib threads together.
            std::process::exit(0);
        }
    }

    eprintln!("did not receive a version response within the timeout");
    std::process::exit(1);
}
