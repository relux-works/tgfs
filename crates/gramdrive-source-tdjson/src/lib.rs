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
//! - [`auth`] — [`AuthMachine`], the deterministic sans-IO authorization
//!   state machine (TASK-260715-51n6jb): TDLib authorization updates and
//!   user inputs become core-facing typed states, requests, and rejection
//!   classifications; unknown TDLib states fail safe as typed
//!   `Unsupported`, never a panic.
//! - [`runtime`] — [`TdRuntime`]: the single receive-loop owner, request-id
//!   correlation over `@extra`, per-client bounded update queues, typed
//!   error conversion, cancellation, and coordinated shutdown with a
//!   deterministic drain.
//! - [`error`] — [`TdError`], the typed conversion of tdjson
//!   `{"@type":"error"}` objects plus the runtime's own failure states.
//! - [`config`] — [`TdlibConfig`] and the storage/memory policy: the
//!   per-account `setTdlibParameters`/`setOption`/`addProxy` request
//!   sequence, per-account on-disk isolation with a clean logout wipe, and
//!   the [`SecretSource`]/[`SecretStore`] seam to platform secure storage —
//!   the database-key lifecycle (creation, validated retrieval that fails
//!   closed, rotation, logout deletion; TASK-260715-2odowl). Secrets are
//!   redacted from every `Debug`/log form (TASK-260715-1hdnuy).
//! - [`removal`] — [`AccountRemoval`], the crash-resumable account-removal
//!   workflow (TASK-260715-wjaux5, SEC-004): the SEC-004 cleanup sequenced
//!   behind a durable journal, distinguishing Telegram logout
//!   ([`RemovalMode::RevokeSession`]) from local-only removal
//!   ([`RemovalMode::LocalOnly`]), idempotent and fail-safe under concurrent
//!   access. Owns the stages this crate can (session request, on-disk wipe,
//!   keychain revocation, journal); the engine/state stages it directs.
//! - [`snapshot`] — [`SnapshotMachine`], the deterministic sans-IO initial
//!   chat-list snapshot (TASK-260715-30amrq): `loadChats` pagination per
//!   list, the `getChats` order witness, lazy `getChat` detail resolution,
//!   flood-wait backoff advice, and resumable per-list commits carrying
//!   Telegram's exact ordering metadata — never a history or media request
//!   (SYNC-020). The composing caller persists each commit through the
//!   state repositories together with its resume token (SYNC-022).
//! - [`updates`] — [`UpdateMachine`], the deterministic sans-IO live
//!   chat-metadata/list update mapper (TASK-260715-1c8fea): TDLib's push
//!   updates (`updateChatTitle`/`updateChatPhoto`/`updateChatPosition`/
//!   `updateChatRemovedFromList`/`updateChatHasProtectedContent`, and the
//!   `updateUser`/`updateSupergroup` username feed) fold into the same
//!   provider-neutral normalized change stream the snapshot commits in, with
//!   POL-1 invalidation classification (reorder regenerates `order.json`, a
//!   rename renames the folder), idempotent under duplicate and out-of-order
//!   delivery, and gap reporting for unknown chats (SYNC-023).
//! - [`message`] — [`normalize_message`], the pure message normalizer
//!   (TASK-260715-1ynmct, PRD-022): one TDLib `message` object becomes one
//!   typed, provider-neutral [`MessageRecord`] — identity, time, sender,
//!   text/caption entities, reply, topic, album key, reactions, service
//!   actions, POL-4 protection facts, and attachment descriptors. Unknown
//!   content degrades explicitly ([`MessageContent::Unsupported`], raw JSON
//!   preserved under a schema version for migration), never a panic and
//!   never a silent drop; the history crawl (TASK-260715-26dnp6) and the
//!   ordered update loop (TASK-260715-10p5zp) are its composing callers.
//! - [`folders`] — [`FolderCatalogMachine`], the deterministic sans-IO folder
//!   (chat filter) catalog reducer (TASK-260715-54nopz): TDLib's
//!   `updateChatFolders` folds into a normalized folder create/rename/delete/
//!   reorder change stream with POL-1 invalidation classification, and yields
//!   the ordered folder set the snapshot enumerates. Folder membership stays
//!   the chat machines' `chat_list_entries` appearances, so a chat in several
//!   folders is one canonical record with one appearance per folder and a
//!   folder deletion removes only those appearances (SYNC-026, DOM-022).
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
//! lands with the enumeration follow-ups of the owning stories, composing
//! the configuration ([`config`]) and authorization ([`auth`]) layers that
//! are already here. This layer's contract is only that tdjson's
//! asynchrony — one global receive stream multiplexing every client's
//! responses and updates — becomes safe, correlated, cancellable Rust.
//!
//! [`TdSendApi`]: api::TdSendApi
//! [`TdReceiveApi`]: api::TdReceiveApi
//! [`TdRuntime`]: runtime::TdRuntime
//! [`TdError`]: error::TdError
//! [`MockTdJson`]: mock::MockTdJson
//! [`AuthMachine`]: auth::AuthMachine
//! [`normalize_message`]: message::normalize_message
//! [`MessageRecord`]: message::MessageRecord
//! [`MessageContent::Unsupported`]: message::MessageContent::Unsupported
//! [`SnapshotMachine`]: snapshot::SnapshotMachine
//! [`UpdateMachine`]: updates::UpdateMachine
//! [`FolderCatalogMachine`]: folders::FolderCatalogMachine
//! [`AccountRemoval`]: removal::AccountRemoval
//! [`RemovalMode::RevokeSession`]: removal::RemovalMode::RevokeSession
//! [`RemovalMode::LocalOnly`]: removal::RemovalMode::LocalOnly

