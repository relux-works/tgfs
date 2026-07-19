import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport

// `NSProgress` is documented thread-safe ("The NSProgress class is
// thread-safe"); the SDK just has not annotated it. The fetch task updates
// a progress the system concurrently observes, which is exactly the
// object's documented use.
extension Progress: @retroactive @unchecked Sendable {}

/// Tunables of the extension's fetch path.
struct ContentFetcherConfiguration: Sendable {
    /// Concurrent hydration bound (NFR-021): fetches beyond it wait in
    /// FIFO order, still cancellable while queued. Kept below the agent's
    /// own bound so the extension never trips the server's `busy` refusal.
    var maxConcurrentFetches: Int = 4
    /// How many times a fetch restarts against a fresh metadata snapshot
    /// after a mid-fetch version conflict before failing safely.
    var maxVersionRestarts: Int = 1
}

/// The extension's content-fetch orchestration (TASK-260715-kkglhx;
/// PLAT-MAC-004, SYNC-040..046 as seen from the provider):
/// `fetchContents` minus the untestable `NSFileProviderRequest` plumbing.
///
/// What this type owns, in order per fetch:
///
/// - **Refusals before any IPC.** A directory answers `featureUnsupported`;
///   POL-4 content (restricted, or gone at the source) answers
///   `fileReadNoPermission` — the item's capability surface already
///   advertises no read, so a fetch is a client ignoring capabilities, and
///   the agent is never contacted for bytes the engine will never fetch.
/// - **The bounded gate.** At most `maxConcurrentFetches` hydrations run;
///   the rest wait, cancellable while waiting.
/// - **The version pin and its race.** The fetch pins the content version
///   the metadata snapshot shows (DOM-003). A requested version that is no
///   longer current is served as the *current* version instead — Telegram
///   keeps no history, and returning the fresh item with the bytes is the
///   provider API's documented fallback. A conflict *during* the fetch
///   (the agent's `versionConflict`, or staged bytes reporting a different
///   token) restarts once against a fresh snapshot; a snapshot that has
///   not moved — or a second conflict — fails safely with
///   `serverUnreachable`, and stale bytes are never published (SYNC-042).
/// - **Atomic materialization** (PRD-043). The staged file the agent
///   reports is cloned into the extension's scratch directory (an APFS
///   clone — no byte loop, memory bounded by construction) and verified
///   against the reported byte count; only a complete, verified file is
///   handed to the system, and a failed copy is deleted, never returned.
/// - **Progress and cancellation.** The returned `Progress` counts bytes
///   and cancels the fetch: while queued, while hydrating (tearing the
///   agent connection down, which the agent observes as its cancel), and
///   through `cancelAll()` on invalidation.
/// - **Error mapping** onto the provider surface (NFR-030): each wire
///   category maps to the `NSFileProviderError`/`CocoaError` the system
///   acts on — see `providerError(for:)`.
final class ContentFetcher: @unchecked Sendable {
    /// What a fetch resolves per call: the account and the store handle —
    /// a fresh snapshot per fetch, like every other extension callback.
    typealias Context = (account: AccountInfo, store: any SharedStateStoreProtocol)
    typealias Completion = @Sendable (URL?, (any NSFileProviderItem)?, (any Error)?) -> Void

    private let hydration: any HydrationRequesting
    private let scratchDirectory: @Sendable () throws -> URL
    private let configuration: ContentFetcherConfiguration
    private let gate: FetchGate
    private let lock = NSLock()
    private var tasks: [UUID: Task<Void, Never>] = [:]

    init(
        hydration: any HydrationRequesting,
        scratchDirectory: @escaping @Sendable () throws -> URL,
        configuration: ContentFetcherConfiguration = ContentFetcherConfiguration()
    ) {
        self.hydration = hydration
        self.scratchDirectory = scratchDirectory
        self.configuration = configuration
        self.gate = FetchGate(width: configuration.maxConcurrentFetches)
    }

