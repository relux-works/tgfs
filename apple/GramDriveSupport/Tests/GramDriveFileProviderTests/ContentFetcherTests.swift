import Darwin
import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport
import Testing

@testable import GramDriveFileProvider

// MARK: - Fixtures

private let accountId: Int64 = 7
private let rootId = "acct-root"

private final class RecordingProviderFetchObserver: ProviderFetchObserving, @unchecked Sendable {
  private let lock = NSLock()
  private var values: [ProviderFetchTelemetryRecord] = []

  func record(_ record: ProviderFetchTelemetryRecord) {
    lock.lock()
    values.append(record)
    lock.unlock()
  }

  func snapshot() -> [ProviderFetchTelemetryRecord] {
    lock.lock()
    defer { lock.unlock() }
    return values
  }
}

private func makeAccount() -> AccountInfo {
  AccountInfo(
    accountId: accountId,
    sourceKind: .localTdlib,
    displayName: "Test Account",
    authState: "authorized",
    namespaceVersion: 1,
    displayTimezone: "UTC",
    rootItemId: rootId
  )
}

private func rootItem() -> ItemMetadata {
  ItemMetadata(
    contractVersion: 1,
    id: rootId, parent: nil, kind: .account, isDirectory: true,
    displayName: "Test Account", safeName: "Test Account", metadataVersion: "m1",
    mimeType: nil, logicalSize: nil, attachmentLogicalKind: nil,
    attachmentRepresentation: nil, attachmentFidelity: nil,
    attachmentSourceName: nil, attachmentExactSize: nil, contentVersion: nil,
    availability: .fetchable, createdAtMs: 1_000, modifiedAtMs: 1_000, deletedAtMs: nil
  )
}

private func file(
  id: String,
  size: UInt64? = 5,
  contentVersion: String? = "v1",
  availability: ItemAvailability = .fetchable,
  chatId: Int64? = nil
) -> ItemMetadata {
  ItemMetadata(
    contractVersion: 1,
    id: id, parent: rootId, kind: .attachment, isDirectory: false,
    displayName: id, safeName: id + ".bin", metadataVersion: "m1",
    mimeType: "application/octet-stream", logicalSize: size,
    attachmentLogicalKind: nil, attachmentRepresentation: nil,
    attachmentFidelity: nil, attachmentSourceName: nil,
    attachmentExactSize: size, contentVersion: contentVersion,
    availability: availability, createdAtMs: 1_000, modifiedAtMs: 1_000, deletedAtMs: nil,
    chatId: chatId
  )
}

private func generatedFile(
  id: String,
  name: String,
  mimeType: String,
  size: UInt64,
  contentVersion: String,
  chatId: Int64? = nil
) -> ItemMetadata {
  ItemMetadata(
    contractVersion: 1,
    id: id, parent: rootId, kind: .generatedDoc, isDirectory: false,
    displayName: name, safeName: name, metadataVersion: "generated-m1",
    mimeType: mimeType, logicalSize: size,
    attachmentLogicalKind: nil, attachmentRepresentation: nil,
    attachmentFidelity: nil, attachmentSourceName: nil,
    attachmentExactSize: nil, contentVersion: contentVersion,
    availability: .fetchable, createdAtMs: 1_000, modifiedAtMs: 2_000, deletedAtMs: nil,
    chatId: chatId
  )
}

private func directory(id: String) -> ItemMetadata {
  ItemMetadata(
    contractVersion: 1,
    id: id, parent: rootId, kind: .chat, isDirectory: true,
    displayName: id, safeName: id, metadataVersion: "m1",
    mimeType: nil, logicalSize: nil, attachmentLogicalKind: nil,
    attachmentRepresentation: nil, attachmentFidelity: nil,
    attachmentSourceName: nil, attachmentExactSize: nil, contentVersion: nil,
    availability: .fetchable, createdAtMs: 1_000, modifiedAtMs: 1_000, deletedAtMs: nil
  )
}

/// Failures that happen after durable admission has proven an attachment row
/// live. They are deliberately represented at the same wire boundary Finder
/// uses, so this matrix guards the Open callback against treating any one of
/// them as evidence of deletion.
private enum LiveOpenFault: CaseIterable {
  case timeout
  case transport
  case rendererSourceNotFound
  case sourceUnavailable

  func enqueue(on hydration: ScriptedHydration) {
    switch self {
    case .timeout:
      hydration.enqueueTransportFailure(.timedOut(path: "/agent.sock"))
    case .transport:
      hydration.enqueueSocketFailure(.failed(operation: "read", code: ECONNRESET))
    case .rendererSourceNotFound:
      hydration.enqueueFailure(
        HydrationFailure(category: .notFound, detail: "renderer lost its source object"))
    case .sourceUnavailable:
      hydration.enqueueFailure(
        HydrationFailure(category: .sourceUnavailable, detail: "source temporarily unavailable"))
    }
  }
}

/// A fetcher over a scripted store and scripted hydration; every test
/// starts here.
private struct Harness {
  let store: ScriptedStore
  let hydration: ScriptedHydration
  let historyPriority: RecordingHistoryPrioritySignaler
  let telemetry: RecordingProviderFetchObserver
  let fetcher: ContentFetcher

