import CoreGraphics
import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport
import ImageIO
import Testing
import UniformTypeIdentifiers

@testable import GramDriveFileProvider

private let thumbnailAccount = AccountInfo(
  accountId: 7,
  sourceKind: .localTdlib,
  displayName: "Test Account",
  authState: "authorized",
  namespaceVersion: 1,
  displayTimezone: "UTC",
  rootItemId: "root")

private enum ThumbnailFixtureError: Error {
  case cannotCreateContext
  case cannotCreateImage
  case cannotCreateDestination
  case cannotEncode
}

private func pngFixture(width: Int, height: Int) throws -> Data {
  guard
    let context = CGContext(
      data: nil,
      width: width,
      height: height,
      bitsPerComponent: 8,
      bytesPerRow: width * 4,
      space: CGColorSpaceCreateDeviceRGB(),
      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
  else {
    throw ThumbnailFixtureError.cannotCreateContext
  }
  context.setFillColor(red: 0.2, green: 0.4, blue: 0.8, alpha: 1)
  context.fill(CGRect(x: 0, y: 0, width: width, height: height))
  guard let image = context.makeImage() else {
    throw ThumbnailFixtureError.cannotCreateImage
  }
  let output = NSMutableData()
  guard
    let destination = CGImageDestinationCreateWithData(
      output, UTType.png.identifier as CFString, 1, nil)
  else {
    throw ThumbnailFixtureError.cannotCreateDestination
  }
  CGImageDestinationAddImage(destination, image, nil)
  guard CGImageDestinationFinalize(destination) else {
    throw ThumbnailFixtureError.cannotEncode
  }
  return output as Data
}

private func imageDimensions(_ data: Data?) -> CGSize? {
  guard let data,
    let source = CGImageSourceCreateWithData(data as CFData, nil),
    let image = CGImageSourceCreateImageAtIndex(source, 0, nil)
  else {
    return nil
  }
  return CGSize(width: image.width, height: image.height)
}

private func thumbnailItem(
  id: String,
  parent: String = "month",
  logicalKind: String? = "photo",
  mimeType: String? = "image/jpeg",
  availability: ItemAvailability = .fetchable,
  contentVersion: String = "v1"
) -> ItemMetadata {
  ItemMetadata(
    contractVersion: 2,
    id: id, parent: parent, kind: .attachment, isDirectory: false,
    displayName: id, safeName: "2026-07-21 12-34-56 photo.jpg", metadataVersion: "m1",
    mimeType: mimeType, logicalSize: 10_000, attachmentLogicalKind: logicalKind,
    attachmentRepresentation: "message_photo", attachmentFidelity: "telegram_variant",
    attachmentSourceName: nil, attachmentExactSize: 10_000,
    contentVersion: contentVersion, availability: availability,
    createdAtMs: 1_721_562_896_000, modifiedAtMs: 1_721_562_896_000,
    deletedAtMs: nil)
}

/// Preview's equivalent set of post-admission transient failures. A
/// thumbnail has its own endpoint, so keep this matrix separate from Open.
private enum LivePreviewFault: CaseIterable {
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
        HydrationFailure(category: .notFound, detail: "renderer lost preview source"))
    case .sourceUnavailable:
      hydration.enqueueFailure(
        HydrationFailure(category: .sourceUnavailable, detail: "preview source temporarily unavailable"))
    }
  }
}

private final class ThumbnailRecorder: @unchecked Sendable {
  struct ItemResult: @unchecked Sendable {
    let data: Data?
    let error: (any Error)?
  }

  private let lock = NSLock()
  private var results: [String: ItemResult] = [:]
  struct GlobalResult: @unchecked Sendable {
    let error: (any Error)?
  }

  let completed = TestFuture<GlobalResult>()

  func record(_ identifier: NSFileProviderItemIdentifier, _ data: Data?, _ error: (any Error)?) {
    lock.lock()
    results[identifier.rawValue] = ItemResult(data: data, error: error)
    lock.unlock()
  }

  func result(_ id: String) -> ItemResult? {
    lock.lock()
    defer { lock.unlock() }
    return results[id]
  }
}

