import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport

/// Tunables of the extension's fetch path.
struct ContentFetcherConfiguration: Sendable {
  /// Concurrent hydration bound (NFR-021): fetches beyond it wait in
  /// FIFO order, still cancellable while queued. Kept below the agent's
  /// own bound so the extension never trips the server's `busy` refusal.
  var maxConcurrentFetches: Int = 4
  /// How many times an attachment fetch restarts against a fresh metadata
  /// snapshot after a mid-fetch version conflict before failing safely.
  /// Attachment hydration remains pinned to the byte generation Finder
  /// asked for, so further churn is retryable rather than followed.
  var maxVersionRestarts: Int = 1
  /// Generated documents are immutable cache-only publications. History
  /// backfill can advance their input watermark several times while one
  /// Finder read is in flight, so follow a small, bounded number of fresh
  /// atomic publications before returning a retryable failure. This budget
  /// deliberately does not relax attachment byte pinning.
  var maxGeneratedVersionRestarts: Int = 3
}

/// Carries the engine-side reason across provider error mapping. Only the
/// mapped error reaches macOS; telemetry retains this fixed, content-free
/// classification. Both content and thumbnail callbacks use it so durable
/// health distinguishes an engine refusal from the File Provider error we
/// return to macOS.
struct ObservedProviderFetchError: Error, @unchecked Sendable {
  let providerError: any Error
  let classification: ProviderFetchClassification
}

/// The extension's content-fetch orchestration (TASK-260715-kkglhx;
/// PLAT-MAC-004, SYNC-040..046 as seen from the provider):
/// `fetchContents` minus the untestable `NSFileProviderRequest` plumbing.
///
/// What this type owns, in order per fetch:
///
/// - **Refusals before any IPC.** A directory answers `featureUnsupported`;
///   POL-4 content (restricted, or gone at the source) answers its typed
///   permission/absence error. A fetchable attachment whose exact extent is
///   not projected yet answers retryable-unavailable: its item advertises no
///   read capability until completeness can be verified, and the extension
///   never starts an agent transfer that cannot be atomically published.
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
/// - **Foreground history demand** for the enclosing chat, raised while a
///   read is in flight — see `raiseChatDemand(accountId:chatId:)`.
final class ContentFetcher: @unchecked Sendable {
  /// What a fetch resolves per call: the account and the store handle —
  /// a fresh snapshot per fetch, like every other extension callback.
  typealias Context = (account: AccountInfo, store: any SharedStateStoreProtocol)
  typealias Completion = @Sendable (URL?, (any NSFileProviderItem)?, (any Error)?) -> Void

  /// One chat whose history a read is currently asking for.
  private struct ChatDemandKey: Hashable {
    let accountId: Int64
    let chatId: Int64
  }

  private let hydration: any HydrationRequesting
  private let scratchDirectory: @Sendable () throws -> URL
  private let configuration: ContentFetcherConfiguration
  /// The hint seam reads raise demand through; internal so the extension's
  /// wiring of it is assertable rather than only reachable through a live
  /// agent socket.
  let historyPriority: (any HistoryPrioritySignaling)?
  private let telemetry: any ProviderFetchObserving
  private let gate: FetchGate
  private let lock = NSLock()
  private var tasks: [UUID: Task<Void, Never>] = [:]
  /// Reads in flight per enclosing chat, so overlapping reads raise the
  /// hint once and release it once.
  private var chatDemand: [ChatDemandKey: Int] = [:]