  init(
    scratch: URL,
    hydration: (any HydrationRequesting)? = nil,
    configuration: ContentFetcherConfiguration = ContentFetcherConfiguration()
  ) {
    let store = ScriptedStore(account: makeAccount())
    store.apply(rootItem())
    self.store = store
    self.hydration = ScriptedHydration()
    let requesting = hydration ?? self.hydration
    self.historyPriority = RecordingHistoryPrioritySignaler()
    self.telemetry = RecordingProviderFetchObserver()
    self.fetcher = ContentFetcher(
      hydration: requesting,
      scratchDirectory: { scratch },
      configuration: configuration,
      historyPriority: historyPriority,
      telemetry: telemetry)
  }

  /// Runs one fetch to completion; the returned outcome is the recorded
  /// completion call.
  func fetch(
    _ identifier: NSFileProviderItemIdentifier,
    version: NSFileProviderItemVersion? = nil
  ) async -> FetchOutcome {
    let future = TestFuture<FetchOutcome>()
    _ = start(identifier, version: version, future: future)
    return await future.settled
  }

  /// Starts one fetch without awaiting it.
  func start(
    _ identifier: NSFileProviderItemIdentifier,
    version: NSFileProviderItemVersion? = nil,
    future: TestFuture<FetchOutcome>
  ) -> Progress {
    fetcher.fetchContents(
      itemIdentifier: identifier,
      requestedVersion: version,
      context: { [store] in (account: makeAccount(), store: store) },
      completionHandler: { url, item, error in
        future.fulfill(FetchOutcome(url: url, item: item, error: error))
      })
  }
}

@Suite("Content fetch")
struct ContentFetcherTests {
  @Test("Fetch telemetry logs only a stable redacted token and fixed categories")
  func telemetryRedactsAllUserDerivedCallbackData() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      let rawIdentifier = "chat-name-Alice-telegram-123456789"
      let outcome = await harness.fetch(NSFileProviderItemIdentifier(rawIdentifier))
      outcome.expectProviderError(.noSuchItem)

      let records = harness.telemetry.snapshot()
      #expect(records.count == 1)
      let record = try #require(records.first)
      #expect(record.callback == "fetchContents")
      #expect(record.outcome == "no-such-item")
      #expect(record.noSuchItem)
      #expect(!record.retryable)
      #expect(record.itemToken.hasPrefix("fp-"))
      #expect(record.itemToken != rawIdentifier)
      #expect(!record.itemToken.contains("Alice"))
      #expect(!record.itemToken.contains("123456789"))
      #expect(record.elapsedMs < 60_000)