private final class ThumbnailTelemetryRecorder: ProviderFetchObserving, @unchecked Sendable {
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

private func runThumbnailFetch(
  _ fetcher: ThumbnailFetcher,
  identifiers: [String],
  size: CGSize,
  store: ScriptedStore
) async -> (Progress, ThumbnailRecorder) {
  let recorder = ThumbnailRecorder()
  let progress = fetcher.fetchThumbnails(
    itemIdentifiers: identifiers.map { NSFileProviderItemIdentifier($0) },
    requestedSize: size,
    context: { (account: thumbnailAccount, store: store) },
    perItemCompletion: recorder.record,
    completion: { recorder.completed.fulfill(.init(error: $0)) })
  _ = await recorder.completed.settled
  return (progress, recorder)
}

@Suite("Bounded File Provider thumbnails")
struct ThumbnailFetcherTests {
  @Test("Thumbnail demand reads metadata on the user-initiated executor")
  func thumbnailDemandUsesForegroundExecutor() async throws {
    try await withFetchScratchDirectory { scratch in
      let store = ScriptedStore(account: thumbnailAccount)
      store.apply(thumbnailItem(id: "photo"))
      let hydration = ScriptedHydration()
      let preview = try pngFixture(width: 16, height: 12)
      let staged = try stageContent(preview, in: scratch, name: "foreground-preview.png")
      hydration.enqueueSuccess(
        HydratedContent(
          stagedPath: staged, contentVersion: "v1",
          byteCount: UInt64(preview.count), mimeType: "image/png"))

      let (_, recorder) = await runThumbnailFetch(
        ThumbnailFetcher(hydration: hydration), identifiers: ["photo"],
        size: CGSize(width: 64, height: 64), store: store)

      #expect(recorder.result("photo")?.error == nil)
      #expect(!store.itemQos.isEmpty)
      #expect(store.itemQos.allSatisfy { $0 == QOS_CLASS_USER_INITIATED })
      #expect(hydration.priorities.allSatisfy { $0 >= .userInitiated })
    }
  }

  @Test("Thumbnail callbacks emit redacted fetch telemetry and preserve engine attribution")
  func thumbnailTelemetryIsPrivacySafeAndClassified() async throws {
    let store = ScriptedStore(account: thumbnailAccount)
    let rawIdentifier = "chat-Alice-telegram-123456789-photo"
    store.apply(thumbnailItem(id: rawIdentifier))
    let hydration = ScriptedHydration()
    hydration.enqueueFailure(HydrationFailure(category: .notFound, detail: "source unavailable"))
    let telemetry = ThumbnailTelemetryRecorder()

    let (_, recorder) = await runThumbnailFetch(
      ThumbnailFetcher(hydration: hydration, telemetry: telemetry),
      identifiers: [rawIdentifier], size: CGSize(width: 128, height: 128), store: store)

    let error = recorder.result(rawIdentifier)?.error as NSError?
    #expect(error?.code == NSFileProviderError.Code.serverUnreachable.rawValue)
    let record = try #require(telemetry.snapshot().first)
    #expect(record.callback == "fetchThumbnails")
    #expect(
      record.itemToken
        == ProviderFetchTelemetry.itemToken(
          for: NSFileProviderItemIdentifier(rawIdentifier)))
    #expect(record.itemToken != rawIdentifier)
    #expect(!record.itemToken.contains("Alice"))
    #expect(!record.itemToken.contains("123456789"))
    #expect(record.outcome == "engine-not-found")
    #expect(record.engineFailure)
    #expect(record.providerMapping)
    #expect(!record.noSuchItem)
    #expect(record.retryable)
  }