  init(
    hydration: any HydrationRequesting,
    scratchDirectory: @escaping @Sendable () throws -> URL,
    configuration: ContentFetcherConfiguration = ContentFetcherConfiguration(),
    historyPriority: (any HistoryPrioritySignaling)? = nil,
    telemetry: (any ProviderFetchObserving)? = nil
  ) {
    self.hydration = hydration
    self.scratchDirectory = scratchDirectory
    self.configuration = configuration
    self.historyPriority = historyPriority
    self.telemetry = telemetry ?? ProviderFetchTelemetry()
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
    let startedAt = DispatchTime.now().uptimeNanoseconds
    let itemToken = ProviderFetchTelemetry.itemToken(for: itemIdentifier)
    // Register under the same lock that `forget` takes. The new task may
    // begin immediately, but it cannot remove itself until its dictionary
    // entry exists, so synchronous local refusals cannot retain a
    // completed task forever.
    lock.lock()
    let task = Task(priority: .userInitiated) { [weak self] in
      guard let self else {
        completionHandler(nil, nil, NSFileProviderError(.serverUnreachable))
        return
      }
      let outcome: (URL?, (any NSFileProviderItem)?, (any Error)?)
      let classification: ProviderFetchClassification
      do {
        let (url, item) = try await self.performFetch(
          itemIdentifier: itemIdentifier,
          context: context,
          progress: progress)
        outcome = (url, item, nil)
        classification = ProviderFetchTelemetry.classification(for: nil)
      } catch let observed as ObservedProviderFetchError {
        outcome = (nil, nil, observed.providerError)
        classification = observed.classification
      } catch is CancellationError {
        outcome = (nil, nil, CocoaError(.userCancelled))
        classification = ProviderFetchTelemetry.classification(for: outcome.2)
      } catch {
        outcome = (nil, nil, error)
        classification = ProviderFetchTelemetry.classification(for: error)
      }
      self.forget(id)
      let elapsedMs = (DispatchTime.now().uptimeNanoseconds - startedAt) / 1_000_000
      self.telemetry.record(
        ProviderFetchTelemetryRecord(
          callback: "fetchContents",
          itemToken: itemToken,
          outcome: classification.outcome,
          retryable: classification.retryable,
          elapsedMs: elapsedMs,
          engineFailure: classification.engineFailure,
          providerMapping: classification.providerMapping,
          noSuchItem: classification.noSuchItem))
      completionHandler(outcome.0, outcome.1, outcome.2)
    }
    tasks[id] = task
    lock.unlock()
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

    let (account, store) = try await FileProviderDemandExecutor.run(context)
    let coreItemId = ItemIdentifierMapping.coreItemId(
      for: itemIdentifier, accountRootId: account.rootItemId)
    var metadata = try await FileProviderDemandExecutor.run {
      try self.liveFile(in: store, id: coreItemId)
    }
    // Before the refusals: a POL-4 restricted attachment a user tried to
    // open is the same evidence about which chat they are in as one whose
    // bytes arrive.
    let demandChat = metadata.chatId
    if let demandChat {
      raiseChatDemand(accountId: account.accountId, chatId: demandChat)
    }
    defer {
      if let demandChat {
        releaseChatDemand(accountId: account.accountId, chatId: demandChat)
      }
    }
    try Self.checkFetchable(metadata)

    // Generated cache rows and item facts are committed together by the
    // render pipeline. It is therefore safe to follow the current
    // published generation through a bounded amount of history churn.
    // Attachments retain their single-restart pinned-byte contract.
    let maxVersionRestarts =
      metadata.kind == .generatedDoc
      ? configuration.maxGeneratedVersionRestarts
      : configuration.maxVersionRestarts
    var restarts = 0
    while true {
      Self.reset(progress, for: metadata)
      let pinned = metadata.contentVersion
      do {
        let scratchDirectory = self.scratchDirectory
        let url = try await hydration.hydrateAndMaterialize(
          HydrationRequest(
            accountId: account.accountId,
            itemId: metadata.id,
            contentVersion: pinned),
          onProgress: { [weak progress] update in
            guard let progress else { return }
            Self.apply(update, to: progress)
          },
          materialize: { content in
            if let pinned, let staged = content.contentVersion, staged != pinned {
              // The agent staged some other version's bytes; treat it
              // exactly like its own conflict answer.
              throw HydrationFailure(
                category: .versionConflict, detail: "staged version diverged")
            }
            return try Self.materialize(
              content, progress: progress, scratchDirectory: scratchDirectory)
          })
        let item = GramDriveFileProviderItem(
          metadata: metadata, accountRootId: account.rootItemId)
        return (url, item)
      } catch let failure as HydrationFailure
        where failure.category == .versionConflict && restarts < max(0, maxVersionRestarts)
      {
        try Task.checkCancellation()
        let fresh = try await FileProviderDemandExecutor.run {
          try self.liveFile(in: store, id: coreItemId)
        }
        guard fresh.contentVersion != metadata.contentVersion else {
          // The store has not observed the new version yet;
          // restarting now would pin the same stale token and
          // spin. Fail transiently — the system retries after
          // change enumeration delivers the new version.
          throw Self.observedProviderError(for: failure)
        }
        try Self.checkFetchable(fresh)
        metadata = fresh
        restarts += 1
      } catch let failure as HydrationFailure {
        throw Self.observedProviderError(for: failure)
      } catch let transport as HydrationTransportError {
        // Unreachable agent, a mid-stream silence, or a broken
        // frame: all transient service conditions the system
        // retries.
        throw ObservedProviderFetchError(
          providerError: NSFileProviderError(.serverUnreachable),
          classification: ProviderFetchTelemetry.classification(for: transport))
      } catch let socketError as UnixSocketError {
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
        throw ObservedProviderFetchError(
          providerError: NSFileProviderError(.serverUnreachable),
          classification: ProviderFetchTelemetry.classification(for: socketError))
      }
    }
  }

}

