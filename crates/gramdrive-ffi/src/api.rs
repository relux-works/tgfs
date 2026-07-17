//! The UniFFI-exposed contract: everything Swift and Kotlin can see.
//!
//! Every type here is provider-neutral (DEC-003): no Telegram/TDLib/gotd
//! types, no OS-native types — paths are strings, times are integer
//! milliseconds. The surface grows as engine capabilities land; the rules
//! for growing it without breaking native consumers are in README.md
//! (§ Versioning policy).

use std::sync::Arc;

/// The version of this FFI contract, independent of crate versions.
///
/// Native consumers pin against this; bump rules are in README.md
/// (§ Versioning policy).
pub const CONTRACT_VERSION: ContractVersion = ContractVersion {
    major: 0,
    minor: 1,
    patch: 0,
};

/// Semantic version of the FFI contract (not of any crate or artifact).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ContractVersion {
    /// Incremented on breaking changes (removal, rename, semantic change).
    pub major: u32,
    /// Incremented on additive changes (new operations, records, fields
    /// with defaults, error variants).
    pub minor: u32,
    /// Incremented on behavior-preserving fixes.
    pub patch: u32,
}

/// Returns the version of the FFI contract this library implements.
#[uniffi::export]
pub fn contract_version() -> ContractVersion {
    CONTRACT_VERSION
}