  @Test("Preview source-not-found preserves the live item and retries after fault clearance")
  func previewSourceNotFoundPreservesLiveItemAndRetries() async throws {
    try await withFetchScratchDirectory { scratch in
      let store = ScriptedStore(account: thumbnailAccount)
      store.apply(thumbnailItem(id: "photo"))
      let hydration = ScriptedHydration()
      hydration.enqueueFailure(
        HydrationFailure(category: .notFound, detail: "preview source unavailable"))
      let preview = try pngFixture(width: 16, height: 12)
      let staged = try stageContent(preview, in: scratch, name: "retry-preview.png")
      hydration.enqueueSuccess(
        HydratedContent(
          stagedPath: staged, contentVersion: "v1",
          byteCount: UInt64(preview.count), mimeType: "image/png"))
      let fetcher = ThumbnailFetcher(hydration: hydration)

      let (_, first) = await runThumbnailFetch(
        fetcher, identifiers: ["photo"], size: CGSize(width: 64, height: 64), store: store)
      let firstError = first.result("photo")?.error as NSError?
      #expect(firstError?.domain == NSFileProviderError.errorDomain)
      #expect(firstError?.code == NSFileProviderError.Code.serverUnreachable.rawValue)
      let preserved = try #require(try store.item(id: "photo"))
      #expect(preserved.deletedAtMs == nil)

      let (_, retry) = await runThumbnailFetch(
        fetcher, identifiers: ["photo"], size: CGSize(width: 64, height: 64), store: store)
      #expect(retry.result("photo")?.error == nil)
      #expect(retry.result("photo")?.data != nil)
      #expect(hydration.requests.count == 2)
    }
  }

  @Test("Preview retryable hydration failures preserve the durable item and retry after clearance")
  func retryableHydrationFailuresPreserveLiveItemAndRetry() async throws {
    try await withFetchScratchDirectory { scratch in
      for fault in LivePreviewFault.allCases {
        let store = ScriptedStore(account: thumbnailAccount)
        store.apply(thumbnailItem(id: "photo"))
        let hydration = ScriptedHydration()
        fault.enqueue(on: hydration)
        let preview = try pngFixture(width: 16, height: 12)
        let staged = try stageContent(preview, in: scratch, name: "\(fault).png")
        hydration.enqueueSuccess(
          HydratedContent(
            stagedPath: staged, contentVersion: "v1",
            byteCount: UInt64(preview.count), mimeType: "image/png"))
        let fetcher = ThumbnailFetcher(hydration: hydration)

        let (_, first) = await runThumbnailFetch(
          fetcher, identifiers: ["photo"], size: CGSize(width: 64, height: 64), store: store)
        let firstError = first.result("photo")?.error as NSError?
        #expect(firstError?.domain == NSFileProviderError.errorDomain)
        #expect(firstError?.code == NSFileProviderError.Code.serverUnreachable.rawValue)
        let preserved = try #require(try store.item(id: "photo"))
        #expect(preserved.deletedAtMs == nil)
        #expect(preserved.parent == "month")
        #expect(preserved.contentVersion == "v1")

        let (_, retry) = await runThumbnailFetch(
          fetcher, identifiers: ["photo"], size: CGSize(width: 64, height: 64), store: store)
        #expect(retry.result("photo")?.error == nil)
        #expect(retry.result("photo")?.data != nil)
        #expect(hydration.requests.count == 2)
      }
    }
  }

  @Test("Preview unavailable live content keeps its durable item and retries after availability clears")
  func unavailableLiveItemPreservesIdentityAndRetries() async throws {
    try await withFetchScratchDirectory { scratch in
      let store = ScriptedStore(account: thumbnailAccount)
      store.apply(thumbnailItem(id: "photo", availability: .unavailable))
      let hydration = ScriptedHydration()
      let fetcher = ThumbnailFetcher(hydration: hydration)

      let (_, first) = await runThumbnailFetch(
        fetcher, identifiers: ["photo"], size: CGSize(width: 64, height: 64), store: store)
      let firstError = first.result("photo")?.error as NSError?
      #expect(firstError?.domain == NSFileProviderError.errorDomain)
      #expect(firstError?.code == NSFileProviderError.Code.serverUnreachable.rawValue)
      let preserved = try #require(try store.item(id: "photo"))
      #expect(preserved.deletedAtMs == nil)
      #expect(preserved.parent == "month")
      #expect(preserved.contentVersion == "v1")
      #expect(hydration.requests.isEmpty)

      store.apply(thumbnailItem(id: "photo", availability: .fetchable))
      let preview = try pngFixture(width: 16, height: 12)
      let staged = try stageContent(preview, in: scratch, name: "unavailable-retry.png")
      hydration.enqueueSuccess(
        HydratedContent(
          stagedPath: staged, contentVersion: "v1",
          byteCount: UInt64(preview.count), mimeType: "image/png"))
      let (_, retry) = await runThumbnailFetch(
        fetcher, identifiers: ["photo"], size: CGSize(width: 64, height: 64), store: store)
      #expect(retry.result("photo")?.error == nil)
      #expect(retry.result("photo")?.data != nil)
    }
  }