    /// The `fetchContents` callback's whole behavior. `context` resolves
    /// the domain's account per call and throws provider-mapped errors;
    /// `completionHandler` is called exactly once, from a background task.
    func fetchContents(
        itemIdentifier: NSFileProviderItemIdentifier,
        requestedVersion: NSFileProviderItemVersion?,
        context: @escaping @Sendable () throws -> Context,
        completionHandler: @escaping Completion
    ) -> Progress {
        let progress = Progress(totalUnitCount: -1)
        progress.kind = .file
        progress.setUserInfoObject(
            Progress.FileOperationKind.downloading, forKey: .fileOperationKindKey)
        progress.isCancellable = true

        // `requestedVersion` needs no branch and does not enter the fetch:
        // a pinned older version no longer exists at the source, so the
        // current version — which the fetch pins and delivers — is the
        // honest answer either way; the returned item carries the version
        // the bytes belong to.
        _ = requestedVersion

        let id = UUID()
        let task = Task { [weak self] in
            guard let self else {
                completionHandler(nil, nil, NSFileProviderError(.serverUnreachable))
                return
            }
            do {
                let (url, item) = try await self.performFetch(
                    itemIdentifier: itemIdentifier,
                    context: context,
                    progress: progress)
                completionHandler(url, item, nil)
            } catch is CancellationError {
                completionHandler(nil, nil, CocoaError(.userCancelled))
            } catch {
                completionHandler(nil, nil, error)
            }
            self.forget(id)
        }
        register(id, task)
        progress.cancellationHandler = { task.cancel() }
        return progress
    }

    /// Cancels every in-flight fetch — the invalidation path.
    func cancelAll() {
        lock.lock()
        let active = Array(tasks.values)
        lock.unlock()
        for task in active {
            task.cancel()
        }
    }