/// The boundary error: every fallible exported operation fails with one of
/// these categories and nothing else.
///
/// Categories are the stable, contractual part (NFR-030): native hosts
/// branch on them to pick an actionable user state. `detail` is diagnostic
/// text for logs — human-readable, redacted, and never contractual; do not
/// parse it. (The field is deliberately not named `message`: that name
/// collides with `kotlin.Exception.message` and produces uncompilable
/// Kotlin bindings under uniffi 0.32.) Mapping core-internal errors into a
/// category is owned by the code that raises them; this boundary never lets
/// an uncategorized error escape (UniFFI converts a panic into a generic
/// binding-level exception, which native hosts must treat as `Internal`).
///
/// Adding a variant is an additive (minor) contract change; native `switch`
/// statements over this enum must keep a default arm. Removing or renaming
/// a variant is breaking (major).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum DriveError {
    /// The caller passed an argument the contract forbids. Retrying the
    /// identical call cannot succeed.
    InvalidArgument {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The referenced item, version, or cursor does not exist (or no longer
    /// exists) in the drive.
    NotFound {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The source has no usable authorization; the user must (re)authorize
    /// in the host app before the operation can succeed.
    AuthRequired {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The source is throttling (e.g. a flood wait). Honor `retry_after_ms`
    /// when present; retry loops are bounded on the Rust side (NFR-033) and
    /// callers must not add tight retry loops of their own.
    RateLimited {
        /// Diagnostic detail; not contractual.
        detail: String,
        /// Source-provided minimum backoff, when it supplied one.
        retry_after_ms: Option<u64>,
    },
    /// The source cannot be reached right now (network down, backend
    /// unavailable). Retrying later with backoff is reasonable.
    SourceUnavailable {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// Local persistence failed (state database or cache storage).
    Storage {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// Fetched or cached content failed an integrity check; partial or
    /// wrong content is never published (NFR-012).
    Integrity {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// The operation was cancelled — via its [`CancellationToken`] or from
    /// the Rust side (e.g. core shutdown). See README.md (§ Cancellation).
    Cancelled {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
    /// A bug: an invariant the core promised to hold did not. Not
    /// actionable by the user; report and log.
    Internal {
        /// Diagnostic detail; not contractual.
        detail: String,
    },
}

impl std::fmt::Display for DriveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArgument { detail } => write!(f, "invalid argument: {detail}"),
            Self::NotFound { detail } => write!(f, "not found: {detail}"),
            Self::AuthRequired { detail } => write!(f, "authorization required: {detail}"),
            Self::RateLimited {
                detail,
                retry_after_ms,
            } => match retry_after_ms {
                Some(ms) => write!(f, "rate limited (retry after {ms} ms): {detail}"),
                None => write!(f, "rate limited: {detail}"),
            },
            Self::SourceUnavailable { detail } => write!(f, "source unavailable: {detail}"),
            Self::Storage { detail } => write!(f, "storage failure: {detail}"),
            Self::Integrity { detail } => write!(f, "integrity failure: {detail}"),
            Self::Cancelled { detail } => write!(f, "cancelled: {detail}"),
            Self::Internal { detail } => write!(f, "internal error: {detail}"),
        }
    }
}

impl std::error::Error for DriveError {}

/// A point-in-time progress report for one running operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct TransferProgress {
    /// Bytes completed so far; monotonically non-decreasing within one
    /// operation.
    pub bytes_transferred: u64,
    /// Total expected bytes, when known up front. `None` means the total is
    /// not yet known (e.g. a streaming source).
    pub bytes_total: Option<u64>,
}

/// Foreign-implemented progress sink for long-running operations.
///
/// Dispatch rules (README.md § Callback dispatch): calls arrive
/// synchronously on a background thread owned by the operation — never on a
/// platform main thread by contract. Implementations must be thread-safe,
/// must return quickly without blocking (they run inside the operation's
/// execution path, NFR-025), and must not throw — a thrown exception cannot
/// cross this boundary meaningfully and is a programming error in the host.
#[uniffi::export(with_foreign)]
pub trait ProgressListener: Send + Sync {
    /// Receives one progress snapshot. Called after each completed chunk;
    /// the final call reports the fully transferred size.
    fn on_progress(&self, progress: TransferProgress);
}

/// Explicit, in-band cancellation for long-running operations.
///
/// Cancellation is part of this contract rather than delegated to the
/// binding runtime, because the platform provider APIs deliver cancellation
/// as handles/callbacks (`NSProgress` cancellation handlers, Android
/// `CancellationSignal`) and because generated-binding task cancellation is
/// not dependable across languages (uniffi 0.32 Swift does not propagate
/// `Task` cancellation at all — README.md § Cancellation). The host creates
/// a token, passes it to the operation, and may call [`cancel`] from any
/// thread at any time; the operation then fails with
/// [`DriveError::Cancelled`] at its next cancellation point (NFR-025).
/// Tokens are single-use: one token per operation, and a token cancelled
/// before the call cancels it immediately.
///
/// [`cancel`]: CancellationToken::cancel
#[derive(Debug, uniffi::Object)]
pub struct CancellationToken {
    // watch, not Notify: a cancel that happens before the operation starts
    // waiting must still be observed, and watch keeps that state.
    cancelled: tokio::sync::watch::Sender<bool>,
}

#[uniffi::export]
impl CancellationToken {
    /// Creates a token in the not-cancelled state.
    #[uniffi::constructor]
    #[allow(clippy::new_without_default)] // constructed via FFI, not Default
    pub fn new() -> Arc<Self> {
        let (cancelled, _) = tokio::sync::watch::channel(false);
        Arc::new(Self { cancelled })
    }

    /// Requests cancellation. Idempotent, thread-safe, never blocks.
    pub fn cancel(&self) {
        self.cancelled.send_replace(true);
    }

    /// Whether [`CancellationToken::cancel`] has been called.
    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }
}

impl CancellationToken {
    /// Resolves once the token is cancelled; immediately if it already is.
    /// Not exported: operations await this internally at cancellation
    /// points.
    pub(crate) async fn cancelled(&self) {
        let mut rx = self.cancelled.subscribe();
        // The sender lives in `self`, so `wait_for` cannot see a closed
        // channel and the Result is always Ok.
        let _ = rx.wait_for(|cancelled| *cancelled).await;
    }
}

/// Configuration for constructing a [`DriveCore`].
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CoreConfig {
    /// Directory for the core's durable state (metadata database, cache).
    /// A plain string, not an OS path type, by boundary rule; the host
    /// passes a directory it owns (e.g. an App Group container path). Must
    /// be non-empty.
    pub data_dir: String,
}

/// Handle to one drive core instance — the root object native hosts hold.
///
/// Real drive operations (enumeration, hydration, pinning) attach here as
/// the engine lands (STORY-260715-2hs8cf); the handle, its constructor
/// validation, and [`DriveCore::probe_transfer`] establish the lifecycle,
/// error, progress, and cancellation contract those operations will reuse.
#[derive(Debug, uniffi::Object)]
pub struct DriveCore {
    config: CoreConfig,
}

#[uniffi::export(async_runtime = "tokio")]
impl DriveCore {
    /// Creates a core instance for the given configuration.
    ///
    /// Fails with [`DriveError::InvalidArgument`] when the configuration is
    /// unusable. Validation is intentionally shallow until the state store
    /// lands (STORY-260715-16ik2x): the directory string must be non-empty.
    #[uniffi::constructor]
    pub fn new(config: CoreConfig) -> Result<Arc<Self>, DriveError> {
        if config.data_dir.is_empty() {
            return Err(DriveError::InvalidArgument {
                detail: "data_dir must be a non-empty directory path".to_owned(),
            });
        }
        Ok(Arc::new(Self { config }))
    }

    /// The data directory this core was constructed with.
    pub fn data_dir(&self) -> String {
        self.config.data_dir.clone()
    }

    /// Boundary conformance probe: a synthetic transfer that exercises the
    /// full async/progress/cancellation path without a Telegram account.
    ///
    /// Native hosts use it in integration smoke tests to prove that async
    /// dispatch, progress callbacks, structured errors, and cancellation
    /// round-trip through the generated bindings; it stays in the contract
    /// because those guarantees must remain executable on every platform.
    ///
    /// Reports progress after each `chunk_bytes` slice of `total_bytes`,
    /// pausing `chunk_delay_ms` before each slice (every pause is a
    /// cancellation point), and returns the total bytes "transferred".
    /// Fails with [`DriveError::InvalidArgument`] when `chunk_bytes` is 0,
    /// and with [`DriveError::Cancelled`] once `token` is cancelled — after
    /// which no further progress callbacks arrive.
    pub async fn probe_transfer(
        &self,
        total_bytes: u64,
        chunk_bytes: u64,
        chunk_delay_ms: u64,
        listener: Arc<dyn ProgressListener>,
        token: Arc<CancellationToken>,
    ) -> Result<u64, DriveError> {
        if chunk_bytes == 0 {
            return Err(DriveError::InvalidArgument {
                detail: "chunk_bytes must be greater than 0".to_owned(),
            });
        }
        let mut transferred: u64 = 0;
        while transferred < total_bytes {
            tokio::select! {
                () = token.cancelled() => {
                    return Err(DriveError::Cancelled {
                        detail: "cancelled by caller".to_owned(),
                    });
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(chunk_delay_ms)) => {}
            }
            transferred = transferred.saturating_add(chunk_bytes).min(total_bytes);
            listener.on_progress(TransferProgress {
                bytes_transferred: transferred,
                bytes_total: Some(total_bytes),
            });
        }
        Ok(transferred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct RecordingListener {
        events: Mutex<Vec<TransferProgress>>,
    }

    impl ProgressListener for RecordingListener {
        fn on_progress(&self, progress: TransferProgress) {
            self.events.lock().unwrap().push(progress);
        }
    }

    impl RecordingListener {
        fn events(&self) -> Vec<TransferProgress> {
            self.events.lock().unwrap().clone()
        }
    }

    fn core() -> Arc<DriveCore> {
        DriveCore::new(CoreConfig {
            data_dir: "/tmp/gramdrive-test".to_owned(),
        })
        .expect("valid config")
    }

    #[test]
    fn contract_version_is_exposed() {
        assert_eq!(contract_version(), CONTRACT_VERSION);
    }

    #[test]
    fn constructor_rejects_empty_data_dir() {
        let err = DriveCore::new(CoreConfig {
            data_dir: String::new(),
        })
        .expect_err("empty data_dir must be rejected");
        assert!(matches!(err, DriveError::InvalidArgument { .. }));
    }

    #[test]
    fn constructor_keeps_config() {
        assert_eq!(core().data_dir(), "/tmp/gramdrive-test");
    }

    #[test]
    fn error_display_names_category_and_detail() {
        let err = DriveError::RateLimited {
            detail: "flood wait".to_owned(),
            retry_after_ms: Some(1500),
        };
        assert_eq!(
            err.to_string(),
            "rate limited (retry after 1500 ms): flood wait"
        );
        let err = DriveError::Integrity {
            detail: "hash mismatch".to_owned(),
        };
        assert_eq!(err.to_string(), "integrity failure: hash mismatch");
    }

    #[test]
    fn token_starts_clear_and_cancel_is_idempotent() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        token.cancel();
        assert!(token.is_cancelled());
    }

    // start_paused: tokio's test clock auto-advances timers, so chunk delays
    // cost no wall time and orderings are deterministic.
    #[tokio::test(start_paused = true)]
    async fn probe_reports_each_chunk_and_returns_total() {
        let listener = Arc::new(RecordingListener::default());
        let transferred = core()
            .probe_transfer(100, 40, 10, listener.clone(), CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert_eq!(transferred, 100);
        let done: Vec<u64> = listener
            .events()
            .iter()
            .map(|p| p.bytes_transferred)
            .collect();
        assert_eq!(done, vec![40, 80, 100]);
        assert!(listener.events().iter().all(|p| p.bytes_total == Some(100)));
    }

    #[tokio::test(start_paused = true)]
    async fn probe_with_zero_total_completes_without_callbacks() {
        let listener = Arc::new(RecordingListener::default());
        let transferred = core()
            .probe_transfer(0, 8, 10, listener.clone(), CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert_eq!(transferred, 0);
        assert!(listener.events().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn probe_rejects_zero_chunk() {
        let listener = Arc::new(RecordingListener::default());
        let err = core()
            .probe_transfer(100, 0, 10, listener.clone(), CancellationToken::new())
            .await
            .expect_err("zero chunk must be rejected");
        assert!(matches!(err, DriveError::InvalidArgument { .. }));
        assert!(listener.events().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn probe_saturates_oversized_chunks_at_total() {
        let listener = Arc::new(RecordingListener::default());
        let transferred = core()
            .probe_transfer(10, u64::MAX, 10, listener.clone(), CancellationToken::new())
            .await
            .expect("probe succeeds");
        assert_eq!(transferred, 10);
        let done: Vec<u64> = listener
            .events()
            .iter()
            .map(|p| p.bytes_transferred)
            .collect();
        assert_eq!(done, vec![10]);
    }

    #[tokio::test(start_paused = true)]
    async fn probe_with_pre_cancelled_token_fails_before_any_progress() {
        let listener = Arc::new(RecordingListener::default());
        let token = CancellationToken::new();
        token.cancel();
        let err = core()
            .probe_transfer(1_000, 10, 1_000, listener.clone(), token)
            .await
            .expect_err("pre-cancelled token must cancel the probe");
        assert!(matches!(err, DriveError::Cancelled { .. }));
        assert!(listener.events().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn token_cancellation_fails_probe_and_stops_progress() {
        let listener = Arc::new(RecordingListener::default());
        let token = CancellationToken::new();
        let core = core();
        let probe = core.probe_transfer(1_000, 10, 1_000, listener.clone(), token.clone());
        tokio::pin!(probe);
        tokio::select! {
            _ = &mut probe => panic!("probe must not finish before cancellation"),
            () = tokio::time::sleep(std::time::Duration::from_millis(2_500)) => token.cancel(),
        }
        let err = probe.await.expect_err("cancelled probe must fail");
        assert!(matches!(err, DriveError::Cancelled { .. }));
        let seen = listener.events().len();
        assert_eq!(seen, 2, "two 1000 ms chunks fit into 2500 ms");
        tokio::time::sleep(std::time::Duration::from_millis(5_000)).await;
        assert_eq!(
            listener.events().len(),
            seen,
            "no callbacks may arrive after cancellation"
        );
    }

    // Dropping the future at an await point is what a Kotlin coroutine
    // cancellation does to the Rust side (the binding frees the wrapped
    // future); after the drop no further callbacks may arrive even without
    // a token.
    #[tokio::test(start_paused = true)]
    async fn dropping_the_future_stops_progress() {
        let listener = Arc::new(RecordingListener::default());
        let core = core();
        {
            let probe =
                core.probe_transfer(1_000, 10, 1_000, listener.clone(), CancellationToken::new());
            tokio::pin!(probe);
            tokio::select! {
                _ = &mut probe => panic!("probe must not finish before being dropped"),
                () = tokio::time::sleep(std::time::Duration::from_millis(2_500)) => {}
            }
            // `probe` dropped here.
        }
        let seen = listener.events().len();
        assert_eq!(seen, 2, "two 1000 ms chunks fit into 2500 ms");
        tokio::time::sleep(std::time::Duration::from_millis(5_000)).await;
        assert_eq!(
            listener.events().len(),
            seen,
            "no callbacks may arrive after the future is dropped"
        );
    }
}