/// The File Provider's explicit foreground boundary for operations that may
/// synchronously enter SQLite, the filesystem, or a UniFFI binding.  These
/// operations are deliberately never performed by a Swift task: a suspended
/// caller yields its cooperative executor thread while GCD carries the actual
/// work at Finder-open priority.
enum FileProviderDemandExecutor {
  private static let queue = DispatchQueue(
    label: "com.reluxworks.gramdrive.fileprovider.demand",
    qos: .userInitiated,
    attributes: .concurrent)

  static func run<T: Sendable>(
    _ operation: @escaping @Sendable () throws -> T
  ) async throws -> T {
    try Task.checkCancellation()
    let value = try await withCheckedThrowingContinuation {
      (continuation: CheckedContinuation<T, Error>) in
      queue.async {
        continuation.resume(with: Result(catching: operation))
      }
    }
    try Task.checkCancellation()
    return value
  }
}

extension ContentFetcher {
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
  /// content to fetch. Restricted content is a durable policy refusal;
  /// unavailable content is still a live row whose source may recover, so
  /// it is transient rather than an absence assertion.
  private static func checkFetchable(_ metadata: ItemMetadata) throws {
    if metadata.isDirectory {
      throw CocoaError(.featureUnsupported)
    }
    switch metadata.availability {
    case .fetchable:
      guard metadata.logicalSize != nil else {
        // TDLib's expected_size is not an exact extent. Until the
        // durable projection learns an exact size, neither the
        // provider nor the engine can prove whole-content atomicity.
        throw NSFileProviderError(.serverUnreachable)
      }
    case .restricted:
      throw CocoaError(.fileReadNoPermission)
    case .unavailable:
      throw NSFileProviderError(.serverUnreachable)
    }
  }

  // MARK: - Foreground history demand (BUG-260728-2qfzbd)