    /// In-flight fetch count, for tests.
    var inFlightCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return tasks.count
    }

    // MARK: - The fetch sequence

    private func performFetch(
        itemIdentifier: NSFileProviderItemIdentifier,
        context: @escaping @Sendable () throws -> Context,
        progress: Progress
    ) async throws -> (URL, any NSFileProviderItem) {
        try await gate.acquire()
        defer { gate.release() }
        try Task.checkCancellation()

        let (account, store) = try context()
        let coreItemId = ItemIdentifierMapping.coreItemId(
            for: itemIdentifier, accountRootId: account.rootItemId)
        var metadata = try liveFile(in: store, id: coreItemId)
        try Self.checkFetchable(metadata)

        var restarts = 0
        while true {
            Self.reset(progress, for: metadata)
            let pinned = metadata.contentVersion
            do {
                let content = try await hydration.hydrate(
                    HydrationRequest(
                        accountId: account.accountId,
                        itemId: metadata.id,
                        contentVersion: pinned),
                    onProgress: { [weak progress] update in
                        guard let progress else { return }
                        Self.apply(update, to: progress)
                    })
                if let pinned, let staged = content.contentVersion, staged != pinned {
                    // The agent staged some other version's bytes; treat it
                    // exactly like its own conflict answer.
                    throw HydrationFailure(
                        category: .versionConflict, detail: "staged version diverged")
                }
                let url = try materialize(content, progress: progress)
                let item = GramDriveFileProviderItem(
                    metadata: metadata, accountRootId: account.rootItemId)
                return (url, item)
            } catch let failure as HydrationFailure
                where failure.category == .versionConflict && restarts < configuration.maxVersionRestarts
            {
                try Task.checkCancellation()
                let fresh = try liveFile(in: store, id: coreItemId)
                guard fresh.contentVersion != metadata.contentVersion else {
                    // The store has not observed the new version yet;
                    // restarting now would pin the same stale token and
                    // spin. Fail transiently — the system retries after
                    // change enumeration delivers the new version.
                    throw NSFileProviderError(.serverUnreachable)
                }
                try Self.checkFetchable(fresh)
                metadata = fresh
                restarts += 1
            } catch let failure as HydrationFailure {
                throw Self.providerError(for: failure)
            } catch let transport as HydrationTransportError {
                _ = transport
                // Unreachable agent, a mid-stream silence, or a broken
                // frame: all transient service conditions the system
                // retries.
                throw NSFileProviderError(.serverUnreachable)
            } catch let socketError as UnixSocketError {
                _ = socketError
                // A raw socket fault the client surfaces when the channel
                // breaks below the protocol layer: fd exhaustion on
                // `socket()`, EPIPE as the agent dies mid-exchange,
                // EINTR/ECONNRESET on read, a sandbox-denied `connect`, or an
                // unrepresentable path. All transient transport conditions
                // (NFR-030) — the same class as HydrationTransportError, so
                // the same retryable answer. This catch is deliberately
                // scoped to the wire; the DriveError storage passthrough from
                // `liveFile` is thrown outside this `do` (or from within a
                // sibling catch) and is never folded in here.
                throw NSFileProviderError(.serverUnreachable)
            }
        }
    }

    /// One live *file*'s metadata, or the provider error the callback
    /// reports: `noSuchItem` for an unknown id, a POL-3 tombstone, or a
    /// system identifier that does not even parse as a core id; a transient
    /// storage failure passes through as-is (the system retries rather than
    /// caching a false absence).
    private func liveFile(in store: any SharedStateStoreProtocol, id: String) throws -> ItemMetadata {
        let metadata: ItemMetadata?
        do {
            metadata = try store.item(id: id)
        } catch let error as DriveError {
            if case .InvalidArgument = error {
                throw NSFileProviderError(.noSuchItem)
            }
            throw error
        }
        guard let metadata, metadata.deletedAtMs == nil else {
            throw NSFileProviderError(.noSuchItem)
        }
        return metadata
    }

    /// The refusals that never reach the agent: directories have no
    /// content to fetch, and POL-4 withholds bytes for restricted and
    /// gone content — mirroring the item's empty read capability.
    private static func checkFetchable(_ metadata: ItemMetadata) throws {
        if metadata.isDirectory {
            throw CocoaError(.featureUnsupported)
        }
        guard metadata.availability == .fetchable else {
            throw CocoaError(.fileReadNoPermission)
        }
    }

    // MARK: - Materialization (PRD-043: never publish partial content)

    /// Clones the staged file into the extension's scratch directory and
    /// verifies completeness; only then does the URL exist for the system.
    /// The staged source is engine-owned cache content and is never moved
    /// or modified.
    private func materialize(_ content: HydratedContent, progress: Progress) throws -> URL {
        let directory = try scratchDirectory()
        try? FileManager.default.createDirectory(
            at: directory, withIntermediateDirectories: true)
        let destination = directory.appendingPathComponent(
            "fetch-" + UUID().uuidString, isDirectory: false)
        do {
            try FileManager.default.copyItem(
                at: URL(fileURLWithPath: content.stagedPath, isDirectory: false),
                to: destination)
        } catch let error as CocoaError
            where error.code == .fileReadNoSuchFile || error.code == .fileNoSuchFile
        {
            // The staged file vanished between the agent's answer and the
            // clone (an eviction race): transient, retry later.
            throw NSFileProviderError(.serverUnreachable)
        } catch let error as CocoaError where error.code == .fileWriteOutOfSpace {
            throw NSFileProviderError(.insufficientQuota)
        } catch {
            throw NSFileProviderError(.cannotSynchronize)
        }

        let attributes = try? FileManager.default.attributesOfItem(atPath: destination.path)
        let size = (attributes?[.size] as? NSNumber)?.uint64Value
        guard size == content.byteCount else {
            try? FileManager.default.removeItem(at: destination)
            throw NSFileProviderError(.cannotSynchronize)
        }

        let total = Int64(clamping: content.byteCount)
        progress.totalUnitCount = max(total, 1)
        progress.completedUnitCount = progress.totalUnitCount
        return destination
    }

    // MARK: - Progress plumbing

    private static func reset(_ progress: Progress, for metadata: ItemMetadata) {
        progress.completedUnitCount = 0
        if let size = metadata.logicalSize, size > 0 {
            progress.totalUnitCount = Int64(clamping: size)
        } else {
            progress.totalUnitCount = -1
        }
    }

    private static func apply(_ update: HydrationProgress, to progress: Progress) {
        if let total = update.bytesTotal, total > 0 {
            progress.totalUnitCount = Int64(clamping: total)
        }
        let transferred = Int64(clamping: update.bytesTransferred)
        if progress.totalUnitCount > 0 {
            progress.completedUnitCount = min(transferred, progress.totalUnitCount)
        } else {
            progress.completedUnitCount = transferred
        }
    }

    // MARK: - Error mapping (NFR-030)

    /// Maps a terminal wire failure onto the error the system acts on.
    /// Transient service conditions (`rateLimited`, `sourceUnavailable`,
    /// `draining`, `busy`, and an unrestarted `versionConflict`) answer
    /// `serverUnreachable` — retry later; broken local machinery
    /// (`storage`, `integrity`, `internal`) answers `cannotSynchronize`;
    /// the rest have exact provider spellings.
    static func providerError(for failure: HydrationFailure) -> any Error {
        switch failure.category {
        case .notFound:
            return NSFileProviderError(.noSuchItem)
        case .restricted:
            return CocoaError(.fileReadNoPermission)
        case .authRequired:
            return NSFileProviderError(.notAuthenticated)
        case .cancelled:
            return CocoaError(.userCancelled)
        case .versionConflict, .rateLimited, .sourceUnavailable, .draining, .busy:
            return NSFileProviderError(.serverUnreachable)
        case .storage, .integrity, .internalError:
            return NSFileProviderError(.cannotSynchronize)
        }
    }

    // MARK: - Task bookkeeping

    private func register(_ id: UUID, _ task: Task<Void, Never>) {
        lock.lock()
        tasks[id] = task
        lock.unlock()
    }

    private func forget(_ id: UUID) {
        lock.lock()
        tasks.removeValue(forKey: id)
        lock.unlock()
    }
}

