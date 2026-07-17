//! GramDrive local TDLib source — the safe tdjson runtime
//! (EPIC-260715-2ptb18, STORY-260715-3elo6l; TASK-260715-2ulon7).
//!
//! This crate wraps TDLib's C JSON interface (`td_json_client.h`, the
//! modern `td_create_client_id`/`td_send`/`td_receive`/`td_execute`
//! surface) behind a safe, deterministic runtime:
//!
//! - [`api`] — the two-trait seam ([`TdSendApi`]/[`TdReceiveApi`]) between
//!   the runtime and any tdjson implementation. No C pointer crosses it:
//!   implementations copy every C string into an owned `String` before
//!   returning, and the receive half takes `&mut self` so the one-receiver
//!   rule of `td_receive` is enforced by ownership, not discipline.
//! - [`runtime`] — [`TdRuntime`]: the single receive-loop owner, request-id
//!   correlation over `@extra`, per-client bounded update queues, typed
//!   error conversion, cancellation, and coordinated shutdown with a
//!   deterministic drain.
//! - [`error`] — [`TdError`], the typed conversion of tdjson
//!   `{"@type":"error"}` objects plus the runtime's own failure states.
//! - [`config`] — [`TdlibConfig`] and the storage/memory policy: the
//!   per-account `setTdlibParameters`/`setOption`/`addProxy` request
//!   sequence, per-account on-disk isolation with a clean logout wipe, and
//!   the [`SecretSource`] seam to platform secure storage. Secrets are
//!   redacted from every `Debug`/log form (TASK-260715-1hdnuy).
//! - [`mock`] — [`MockTdJson`], the deterministic in-process tdjson double
//!   this crate's own tests run against, and the reason the crate compiles
//!   and tests without the TDLib artifact.
//! - `real` (compiled only under `cfg(real_tdjson)`, the env gate in
//!   `build.rs`) — the FFI implementation over the staged
//!   `libtdjson.dylib`, exercised by the `real_tdjson_smoke` integration
//!   test via `make tdjson-smoke`.
//!
//! The `DriveSource` adapter that maps this runtime onto the
//! provider-neutral contract (DEC-003) is deliberately absent here: it
//! lands with the follow-up tasks of this story (configuration,
//! authorization, enumeration). This layer's contract is only that tdjson's
//! asynchrony — one global receive stream multiplexing every client's
//! responses and updates — becomes safe, correlated, cancellable Rust.
//!
//! [`TdSendApi`]: api::TdSendApi
//! [`TdReceiveApi`]: api::TdReceiveApi
//! [`TdRuntime`]: runtime::TdRuntime
//! [`TdError`]: error::TdError
//! [`MockTdJson`]: mock::MockTdJson

// The only unsafe code is the FFI in `real`, which exists only when the env
// gate compiles it in; every other build forbids unsafe outright.
#![cfg_attr(not(real_tdjson), forbid(unsafe_code))]
#![cfg_attr(real_tdjson, deny(unsafe_code))]

pub mod api;
pub mod config;
pub mod error;
pub mod mock;
pub mod runtime;

mod envelope;
mod queue;
mod slot;

#[cfg(real_tdjson)]
#[allow(unsafe_code)]
pub mod real;

pub use config::{
    AccountConfig, AccountStoragePaths, ApiCredentials, DatabaseKey, DeviceMetadata,
    InMemorySecrets, MemoryOptions, Proxy, Secret, SecretError, SecretSource, StorageLayout,
    StoragePolicy, TdlibConfig,
};
pub use error::TdError;
pub use runtime::{
    PendingRequest, RuntimeConfig, RuntimeStats, TdClient, TdRuntime, UpdateRecvError, UpdateStream,
};