  @Test("An eligible preview uses the thumbnail operation with clamped bounds")
  func eligiblePreviewUsesDedicatedOperation() async throws {
    try await withFetchScratchDirectory { scratch in
      let store = ScriptedStore(account: thumbnailAccount)
      store.apply(thumbnailItem(id: "photo"))
      let hydration = ScriptedHydration()
      let preview = try pngFixture(width: 16, height: 12)
      let staged = try stageContent(preview, in: scratch, name: "preview.png")
      hydration.enqueueSuccess(
        HydratedContent(
          stagedPath: staged, contentVersion: "v1",
          byteCount: UInt64(preview.count), mimeType: "image/png"))
      let fetcher = ThumbnailFetcher(hydration: hydration)

      let (_, recorder) = await runThumbnailFetch(
        fetcher, identifiers: ["photo"],
        size: CGSize(
          width: CGFloat.greatestFiniteMagnitude,
          height: CGFloat.greatestFiniteMagnitude),
        store: store)

      #expect(
        imageDimensions(recorder.result("photo")?.data)
          == CGSize(width: 16, height: 12))
      #expect(recorder.result("photo")?.error == nil)
      #expect(hydration.requests.count == 1)
      #expect(hydration.requests.first?.purpose == .thumbnail)
      #expect(hydration.requests.first?.maxWidthPx == 512)
      #expect(hydration.requests.first?.maxHeightPx == 512)
      #expect(hydration.requests.first?.contentVersion == "v1")
    }
  }

  @Test("Returned image pixels are downsampled inside the requested box")
  func returnedPixelsAreBounded() async throws {
    try await withFetchScratchDirectory { scratch in
      let store = ScriptedStore(account: thumbnailAccount)
      store.apply(thumbnailItem(id: "oversized-photo"))
      let hydration = ScriptedHydration()
      let preview = try pngFixture(width: 320, height: 240)
      let staged = try stageContent(preview, in: scratch, name: "oversized.png")
      hydration.enqueueSuccess(
        HydratedContent(
          stagedPath: staged, contentVersion: "v1",
          byteCount: UInt64(preview.count), mimeType: "image/png"))
      let fetcher = ThumbnailFetcher(hydration: hydration)

      let (_, recorder) = await runThumbnailFetch(
        fetcher, identifiers: ["oversized-photo"],
        size: CGSize(width: 64, height: 32), store: store)

      let dimensions = imageDimensions(recorder.result("oversized-photo")?.data)
      #expect(dimensions?.width ?? .greatestFiniteMagnitude <= 64)
      #expect(dimensions?.height ?? .greatestFiniteMagnitude <= 32)
      #expect(dimensions?.width ?? 0 > 0)
      #expect(dimensions?.height ?? 0 > 0)
      #expect(recorder.result("oversized-photo")?.error == nil)
      #expect(hydration.requests.first?.maxWidthPx == 64)
      #expect(hydration.requests.first?.maxHeightPx == 32)
    }
  }

  @Test("A corrupt staged image is never published to Finder")
  func corruptImageIsRejected() async throws {
    try await withFetchScratchDirectory { scratch in
      let store = ScriptedStore(account: thumbnailAccount)
      store.apply(thumbnailItem(id: "corrupt-photo"))
      let hydration = ScriptedHydration()
      let bytes = Data([0x89, 0x50, 0x4E, 0x47])
      let staged = try stageContent(bytes, in: scratch, name: "corrupt.png")
      hydration.enqueueSuccess(
        HydratedContent(
          stagedPath: staged, contentVersion: "v1",
          byteCount: UInt64(bytes.count), mimeType: "image/png"))

      let (_, recorder) = await runThumbnailFetch(
        ThumbnailFetcher(hydration: hydration),
        identifiers: ["corrupt-photo"],
        size: CGSize(width: 64, height: 64), store: store)

      let error = recorder.result("corrupt-photo")?.error as NSError?
      #expect(recorder.result("corrupt-photo")?.data == nil)
      #expect(error?.domain == NSFileProviderError.errorDomain)
      #expect(error?.code == NSFileProviderError.Code.cannotSynchronize.rawValue)
    }
  }

  @Test("Preview keeps restricted local but unavailable live items retryable")
  func policyRefusalsAreLocal() async {
    let store = ScriptedStore(account: thumbnailAccount)
    store.apply(thumbnailItem(id: "restricted", availability: .restricted))
    store.apply(thumbnailItem(id: "unavailable", availability: .unavailable))
    let hydration = ScriptedHydration()
    let fetcher = ThumbnailFetcher(hydration: hydration)

    let (_, recorder) = await runThumbnailFetch(
      fetcher, identifiers: ["restricted", "unavailable"],
      size: CGSize(width: 128, height: 128), store: store)

    let restricted = recorder.result("restricted")?.error as NSError?
    #expect(restricted?.domain == CocoaError.errorDomain)
    #expect(restricted?.code == CocoaError.fileReadNoPermission.rawValue)
    let unavailable = recorder.result("unavailable")?.error as NSError?
    #expect(unavailable?.domain == NSFileProviderError.errorDomain)
    #expect(unavailable?.code == NSFileProviderError.Code.serverUnreachable.rawValue)
    #expect(hydration.requests.isEmpty)
  }

  @Test("Non-preview documents answer no thumbnail without hydration")
  func nonPreviewDocumentIsEmpty() async {
    let store = ScriptedStore(account: thumbnailAccount)
    store.apply(
      thumbnailItem(
        id: "document", logicalKind: "document", mimeType: "application/pdf"))
    let hydration = ScriptedHydration()

    let fetcher = ThumbnailFetcher(hydration: hydration)
    let (_, recorder) = await runThumbnailFetch(
      fetcher, identifiers: ["document"],
      size: CGSize(width: 128, height: 128), store: store)

    #expect(recorder.result("document")?.data == nil)
    #expect(recorder.result("document")?.error == nil)
    #expect(hydration.requests.isEmpty)
    #expect(fetcher.inFlightCount == 0)
  }

  @Test("Cancellation tears down an in-flight preview and completes typed")
  func cancellation() async {
    let store = ScriptedStore(account: thumbnailAccount)
    store.apply(thumbnailItem(id: "photo", parent: "root"))
    let hydration = ScriptedHydration()
    let arrived = ArrivalCounter()
    hydration.enqueue { _, _ in
      arrived.signal()
      try await Task.sleep(for: .seconds(30))
      throw HydrationFailure(category: .internalError, detail: "unreachable")
    }
    let recorder = ThumbnailRecorder()
    let fetcher = ThumbnailFetcher(hydration: hydration)
    let progress = fetcher.fetchThumbnails(
      itemIdentifiers: [NSFileProviderItemIdentifier("photo")],
      requestedSize: CGSize(width: 128, height: 128),
      context: { (account: thumbnailAccount, store: store) },
      perItemCompletion: recorder.record,
      completion: { recorder.completed.fulfill(.init(error: $0)) })
    await arrived.waitFor(1)

    progress.cancel()
    let global = await recorder.completed.settled.error as NSError?
    let item = recorder.result("photo")?.error as NSError?
    #expect(global?.domain == CocoaError.errorDomain)
    #expect(global?.code == CocoaError.userCancelled.rawValue)
    #expect(item?.domain == CocoaError.errorDomain)
    #expect(item?.code == CocoaError.userCancelled.rawValue)
  }

  @Test("Preview work has its own concurrency bound")
  func concurrencyBound() async throws {
    try await withFetchScratchDirectory { scratch in
      let store = ScriptedStore(account: thumbnailAccount)
      let hydration = ScriptedHydration()
      let bytes = try pngFixture(width: 16, height: 12)
      let staged = try stageContent(bytes, in: scratch, name: "shared-preview.png")
      for index in 0..<5 {
        store.apply(thumbnailItem(id: "photo-\(index)"))
        hydration.enqueue { request, _ in
          try await Task.sleep(for: .milliseconds(20))
          return HydratedContent(
            stagedPath: staged, contentVersion: request.contentVersion,
            byteCount: UInt64(bytes.count), mimeType: "image/png")
        }
      }
      let fetcher = ThumbnailFetcher(
        hydration: hydration,
        configuration: .init(maxConcurrentFetches: 2))
      let (_, recorder) = await runThumbnailFetch(
        fetcher,
        identifiers: (0..<5).map { "photo-\($0)" },
        size: CGSize(width: 128, height: 128),
        store: store)
      #expect(
        imageDimensions(recorder.result("photo-0")?.data)
          == CGSize(width: 16, height: 12))
      #expect(hydration.requests.count == 5)
      #expect(hydration.concurrentHighWater == 2)
    }
  }

  @Test("A transient failure retries cleanly after provider relaunch")
  func retryAfterRelaunch() async throws {
    try await withFetchScratchDirectory { scratch in
      let store = ScriptedStore(account: thumbnailAccount)
      store.apply(thumbnailItem(id: "photo"))
      let hydration = ScriptedHydration()
      hydration.enqueueTransportFailure(.agentUnavailable(path: "/agent.sock"))
      let first = await runThumbnailFetch(
        ThumbnailFetcher(hydration: hydration), identifiers: ["photo"],
        size: CGSize(width: 64, height: 64), store: store)
      let firstError = first.1.result("photo")?.error as NSError?
      #expect(firstError?.domain == NSFileProviderError.errorDomain)
      #expect(firstError?.code == NSFileProviderError.Code.serverUnreachable.rawValue)

      let preview = try pngFixture(width: 16, height: 12)
      let staged = try stageContent(preview, in: scratch, name: "retry.png")
      hydration.enqueueSuccess(
        HydratedContent(
          stagedPath: staged, contentVersion: "v1",
          byteCount: UInt64(preview.count), mimeType: "image/png"))
      let second = await runThumbnailFetch(
        ThumbnailFetcher(hydration: hydration), identifiers: ["photo"],
        size: CGSize(width: 64, height: 64), store: store)
      #expect(
        imageDimensions(second.1.result("photo")?.data)
          == CGSize(width: 16, height: 12))
      #expect(hydration.requests.map(\.purpose) == [.thumbnail, .thumbnail])
    }
  }

  @Test("Metadata enumeration has no hydration seam or eager preview side effect")
  func enumerationDoesNotHydrate() {
    let store = ScriptedStore(account: thumbnailAccount)
    store.apply(
      ItemMetadata(
        contractVersion: 2,
        id: "root", parent: nil, kind: .account, isDirectory: true,
        displayName: "Test Account", safeName: "Test Account", metadataVersion: "m1",
        mimeType: nil, logicalSize: nil, attachmentLogicalKind: nil,
        attachmentRepresentation: nil, attachmentFidelity: nil,
        attachmentSourceName: nil, attachmentExactSize: nil, contentVersion: nil,
        availability: .fetchable, createdAtMs: nil, modifiedAtMs: nil, deletedAtMs: nil))
    store.apply(thumbnailItem(id: "photo", parent: "root"))
    let hydration = ScriptedHydration()
    let observer = RecordingEnumerationObserver()

    GramDriveEnumerator(
      store: store, accountId: 7, container: .rootContainer
    ).enumerateItems(
      for: observer,
      startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))

    #expect(observer.finishError == nil)
    #expect(observer.enumeratedIdentifiers == ["photo"])
    #expect(hydration.requests.isEmpty)
  }
}