      #expect(
        ProviderFetchTelemetry.itemToken(
          for: NSFileProviderItemIdentifier(rawIdentifier)) == record.itemToken)
      #expect(
        ProviderFetchTelemetry.itemToken(
          for: NSFileProviderItemIdentifier(rawIdentifier + "-other"))
          != record.itemToken)

      let logMessage = ProviderFetchTelemetry.logMessage(for: record)
      let healthData = try JSONEncoder().encode(
        ProviderFetchTelemetry.healthReport(for: record, observedAtMs: 1_000))
      let healthJSON = try #require(String(data: healthData, encoding: .utf8))
      for forbidden in [
        rawIdentifier, "Alice", "123456789", "Test Account",
        "message body", "secret.txt", "telegram",
      ] {
        #expect(!logMessage.localizedCaseInsensitiveContains(forbidden))
        #expect(!healthJSON.localizedCaseInsensitiveContains(forbidden))
      }
    }
  }

  // MARK: Success path

  @Test("A fetch materializes verified staged content atomically")
  func successMaterializesVerifiedContent() async throws {
    try await withFetchScratchDirectory { scratch in
      let staging = scratch.appendingPathComponent("cache", isDirectory: true)
      try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      let stagedPath = try stageContent(Data("hello".utf8), in: staging)
      harness.hydration.enqueueSuccess(
        HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 5),
        progress: [
          HydrationProgress(bytesTransferred: 2, bytesTotal: 5),
          HydrationProgress(bytesTransferred: 5, bytesTotal: 5),
        ])

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      #expect(outcome.error == nil)
      let url = try #require(outcome.url)
      #expect(try Data(contentsOf: url) == Data("hello".utf8))
      // A copy in the extension's scratch, never the staged original.
      #expect(url.path != stagedPath)
      #expect(url.path.hasPrefix(scratch.path))
      #expect(FileManager.default.fileExists(atPath: stagedPath))
      // The returned item is the fetched snapshot.
      #expect(outcome.item?.itemIdentifier.rawValue == "f1")
      // The request pinned the observed content version.
      #expect(harness.hydration.requests.count == 1)
      #expect(harness.hydration.requests.first?.contentVersion == "v1")
      #expect(harness.hydration.requests.first?.accountId == accountId)
      #expect(harness.hydration.requests.first?.purpose == .content)
    }
  }

  @Test("Generated Markdown, NDJSON, and hidden chat JSON materialize exact staged bytes")
  func generatedDocumentsMaterializeExactBytes() async throws {
    try await withFetchScratchDirectory { scratch in
      let staging = scratch.appendingPathComponent("generated-cache", isDirectory: true)
      try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
      let harness = Harness(scratch: scratch)
      let documents: [(id: String, name: String, mime: String, bytes: Data)] = [
        ("generated-md", "Messages.md", "text/markdown", Data("# Messages\n".utf8)),
        (
          "generated-ndjson", "Messages.ndjson", "application/x-ndjson",
          Data("{\"schema\":\"gramdrive.messages\"}\n".utf8)
        ),
        (
          "generated-json", ".chat.json", "application/json",
          Data("{\"schema\":\"gramdrive.chat\"}\n".utf8)
        ),
      ]
      for document in documents {
        let version = document.id + "-v1"
        let metadata = generatedFile(
          id: document.id,
          name: document.name,
          mimeType: document.mime,
          size: UInt64(document.bytes.count),
          contentVersion: version)
        harness.store.apply(metadata)
        let stagedPath = try stageContent(document.bytes, in: staging)
        harness.hydration.enqueueSuccess(
          HydratedContent(
            stagedPath: stagedPath,
            contentVersion: version,
            byteCount: UInt64(document.bytes.count)))

        let outcome = await harness.fetch(NSFileProviderItemIdentifier(document.id))
        #expect(outcome.error == nil)
        let url = try #require(outcome.url)
        #expect(try Data(contentsOf: url) == document.bytes)
        let expectedType = GramDriveFileProviderItem(
          metadata: metadata, accountRootId: rootId
        ).contentType
        #expect(outcome.fetchedItem?.contentType == expectedType)
        #expect(
          outcome.fetchedItem?.documentSize?.uint64Value
            == UInt64(document.bytes.count))
        #expect(
          outcome.fetchedItem?.itemVersion.contentVersion == Data(version.utf8))
      }
    }
  }

  @Test("Twenty generated reads remain bounded and byte-exact while foreground demand is saturated")
  func saturatedGeneratedDocumentReadsRemainByteExact() async throws {
    try await withFetchScratchDirectory { scratch in
      var configuration = ContentFetcherConfiguration()
      configuration.maxConcurrentFetches = 3
      let harness = Harness(scratch: scratch, configuration: configuration)
      let started = ArrivalCounter()
      let release = ManualGate()
      let formats: [(String, String)] = [
        ("Messages.md", "text/markdown"),
        ("Messages.ndjson", "application/x-ndjson"),
        (".chat.json", "application/json"),
      ]
      var expectedBytes: [String: Data] = [:]
      var stagedContent: [String: HydratedContent] = [:]

      for index in 0..<20 {
        let id = "saturated-generated-\(index)"
        let format = formats[index % formats.count]
        let bytes = Data("{\"document\":\"\(format.0)\",\"read\":\(index)}\\n".utf8)
        let version = "generated-v\(index)"
        let stagedPath = try stageContent(bytes, in: scratch, name: "generated-\(index).bin")
        expectedBytes[id] = bytes
        stagedContent[id] = HydratedContent(
          stagedPath: stagedPath,
          contentVersion: version,
          byteCount: UInt64(bytes.count))
        harness.store.apply(
          generatedFile(
            id: id,
            name: format.0,
            mimeType: format.1,
            size: UInt64(bytes.count),
            contentVersion: version,
            chatId: Int64(10_000 + index)))
      }
      let contentByID = stagedContent
      for _ in 0..<20 {
        harness.hydration.enqueue { request, _ in
          started.signal()
          await release.waitUntilOpen()
          guard let content = contentByID[request.itemId] else {
            throw HydrationFailure(category: .internalError, detail: "unknown test item")
          }
          return content
        }
      }

      var futures: [(String, TestFuture<FetchOutcome>)] = []
      for index in 0..<20 {
        let id = "saturated-generated-\(index)"
        let future = TestFuture<FetchOutcome>()
        futures.append((id, future))
        _ = harness.start(NSFileProviderItemIdentifier(id), future: future)
      }

      // The File Provider gate keeps active cache reads bounded even
      // while the other seventeen Finder requests are pending.
      await started.waitFor(3)
      release.open()
      for (id, future) in futures {
        let outcome = await future.settled
        #expect(outcome.error == nil)
        let url = try #require(outcome.url)
        #expect(try Data(contentsOf: url) == expectedBytes[id])
      }
      #expect(harness.hydration.concurrentHighWater == 3)
      #expect(harness.hydration.requests.count == 20)
      #expect(harness.historyPriority.snapshot().filter { $0.priority == .requested }.count == 20)
      #expect(harness.historyPriority.snapshot().filter { $0.priority == .background }.count == 20)
      #expect(harness.fetcher.demandedChatCount == 0)
    }
  }

  @Test("Progress counts the hydration's bytes and completes with them")
  func progressReflectsHydration() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
      harness.hydration.enqueueSuccess(
        HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 5),
        progress: [HydrationProgress(bytesTransferred: 2, bytesTotal: 5)])

      let future = TestFuture<FetchOutcome>()
      let progress = harness.start(
        NSFileProviderItemIdentifier("f1"), future: future)
      let outcome = await future.settled

      #expect(outcome.error == nil)
      #expect(progress.totalUnitCount == 5)
      #expect(progress.completedUnitCount == 5)
      #expect(progress.fractionCompleted == 1.0)
    }
  }

  @Test("A stale requested version is served as the current version")
  func staleRequestedVersionServesCurrent() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
      harness.hydration.enqueueSuccess(
        HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 5))

      let stale = NSFileProviderItemVersion(
        contentVersion: Data("v0".utf8), metadataVersion: Data("m0".utf8))
      let outcome = await harness.fetch(
        NSFileProviderItemIdentifier("f1"), version: stale)

      #expect(outcome.error == nil)
      #expect(outcome.url != nil)
      // The current version was pinned and delivered; the returned
      // item carries it for the system to reconcile.
      #expect(harness.hydration.requests.first?.contentVersion == "v1")
      #expect(
        outcome.fetchedItem?.itemVersion.contentVersion == Data("v1".utf8))
    }
  }

  // MARK: Refusals before any IPC (POL-4, shape)

  @Test("Restricted content reports permission denial without contacting the agent")
  func restrictedRefusedWithoutIPC() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", availability: .restricted))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectCocoaError(.fileReadNoPermission)
      #expect(harness.hydration.requests.isEmpty)
    }
  }

  @Test("Open keeps a live unavailable item retryable without contacting the agent")
  func unavailableRefusedWithoutIPC() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", availability: .unavailable))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectProviderError(.serverUnreachable)
      #expect(harness.hydration.requests.isEmpty)
      let preserved = try #require(try harness.store.item(id: "f1"))
      #expect(preserved.deletedAtMs == nil)
    }
  }

  @Test("An expected-size-only placeholder retries without starting unverifiable hydration")
  func unknownExactSizeRefusedWithoutIPC() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      let metadata = file(id: "expected-only", size: nil)
      harness.store.apply(metadata)

      let projected = GramDriveFileProviderItem(
        metadata: metadata, accountRootId: rootId)
      #expect(projected.documentSize == nil)
      #expect(projected.capabilities == [])

      let outcome = await harness.fetch(
        NSFileProviderItemIdentifier("expected-only"))

      outcome.expectProviderError(.serverUnreachable)
      #expect(harness.hydration.requests.isEmpty)
      #expect(harness.fetcher.inFlightCount == 0)
    }
  }

  @Test("An immediate local refusal leaves no completed task in the ledger")
  func immediateCompletionIsForgotten() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("ghost"))

      outcome.expectProviderError(.noSuchItem)
      #expect(harness.fetcher.inFlightCount == 0)
    }
  }

  @Test("A directory has no content to fetch")
  func directoryRefused() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(directory(id: "d1"))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("d1"))

      outcome.expectCocoaError(.featureUnsupported)
      #expect(harness.hydration.requests.isEmpty)
    }
  }

  @Test("An unknown item answers noSuchItem without contacting the agent")
  func unknownItemRefused() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("ghost"))

      outcome.expectProviderError(.noSuchItem)
      #expect(harness.hydration.requests.isEmpty)
    }
  }

  @Test("A POL-3 tombstone answers noSuchItem")
  func tombstoneRefused() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      harness.store.tombstone(id: "f1", atMs: 2_000)

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectProviderError(.noSuchItem)
      #expect(harness.hydration.requests.isEmpty)
    }
  }

  // MARK: Version races (SYNC-042 as seen from the provider)

  @Test("A mid-fetch version conflict restarts once against the fresh snapshot")
  func versionConflictRestartsOnce() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", contentVersion: "v1"))
      let stagedPath = try stageContent(Data("fresh".utf8), in: scratch)
      harness.hydration.enqueue { [store = harness.store] _, _ in
        // The engine observed a newer version mid-fetch; by the
        // time the conflict answer arrives, the store shows it.
        store.apply(file(id: "f1", contentVersion: "v2"))
        throw HydrationFailure(category: .versionConflict, detail: "stale pin")
      }
      harness.hydration.enqueueSuccess(
        HydratedContent(stagedPath: stagedPath, contentVersion: "v2", byteCount: 5))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      #expect(outcome.error == nil)
      #expect(outcome.url != nil)
      let pins = harness.hydration.requests.map(\.contentVersion)
      #expect(pins == ["v1", "v2"])
      #expect(
        outcome.fetchedItem?.itemVersion.contentVersion == Data("v2".utf8))
    }
  }

  @Test("A conflict the store has not observed yet fails transiently, not in a spin")
  func conflictWithoutStoreMovementFailsSafely() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", contentVersion: "v1"))
      harness.hydration.enqueueFailure(
        HydrationFailure(category: .versionConflict, detail: "stale pin"))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectProviderError(.serverUnreachable)
      #expect(harness.hydration.requests.count == 1)
    }
  }

  @Test("An attachment's second conflict fails safely instead of chasing versions")
  func attachmentSecondConflictFailsSafely() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", contentVersion: "v1"))
      harness.hydration.enqueue { [store = harness.store] _, _ in
        store.apply(file(id: "f1", contentVersion: "v2"))
        throw HydrationFailure(category: .versionConflict, detail: "stale pin")
      }
      harness.hydration.enqueue { [store = harness.store] _, _ in
        store.apply(file(id: "f1", contentVersion: "v3"))
        throw HydrationFailure(category: .versionConflict, detail: "stale pin")
      }

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectProviderError(.serverUnreachable)
      #expect(harness.hydration.requests.count == 2)
    }
  }

  @Test("An actively crawling generated document follows multiple atomic watermark publications")
  func crawlingGeneratedDocumentFollowsMultiBumpWatermark() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      let generation3 = Data("{\"input_watermark_seq\":103}\n".utf8)
      let stagedPath = try stageContent(generation3, in: scratch)
      harness.store.apply(
        generatedFile(
          id: "crawl-messages-ndjson",
          name: "Messages.ndjson",
          mimeType: "application/x-ndjson",
          size: UInt64(generation3.count),
          contentVersion: "ndjson-w101"))
      harness.hydration.enqueue { [store = harness.store] _, _ in
        // Backfill publishes the next complete generation before the
        // cache-only admission can return the prior one.
        store.apply(
          generatedFile(
            id: "crawl-messages-ndjson",
            name: "Messages.ndjson",
            mimeType: "application/x-ndjson",
            size: UInt64(generation3.count),
            contentVersion: "ndjson-w102"))
        throw HydrationFailure(category: .versionConflict, detail: "watermark advanced")
      }
      harness.hydration.enqueue { [store = harness.store] _, _ in
        store.apply(
          generatedFile(
            id: "crawl-messages-ndjson",
            name: "Messages.ndjson",
            mimeType: "application/x-ndjson",
            size: UInt64(generation3.count),
            contentVersion: "ndjson-w103"))
        throw HydrationFailure(category: .versionConflict, detail: "watermark advanced")
      }
      harness.hydration.enqueueSuccess(
        HydratedContent(
          stagedPath: stagedPath,
          contentVersion: "ndjson-w103",
          byteCount: UInt64(generation3.count)))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("crawl-messages-ndjson"))

      #expect(outcome.error == nil)
      let url = try #require(outcome.url)
      #expect(try Data(contentsOf: url) == generation3)
      #expect(
        harness.hydration.requests.map(\.contentVersion) == [
          "ndjson-w101", "ndjson-w102", "ndjson-w103",
        ])
      // The returned bytes and the replacement File Provider item
      // describe the exact same fully published generation.
      #expect(outcome.fetchedItem?.itemVersion.contentVersion == Data("ndjson-w103".utf8))
    }
  }

  @Test("A stable chat generated document uses its current published generation")
  func stableChatGeneratedDocumentRefreshesToCurrentPublication() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      let current = Data("{\"chat\":\"stable\",\"generation\":2}\n".utf8)
      let stagedPath = try stageContent(current, in: scratch)
      harness.store.apply(
        generatedFile(
          id: "stable-chat-json",
          name: ".chat.json",
          mimeType: "application/json",
          size: UInt64(current.count),
          contentVersion: "chat-json-g1"))
      harness.hydration.enqueue { [store = harness.store] _, _ in
        store.apply(
          generatedFile(
            id: "stable-chat-json",
            name: ".chat.json",
            mimeType: "application/json",
            size: UInt64(current.count),
            contentVersion: "chat-json-g2"))
        throw HydrationFailure(category: .versionConflict, detail: "publication replaced")
      }
      harness.hydration.enqueueSuccess(
        HydratedContent(
          stagedPath: stagedPath,
          contentVersion: "chat-json-g2",
          byteCount: UInt64(current.count)))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("stable-chat-json"))

      #expect(outcome.error == nil)
      let url = try #require(outcome.url)
      #expect(try Data(contentsOf: url) == current)
      #expect(harness.hydration.requests.map(\.contentVersion) == ["chat-json-g1", "chat-json-g2"])
      #expect(outcome.fetchedItem?.itemVersion.contentVersion == Data("chat-json-g2".utf8))
    }
  }

  @Test("Generated-document publication chasing remains bounded")
  func generatedDocumentVersionChasingRemainsBounded() async throws {
    try await withFetchScratchDirectory { scratch in
      var configuration = ContentFetcherConfiguration()
      configuration.maxGeneratedVersionRestarts = 2
      let harness = Harness(scratch: scratch, configuration: configuration)
      harness.store.apply(
        generatedFile(
          id: "bounded-generated",
          name: "Messages.md",
          mimeType: "text/markdown",
          size: 5,
          contentVersion: "markdown-w1"))
      for version in ["markdown-w2", "markdown-w3", "markdown-w4"] {
        harness.hydration.enqueue { [store = harness.store] _, _ in
          store.apply(
            generatedFile(
              id: "bounded-generated",
              name: "Messages.md",
              mimeType: "text/markdown",
              size: 5,
              contentVersion: version))
          throw HydrationFailure(category: .versionConflict, detail: "watermark advanced")
        }
      }

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("bounded-generated"))

      outcome.expectProviderError(.serverUnreachable)
      #expect(
        harness.hydration.requests.map(\.contentVersion) == [
          "markdown-w1", "markdown-w2", "markdown-w3",
        ])
    }
  }

  @Test("Staged bytes reporting a foreign version are treated as the conflict they are")
  func stagedVersionMismatchIsAConflict() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", contentVersion: "v1"))
      let stagedPath = try stageContent(Data("wrong".utf8), in: scratch)
      harness.hydration.enqueueSuccess(
        HydratedContent(stagedPath: stagedPath, contentVersion: "v9", byteCount: 5))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      // The store never observed v9, so the restart refuses to spin.
      outcome.expectProviderError(.serverUnreachable)
    }
  }

  @Test("An item that turns restricted across a restart is refused, not fetched")
  func restartReChecksAvailability() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", contentVersion: "v1"))
      harness.hydration.enqueue { [store = harness.store] _, _ in
        store.apply(
          file(id: "f1", contentVersion: "v2", availability: .restricted))
        throw HydrationFailure(category: .versionConflict, detail: "stale pin")
      }

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectCocoaError(.fileReadNoPermission)
      #expect(harness.hydration.requests.count == 1)
    }
  }

  @Test("Open source-not-found preserves the live item and retries after fault clearance")
  func sourceNotFoundPreservesLiveItemAndRetries() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", availability: .fetchable))
      harness.hydration.enqueueFailure(
        HydrationFailure(category: .notFound, detail: "content gone at the source"))
      let staged = try stageContent(Data("retry".utf8), in: scratch)
      harness.hydration.enqueueSuccess(
        HydratedContent(stagedPath: staged, contentVersion: "v1", byteCount: 5))

      let first = await harness.fetch(NSFileProviderItemIdentifier("f1"))
      first.expectProviderError(.serverUnreachable)
      let preserved = try #require(try harness.store.item(id: "f1"))
      #expect(preserved.deletedAtMs == nil)
      #expect(
        GramDriveFileProviderItem(metadata: preserved, accountRootId: rootId).itemIdentifier
          == NSFileProviderItemIdentifier("f1"))

      let retry = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      #expect(retry.error == nil)
      #expect(retry.fetchedItem?.itemIdentifier == NSFileProviderItemIdentifier("f1"))
      #expect(harness.hydration.requests.count == 2)
      let record = try #require(harness.telemetry.snapshot().first)
      #expect(record.outcome == "engine-not-found")
      #expect(record.engineFailure)
      #expect(record.providerMapping)
      #expect(!record.noSuchItem)
      #expect(record.retryable)
    }
  }

  @Test("Open retryable hydration failures preserve the durable item and retry after clearance")
  func retryableHydrationFailuresPreserveLiveItemAndRetry() async throws {
    try await withFetchScratchDirectory { scratch in
      for fault in LiveOpenFault.allCases {
        let harness = Harness(scratch: scratch)
        harness.store.apply(file(id: "f1"))
        fault.enqueue(on: harness.hydration)
        let staged = try stageContent(Data("retry".utf8), in: scratch, name: "\(fault).bin")
        harness.hydration.enqueueSuccess(
          HydratedContent(stagedPath: staged, contentVersion: "v1", byteCount: 5))

        let first = await harness.fetch(NSFileProviderItemIdentifier("f1"))
        first.expectProviderError(.serverUnreachable)
        let preserved = try #require(try harness.store.item(id: "f1"))
        #expect(preserved.deletedAtMs == nil)
        #expect(preserved.parent == rootId)
        #expect(preserved.contentVersion == "v1")
        #expect(
          GramDriveFileProviderItem(metadata: preserved, accountRootId: rootId).itemIdentifier
            == NSFileProviderItemIdentifier("f1"))

        let retry = await harness.fetch(NSFileProviderItemIdentifier("f1"))
        #expect(retry.error == nil)
        #expect(retry.fetchedItem?.itemIdentifier == NSFileProviderItemIdentifier("f1"))
        #expect(retry.fetchedItem?.parentItemIdentifier == .rootContainer)
        #expect(retry.fetchedItem?.itemVersion.contentVersion == Data("v1".utf8))
        #expect(harness.hydration.requests.count == 2)
      }
    }
  }

  @Test("Open unavailable live content keeps its durable item and retries after availability clears")
  func unavailableLiveItemPreservesIdentityAndRetries() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", availability: .unavailable))

      let first = await harness.fetch(NSFileProviderItemIdentifier("f1"))
      first.expectProviderError(.serverUnreachable)
      let preserved = try #require(try harness.store.item(id: "f1"))
      #expect(preserved.deletedAtMs == nil)
      #expect(preserved.parent == rootId)
      #expect(preserved.contentVersion == "v1")
      #expect(harness.hydration.requests.isEmpty)

      harness.store.apply(file(id: "f1", availability: .fetchable))
      let staged = try stageContent(Data("retry".utf8), in: scratch, name: "unavailable-retry.bin")
      harness.hydration.enqueueSuccess(
        HydratedContent(stagedPath: staged, contentVersion: "v1", byteCount: 5))
      let retry = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      #expect(retry.error == nil)
      #expect(retry.fetchedItem?.itemIdentifier == NSFileProviderItemIdentifier("f1"))
      #expect(retry.fetchedItem?.parentItemIdentifier == .rootContainer)
      #expect(retry.fetchedItem?.itemVersion.contentVersion == Data("v1".utf8))
    }
  }

  // MARK: Materialization guarantees (PRD-043)

  @Test("A staged file shorter than promised is never published")
  func partialContentNeverPublished() async throws {
    try await withFetchScratchDirectory { scratch in
      let staging = scratch.appendingPathComponent("cache", isDirectory: true)
      try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", size: 100))
      let stagedPath = try stageContent(Data("short".utf8), in: staging)
      harness.hydration.enqueueSuccess(
        HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 100))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectProviderError(.cannotSynchronize)
      // The failed copy was deleted, not left for anyone to find.
      let scratchLeftovers = try FileManager.default.contentsOfDirectory(atPath: scratch.path)
        .filter { $0.hasPrefix("fetch-") }
      #expect(scratchLeftovers.isEmpty)
    }
  }

  @Test("A staged file that vanished is a transient service condition")
  func missingStagedFileIsTransient() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      harness.hydration.enqueueSuccess(
        HydratedContent(
          stagedPath: scratch.appendingPathComponent("gone.bin").path,
          contentVersion: "v1",
          byteCount: 5))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectProviderError(.serverUnreachable)
    }
  }

  // MARK: Agent unavailable and failure mapping

  @Test("An unreachable agent maps to serverUnreachable")
  func agentUnavailableMapsToServerUnreachable() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      harness.hydration.enqueueTransportFailure(
        .agentUnavailable(path: "/nowhere/hydration.sock"))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectProviderError(.serverUnreachable)
      let record = try #require(harness.telemetry.snapshot().first)
      #expect(record.outcome == "engine-transport")
      #expect(record.engineFailure)
      #expect(record.providerMapping)
      #expect(record.retryable)
    }
  }

  @Test(
    "A raw socket fault below the protocol layer maps to serverUnreachable",
    arguments: [EPIPE, ECONNRESET, EINTR, EMFILE, EPERM, ENOTCONN])
  func rawSocketFaultMapsToServerUnreachable(code: Int32) async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      // What the client throws on socket() exhaustion, an EPIPE send to
      // a dead agent, or an interrupted/reset read — none of which are
      // HydrationTransportError, so they must not escape unmapped.
      harness.hydration.enqueueSocketFailure(
        .failed(operation: "read", code: code))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectProviderError(.serverUnreachable)
    }
  }

  @Test("An unrepresentable socket path maps to serverUnreachable")
  func unrepresentableSocketPathMapsToServerUnreachable() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      harness.hydration.enqueueSocketFailure(
        .pathUnrepresentable(path: "/some/very/long/socket/path"))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectProviderError(.serverUnreachable)
    }
  }

  @Test("A failure category the agent reports maps end to end")
  func reportedFailureMapsEndToEnd() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      harness.hydration.enqueueFailure(
        HydrationFailure(category: .authRequired, detail: "logged out"))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectProviderError(.notAuthenticated)
    }
  }

  @Test("Every wire category maps onto the provider error surface")
  func categoryMappingTable() {
    func code(_ category: HydrationFailureCategory) -> (domain: String, code: Int) {
      let error =
        ContentFetcher.providerError(
          for: HydrationFailure(category: category, detail: "test")) as NSError
      return (error.domain, error.code)
    }
    let provider = NSFileProviderError.errorDomain
    let cocoa = CocoaError.errorDomain
    #expect(code(.notFound) == (provider, NSFileProviderError.Code.serverUnreachable.rawValue))
    #expect(code(.restricted) == (cocoa, CocoaError.Code.fileReadNoPermission.rawValue))
    #expect(
      code(.authRequired) == (provider, NSFileProviderError.Code.notAuthenticated.rawValue))
    #expect(code(.cancelled) == (cocoa, CocoaError.Code.userCancelled.rawValue))
    for transient in [
      HydrationFailureCategory.versionConflict, .rateLimited,
      .sourceUnavailable, .draining, .busy,
    ] {
      #expect(
        code(transient)
          == (provider, NSFileProviderError.Code.serverUnreachable.rawValue))
    }
    for broken in [HydrationFailureCategory.storage, .integrity, .internalError] {
      #expect(
        code(broken)
          == (provider, NSFileProviderError.Code.cannotSynchronize.rawValue))
    }

    let cannotSynchronize = ProviderFetchTelemetry.classification(
      for: NSFileProviderError(.cannotSynchronize))
    #expect(cannotSynchronize.outcome == "provider-error")
    #expect(!cannotSynchronize.retryable)
  }

  // MARK: Cancellation

  @Test("Cancelling the returned Progress cancels a running hydration")
  func cancellationDuringHydration() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      let started = ArrivalCounter()
      harness.hydration.enqueue { _, _ in
        started.signal()
        // Parks until the surrounding task is cancelled — the
        // client contract's cancellation shape.
        try await Task.sleep(for: .seconds(600))
        throw HydrationFailure(category: .internalError, detail: "unreachable")
      }

      let future = TestFuture<FetchOutcome>()
      let progress = harness.start(NSFileProviderItemIdentifier("f1"), future: future)
      await started.waitFor(1)
      progress.cancel()
      let outcome = await future.settled

      outcome.expectCocoaError(.userCancelled)
      #expect(future.fulfillmentCount == 1)
    }
  }

  @Test("A fetch cancelled while queued never contacts the agent")
  func cancellationWhileQueued() async throws {
    try await withFetchScratchDirectory { scratch in
      var configuration = ContentFetcherConfiguration()
      configuration.maxConcurrentFetches = 1
      let harness = Harness(scratch: scratch, configuration: configuration)
      harness.store.apply(file(id: "f1"))
      harness.store.apply(file(id: "f2"))
      let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
      let started = ArrivalCounter()
      let release = ManualGate()
      harness.hydration.enqueue { _, _ in
        started.signal()
        await release.waitUntilOpen()
        return HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 5)
      }

      let first = TestFuture<FetchOutcome>()
      _ = harness.start(NSFileProviderItemIdentifier("f1"), future: first)
      await started.waitFor(1)

      let second = TestFuture<FetchOutcome>()
      let queuedProgress = harness.start(NSFileProviderItemIdentifier("f2"), future: second)
      queuedProgress.cancel()
      let queuedOutcome = await second.settled
      queuedOutcome.expectCocoaError(.userCancelled)

      release.open()
      let firstOutcome = await first.settled
      #expect(firstOutcome.error == nil)
      // The cancelled fetch never reached the agent.
      #expect(harness.hydration.requests.map(\.itemId) == ["f1"])
    }
  }

  @Test("cancelAll unwinds every in-flight fetch (the invalidation path)")
  func cancelAllUnwindsInFlight() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      let started = ArrivalCounter()
      harness.hydration.enqueue { _, _ in
        started.signal()
        try await Task.sleep(for: .seconds(600))
        throw HydrationFailure(category: .internalError, detail: "unreachable")
      }

      let future = TestFuture<FetchOutcome>()
      _ = harness.start(NSFileProviderItemIdentifier("f1"), future: future)
      await started.waitFor(1)
      harness.fetcher.cancelAll()
      let outcome = await future.settled

      outcome.expectCocoaError(.userCancelled)
    }
  }

  // MARK: Concurrency bound (NFR-021)

  @Test("Concurrent fetches are bounded by the gate, and all complete")
  func concurrentFetchesBounded() async throws {
    try await withFetchScratchDirectory { scratch in
      var configuration = ContentFetcherConfiguration()
      configuration.maxConcurrentFetches = 2
      let harness = Harness(scratch: scratch, configuration: configuration)
      let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
      let started = ArrivalCounter()
      let release = ManualGate()
      for index in 1...5 {
        harness.store.apply(file(id: "f\(index)"))
        harness.hydration.enqueue { _, _ in
          started.signal()
          await release.waitUntilOpen()
          return HydratedContent(
            stagedPath: stagedPath, contentVersion: "v1", byteCount: 5)
        }
      }

      var futures: [TestFuture<FetchOutcome>] = []
      for index in 1...5 {
        let future = TestFuture<FetchOutcome>()
        futures.append(future)
        _ = harness.start(NSFileProviderItemIdentifier("f\(index)"), future: future)
      }
      // Exactly the gate's width starts; the rest wait their turn.
      await started.waitFor(2)
      release.open()
      for future in futures {
        let outcome = await future.settled
        #expect(outcome.error == nil)
      }
      #expect(harness.hydration.concurrentHighWater == 2)
      #expect(harness.hydration.requests.count == 5)
    }
  }

  // MARK: Foreground history demand (BUG-260728-2qfzbd)

  @Test("A content read raises requested demand for its chat and releases it when it settles")
  func readRaisesAndReleasesChatDemand() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", chatId: 900))
      let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
      harness.hydration.enqueueSuccess(
        HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 5))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      #expect(outcome.error == nil)
      #expect(
        harness.historyPriority.snapshot() == [
          HistoryPriorityRequest(
            accountId: accountId, chatId: 900, priority: .requested),
          HistoryPriorityRequest(
            accountId: accountId, chatId: 900, priority: .background),
        ])
      #expect(harness.fetcher.demandedChatCount == 0)
    }
  }

  @Test("Content withheld by policy still says which chat the user is in")
  func refusedReadStillRaisesChatDemand() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", availability: .restricted, chatId: 900))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      outcome.expectCocoaError(.fileReadNoPermission)
      // POL-4 withholds the bytes; the gesture is the same evidence
      // about which chat is being read either way, and no agent
      // transfer was started to produce it.
      #expect(harness.hydration.requests.isEmpty)
      #expect(
        harness.historyPriority.snapshot().map(\.priority) == [.requested, .background])
    }
  }

  @Test("A read of an item outside any chat raises no history demand")
  func readOutsideAChatRaisesNothing() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1"))
      let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
      harness.hydration.enqueueSuccess(
        HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 5))

      let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

      #expect(outcome.error == nil)
      #expect(harness.historyPriority.snapshot().isEmpty)
    }
  }

  @Test("Overlapping reads of one chat raise one hint and release it once")
  func overlappingReadsRaiseOneHint() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", chatId: 900))
      harness.store.apply(file(id: "f2", chatId: 900))
      let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
      let started = ArrivalCounter()
      let release = ManualGate()
      for _ in 1...2 {
        harness.hydration.enqueue { _, _ in
          started.signal()
          await release.waitUntilOpen()
          return HydratedContent(
            stagedPath: stagedPath, contentVersion: "v1", byteCount: 5)
        }
      }

      let first = TestFuture<FetchOutcome>()
      let second = TestFuture<FetchOutcome>()
      _ = harness.start(NSFileProviderItemIdentifier("f1"), future: first)
      _ = harness.start(NSFileProviderItemIdentifier("f2"), future: second)
      await started.waitFor(2)
      // Both reads are inside the same chat: the second must not raise
      // a second hint, and the first to finish must not release the
      // demand the other one is still holding.
      #expect(
        harness.historyPriority.snapshot() == [
          HistoryPriorityRequest(
            accountId: accountId, chatId: 900, priority: .requested)
        ])
      #expect(harness.fetcher.demandedChatCount == 1)

      release.open()
      #expect(await first.settled.error == nil)
      #expect(await second.settled.error == nil)
      #expect(
        harness.historyPriority.snapshot().map(\.priority) == [.requested, .background])
      #expect(harness.fetcher.demandedChatCount == 0)
    }
  }

  @Test("A cancelled read releases the demand it raised")
  func cancelledReadReleasesChatDemand() async throws {
    try await withFetchScratchDirectory { scratch in
      let harness = Harness(scratch: scratch)
      harness.store.apply(file(id: "f1", chatId: 900))
      let started = ArrivalCounter()
      harness.hydration.enqueue { _, _ in
        started.signal()
        try await Task.sleep(for: .seconds(600))
        throw HydrationFailure(category: .internalError, detail: "unreachable")
      }

      let future = TestFuture<FetchOutcome>()
      let progress = harness.start(NSFileProviderItemIdentifier("f1"), future: future)
      await started.waitFor(1)
      progress.cancel()
      let outcome = await future.settled

      outcome.expectCocoaError(.userCancelled)
      #expect(
        harness.historyPriority.snapshot().map(\.priority) == [.requested, .background])
      #expect(harness.fetcher.demandedChatCount == 0)
    }
  }
}