  /// Raises `requested` history demand for the chat this read belongs to,
  /// for as long as the read is in flight.
  ///
  /// Reading content is the one interaction that *reliably* reaches this
  /// extension. Opening a chat folder does not: on a replicated domain macOS
  /// answers a read of an already-materialized directory out of its own copy
  /// of the namespace and no enumerator runs, so the enumerator's `visible`
  /// hint is never emitted — measured on the installed profile, where a
  /// plain `readdir` and a Finder window held open for 90 s both delivered
  /// zero hints. Dataless bytes, by contrast, can only be produced here. A
  /// folder-open-only interaction is therefore served by the agent's fair
  /// background rotation, and a content read is what moves the chat the user
  /// is actually in to the front of the queue.
  ///
  /// `requested`, not `visible`: the user asked for one file's bytes, which
  /// is a weaker claim than "this chat is on screen right now", and the
  /// weaker tier still runs ahead of the whole background backlog.
  ///
  /// The hint is released when the read settles, exactly like the
  /// enumerator's `visible`/`background` pair, so a chat cannot stay pinned
  /// to the foreground tier after the user has moved on. The release racing
  /// ahead of the agent's next scheduler boundary is expected and harmless:
  /// the agent admits the hint into a ledger and owes that chat a turn even
  /// when the release lands first.
  private func raiseChatDemand(accountId: Int64, chatId: Int64) {
    guard let historyPriority else { return }
    let key = ChatDemandKey(accountId: accountId, chatId: chatId)
    lock.lock()
    let held = chatDemand[key] ?? 0
    chatDemand[key] = held + 1
    lock.unlock()
    // Overlapping reads of one chat are one gesture; only the first raises.
    guard held == 0 else { return }
    historyPriority.signal(
      HistoryPriorityRequest(accountId: accountId, chatId: chatId, priority: .requested))
  }

  /// Releases the demand one read raised, once the last read of that chat
  /// has settled — completed, refused, or cancelled.
  private func releaseChatDemand(accountId: Int64, chatId: Int64) {
    guard let historyPriority else { return }
    let key = ChatDemandKey(accountId: accountId, chatId: chatId)
    lock.lock()
    let held = chatDemand[key] ?? 0
    if held > 1 {
      chatDemand[key] = held - 1
    } else {
      chatDemand.removeValue(forKey: key)
    }
    lock.unlock()
    guard held <= 1 else { return }
    historyPriority.signal(
      HistoryPriorityRequest(accountId: accountId, chatId: chatId, priority: .background))
  }

  /// Chats holding raised demand right now, for tests.
  var demandedChatCount: Int {
    lock.lock()
    defer { lock.unlock() }
    return chatDemand.count
  }

  // MARK: - Materialization (PRD-043: never publish partial content)

  /// Clones the staged file into the extension's scratch directory and
  /// verifies completeness; only then does the URL exist for the system.
  /// The staged source is engine-owned cache content and is never moved
  /// or modified.
  private static func materialize(
    _ content: HydratedContent,
    progress: Progress,
    scratchDirectory: @escaping @Sendable () throws -> URL
  ) throws -> URL {
    let directory = try scratchDirectory()
    try? FileManager.default.createDirectory(
      at: directory, withIntermediateDirectories: true)
    let destination = directory.appendingPathComponent(
      "fetch-" + UUID().uuidString, isDirectory: false)
    do {
      try content.cloneMaterializationSource(to: destination)
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
  /// Transient service conditions (`notFound` from a hydration source,
  /// `rateLimited`, `sourceUnavailable`, `draining`, `busy`, and an
  /// unrestarted `versionConflict`) answer
  /// `serverUnreachable` — retry later; broken local machinery
  /// (`storage`, `integrity`, `internal`) answers `cannotSynchronize`;
  /// the rest have exact provider spellings.
  static func providerError(for failure: HydrationFailure) -> any Error {
    switch failure.category {
    case .notFound:
      // A durable row was verified before this request crossed the
      // process boundary. A later source/renderer "not found" cannot
      // prove that the row was deleted; let Finder retry and let the
      // next durable lookup make any true absence decision.
      return NSFileProviderError(.serverUnreachable)
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

  static func observedProviderError(
    for failure: HydrationFailure
  ) -> ObservedProviderFetchError {
    ObservedProviderFetchError(
      providerError: providerError(for: failure),
      classification: ProviderFetchTelemetry.classification(for: failure))
  }

  // MARK: - Task bookkeeping

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