/// A FIFO counting gate for async work: at most `width` holders, waiters
/// queued in arrival order and individually cancellable while waiting.
final class FetchGate: @unchecked Sendable {
    private let lock = NSLock()
    private var available: Int
    private var waiters: [(id: UUID, continuation: CheckedContinuation<Void, any Error>)] = []
    /// Cancellations that raced ahead of their waiter being enqueued.
    private var cancelledBeforeWaiting: Set<UUID> = []

    init(width: Int) {
        available = max(1, width)
    }

    /// Acquires one slot, waiting FIFO when none is free. Throws
    /// `CancellationError` if the task is cancelled first — including a
    /// cancellation that lands before the wait is even registered.
    func acquire() async throws {
        let id = UUID()
        try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation {
                (continuation: CheckedContinuation<Void, any Error>) in
                lock.lock()
                if cancelledBeforeWaiting.remove(id) != nil {
                    lock.unlock()
                    continuation.resume(throwing: CancellationError())
                    return
                }
                if available > 0 {
                    available -= 1
                    lock.unlock()
                    continuation.resume()
                    return
                }
                waiters.append((id, continuation))
                lock.unlock()
            }
        } onCancel: {
            cancelWaiter(id)
        }
        // A cancel that fired in the sliver between the slot being granted
        // and the handler deregistering left a stray marker; sweep it.
        sweepStrayMarker(id)
    }

    // `NSLock` may not be taken from an async context; the post-await
    // cleanup lives in this sync helper.
    private func sweepStrayMarker(_ id: UUID) {
        lock.lock()
        cancelledBeforeWaiting.remove(id)
        lock.unlock()
    }

    func release() {
        lock.lock()
        if !waiters.isEmpty {
            let continuation = waiters.removeFirst().continuation
            lock.unlock()
            continuation.resume()
            return
        }
        available += 1
        lock.unlock()
    }

    private func cancelWaiter(_ id: UUID) {
        lock.lock()
        if let index = waiters.firstIndex(where: { $0.id == id }) {
            let continuation = waiters.remove(at: index).continuation
            lock.unlock()
            continuation.resume(throwing: CancellationError())
            return
        }
        cancelledBeforeWaiting.insert(id)
        lock.unlock()
    }
}