// The only unsafe code is the FFI in `real`, which exists only when the env
// gate compiles it in; every other build forbids unsafe outright.
#![cfg_attr(not(real_tdjson), forbid(unsafe_code))]
#![cfg_attr(real_tdjson, deny(unsafe_code))]

pub mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod folders;
pub mod message;
pub mod mock;
pub mod removal;
pub mod runtime;
pub mod snapshot;
pub mod updates;

mod envelope;
mod queue;
mod slot;
mod wire;

#[cfg(real_tdjson)]
#[allow(unsafe_code)]
pub mod real;

pub use auth::{
    AuthError, AuthInput, AuthMachine, AuthRejection, AuthState, AuthStep, CodeInfo, PasswordInfo,
    RetryAdvice,
};
pub use config::{
    AccountConfig, AccountStoragePaths, ApiCredentials, DATABASE_KEY_LEN, DatabaseKey,
    DeviceMetadata, InMemorySecrets, MemoryOptions, Proxy, Secret, SecretError, SecretSource,
    SecretStore, StorageLayout, StoragePolicy, TdlibConfig, set_database_encryption_key_request,
};
pub use error::TdError;
pub use folders::{FolderCatalogBatch, FolderCatalogMachine, FolderDefinition, FolderInvalidation};
pub use message::{
    AttachmentAvailability, AttachmentDescriptor, AttachmentKind, ExpiredKind, FormattedText,
    MessageContent, MessageError, MessageRecord, ProtectionFacts, RAW_SCHEMA_VERSION, Reaction,
    ReactionKind, ReplyTarget, SelfDestruct, SenderRef, ServiceAction, TextEntity, TextEntityKind,
    TopicRef, UnsupportedContent, normalize_content, normalize_message, normalize_reactions,
};
pub use removal::{
    AccountRemoval, ExportPolicy, RemovalError, RemovalMode, RemovalRequest, RemovalStep,
};
pub use runtime::{
    PendingRequest, RuntimeConfig, RuntimeStats, TdClient, TdRuntime, UpdateRecvError, UpdateStream,
};
pub use snapshot::{
    ChatSnapshot, ListCommit, ListEntrySnapshot, SNAPSHOT_CURSOR_STREAM, SnapshotBackoff,
    SnapshotChatKind, SnapshotError, SnapshotMachine, SnapshotPlan, SnapshotRequest, SnapshotStep,
};
pub use updates::{ChatMetadata, Invalidation, MembershipChange, UpdateBatch, UpdateMachine};
