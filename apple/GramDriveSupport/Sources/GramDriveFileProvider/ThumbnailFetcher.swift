import CoreGraphics
import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport
import ImageIO
import UniformTypeIdentifiers

/// Separately bounded preview fetches for Finder's thumbnail cache.
///
/// Enumeration never touches this type. Each request first checks durable
/// metadata, then asks the agent for the source's dedicated preview operation;
/// it never falls back to full-content hydration.
final class ThumbnailFetcher: @unchecked Sendable {
  typealias Context = (account: AccountInfo, store: any SharedStateStoreProtocol)
  typealias PerItemCompletion =
    @Sendable (
      NSFileProviderItemIdentifier, Data?, (any Error)?
    ) -> Void
  typealias Completion = @Sendable ((any Error)?) -> Void

  struct Configuration: Sendable {
    var maxConcurrentFetches = 2
    var maxItemsPerRequest = 64
    var maxDimensionPx: UInt32 = 512
    var maxEncodedBytes: UInt64 = 4 * 1024 * 1024
  }

  private let hydration: any HydrationRequesting
  private let configuration: Configuration
  private let telemetry: any ProviderFetchObserving
  private let gate: FetchGate
  private let lock = NSLock()
  private var tasks: [UUID: Task<Void, Never>] = [:]

  init(
    hydration: any HydrationRequesting,
    configuration: Configuration = Configuration(),
    telemetry: any ProviderFetchObserving = ProviderFetchTelemetry()
  ) {
    self.hydration = hydration
    self.configuration = configuration
    self.telemetry = telemetry
    self.gate = FetchGate(width: configuration.maxConcurrentFetches)
  }

  func fetchThumbnails(
    itemIdentifiers: [NSFileProviderItemIdentifier],
    requestedSize: CGSize,
    context: @escaping @Sendable () throws -> Context,
    perItemCompletion: @escaping PerItemCompletion,
    completion: @escaping Completion
  ) -> Progress {
    let progress = Progress(totalUnitCount: Int64(itemIdentifiers.count))
    progress.isCancellable = true
    guard itemIdentifiers.count <= configuration.maxItemsPerRequest else {
      for identifier in itemIdentifiers {
        record(
          identifier: identifier,
          startedAt: DispatchTime.now().uptimeNanoseconds,
          classification: ProviderFetchTelemetry.classification(
            for: NSFileProviderError(.cannotSynchronize)))
      }
      completion(NSFileProviderError(.cannotSynchronize))
      return progress
    }

    let id = UUID()
    // Task creation and registration are one locked operation. An
    // immediate no-thumbnail/refusal result may run concurrently, but its
    // `forget` cannot pass this lock before the entry has been installed.
    lock.lock()
    let task = Task(priority: .userInitiated) { [weak self] in
      guard let self else {
        completion(NSFileProviderError(.serverUnreachable))
        return
      }
      await withTaskGroup(of: Void.self) { group in
        for identifier in itemIdentifiers {
          group.addTask {
            let startedAt = DispatchTime.now().uptimeNanoseconds
            do {
              let data = try await self.fetchOne(
                identifier: identifier,
                requestedSize: requestedSize,
                context: context)
              self.record(
                identifier: identifier,
                startedAt: startedAt,
                classification: data == nil
                  ? Self.noThumbnailClassification
                  : ProviderFetchTelemetry.classification(for: nil))
              perItemCompletion(identifier, data, nil)
            } catch let observed as ObservedProviderFetchError {
              self.record(
                identifier: identifier,
                startedAt: startedAt,
                classification: observed.classification)
              perItemCompletion(identifier, nil, observed.providerError)
            } catch is CancellationError {
              let error = CocoaError(.userCancelled)
              self.record(
                identifier: identifier,
                startedAt: startedAt,
                classification: ProviderFetchTelemetry.classification(for: error))
              perItemCompletion(identifier, nil, error)
            } catch {
              self.record(
                identifier: identifier,
                startedAt: startedAt,
                classification: ProviderFetchTelemetry.classification(for: error))
              perItemCompletion(identifier, nil, error)
            }
            progress.completedUnitCount += 1
          }
        }
        await group.waitForAll()
      }
      let terminalError: (any Error)? =
        Task.isCancelled ? CocoaError(.userCancelled) : nil
      self.forget(id)
      completion(terminalError)
    }
    tasks[id] = task
    lock.unlock()
    progress.cancellationHandler = { task.cancel() }
    return progress
  }

  func cancelAll() {
    lock.lock()
    let active = Array(tasks.values)
    lock.unlock()
    for task in active {
      task.cancel()
    }
  }

  var inFlightCount: Int {
    lock.lock()
    defer { lock.unlock() }
    return tasks.count
  }

  private func fetchOne(
    identifier: NSFileProviderItemIdentifier,
    requestedSize: CGSize,
    context: @escaping @Sendable () throws -> Context
  ) async throws -> Data? {
    try await gate.acquire()
    defer { gate.release() }
    try Task.checkCancellation()

    // File Provider can ask for previews while a crawl owns every
    // cooperative worker. Context construction and metadata lookup may
    // synchronously enter UniFFI/SQLite, so they share content fetch's
    // explicit Finder-priority dispatch boundary.
    let (account, store) = try await FileProviderDemandExecutor.run(context)
    let coreId = ItemIdentifierMapping.coreItemId(
      for: identifier, accountRootId: account.rootItemId)
    let metadata = try await FileProviderDemandExecutor.run {
      try self.liveItem(in: store, id: coreId)
    }
    guard Self.isThumbnailEligible(metadata) else { return nil }
    switch metadata.availability {
    case .fetchable:
      break
    case .restricted:
      throw CocoaError(.fileReadNoPermission)
    case .unavailable:
      // A thumbnail source can be temporarily unavailable while the
      // durable attachment remains live. Do not make Finder forget it.
      throw NSFileProviderError(.serverUnreachable)
    }

    let width = Self.bound(requestedSize.width, maximum: configuration.maxDimensionPx)
    let height = Self.bound(requestedSize.height, maximum: configuration.maxDimensionPx)
    do {
      let content = try await hydration.hydrate(
        HydrationRequest(
          accountId: account.accountId,
          itemId: metadata.id,
          contentVersion: metadata.contentVersion,
          purpose: .thumbnail,
          maxWidthPx: width,
          maxHeightPx: height),
        onProgress: { _ in })
      guard content.isAvailable else { return nil }
      if let pinned = metadata.contentVersion,
        let staged = content.contentVersion,
        pinned != staged
      {
        throw NSFileProviderError(.serverUnreachable)
      }
      guard content.byteCount > 0,
        content.byteCount <= configuration.maxEncodedBytes,
        content.mimeType?.lowercased().hasPrefix("image/") == true
      else {
        throw NSFileProviderError(.cannotSynchronize)
      }
      let byteCount = content.byteCount
      let stagedPath = content.stagedPath
      let data = try await FileProviderDemandExecutor.run {
        let url = URL(fileURLWithPath: stagedPath, isDirectory: false)
        let attributes = try FileManager.default.attributesOfItem(atPath: url.path)
        guard (attributes[.size] as? NSNumber)?.uint64Value == byteCount else {
          throw NSFileProviderError(.cannotSynchronize)
        }
        let data = try Data(contentsOf: url, options: .mappedIfSafe)
        guard UInt64(data.count) == byteCount else {
          throw NSFileProviderError(.cannotSynchronize)
        }
        return data
      }
      let maxEncodedBytes = configuration.maxEncodedBytes
      return try await FileProviderDemandExecutor.run {
        try Self.boundedImageData(
          data,
          maxWidthPx: width,
          maxHeightPx: height,
          maxEncodedBytes: maxEncodedBytes)
      }
    } catch let failure as HydrationFailure {
      throw ContentFetcher.observedProviderError(for: failure)
    } catch let transport as HydrationTransportError {
      throw ObservedProviderFetchError(
        providerError: NSFileProviderError(.serverUnreachable),
        classification: ProviderFetchTelemetry.classification(for: transport))
    } catch let socketError as UnixSocketError {
      throw ObservedProviderFetchError(
        providerError: NSFileProviderError(.serverUnreachable),
        classification: ProviderFetchTelemetry.classification(for: socketError))
    } catch is CancellationError {
      throw CancellationError()
    } catch let error as NSFileProviderError {
      throw error
    } catch let error as CocoaError
      where error.code == .fileReadNoSuchFile || error.code == .fileNoSuchFile
    {
      throw NSFileProviderError(.serverUnreachable)
    } catch {
      throw NSFileProviderError(.cannotSynchronize)
    }
  }

  private func liveItem(
    in store: any SharedStateStoreProtocol,
    id: String
  ) throws -> ItemMetadata {
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

  static func isThumbnailEligible(_ metadata: ItemMetadata) -> Bool {
    guard !metadata.isDirectory, metadata.kind == .attachment else { return false }
    switch metadata.attachmentLogicalKind?.lowercased() {
    case "photo", "video", "animation", "sticker":
      return true
    default:
      let mime = metadata.mimeType?.lowercased() ?? ""
      return mime.hasPrefix("image/") || mime.hasPrefix("video/")
    }
  }

  static func bound(_ value: CGFloat, maximum: UInt32) -> UInt32 {
    guard maximum > 0 else { return 0 }
    guard value.isFinite, value > 0 else { return 1 }
    // Clamp while still in CGFloat space. Converting an otherwise valid
    // finite request above UInt32.max before clamping traps at runtime.
    let rounded = value.rounded(.up)
    guard rounded < CGFloat(maximum) else { return maximum }
    return UInt32(rounded)
  }

  /// Validates the staged payload as an Image-I/O image and produces one
  /// frame that fits the requested pixel box. Source descriptors are used
  /// to avoid pointless downloads, but the encoded bytes are authoritative:
  /// this second bound prevents stale or corrupt dimensions from escaping
  /// through Finder's per-item completion handler.
  static func boundedImageData(
    _ data: Data,
    maxWidthPx: UInt32,
    maxHeightPx: UInt32,
    maxEncodedBytes: UInt64
  ) throws -> Data {
    guard !data.isEmpty,
      UInt64(data.count) <= maxEncodedBytes,
      maxWidthPx > 0,
      maxHeightPx > 0,
      let source = CGImageSourceCreateWithData(data as CFData, nil),
      CGImageSourceGetCount(source) > 0
    else {
      throw NSFileProviderError(.cannotSynchronize)
    }

    // Image I/O accepts one maximum side rather than a rectangular box.
    // Using the smaller requested side is conservative for every aspect
    // ratio and orientation; the explicit postcondition below remains the
    // authority in case a decoder disregards the hint.
    let maximumSide = Int(min(maxWidthPx, maxHeightPx))
    let options: [CFString: Any] = [
      kCGImageSourceCreateThumbnailFromImageAlways: true,
      kCGImageSourceCreateThumbnailWithTransform: true,
      kCGImageSourceThumbnailMaxPixelSize: maximumSide,
      kCGImageSourceShouldCache: false,
    ]
    guard
      let image = CGImageSourceCreateThumbnailAtIndex(
        source, 0, options as CFDictionary),
      image.width > 0,
      image.height > 0,
      image.width <= Int(maxWidthPx),
      image.height <= Int(maxHeightPx)
    else {
      throw NSFileProviderError(.cannotSynchronize)
    }

    let output = NSMutableData()
    guard
      let destination = CGImageDestinationCreateWithData(
        output, UTType.png.identifier as CFString, 1, nil)
    else {
      throw NSFileProviderError(.cannotSynchronize)
    }
    CGImageDestinationAddImage(destination, image, nil)
    guard CGImageDestinationFinalize(destination) else {
      throw NSFileProviderError(.cannotSynchronize)
    }
    let bounded = output as Data
    guard !bounded.isEmpty, UInt64(bounded.count) <= maxEncodedBytes else {
      throw NSFileProviderError(.cannotSynchronize)
    }
    return bounded
  }

  private func forget(_ id: UUID) {
    lock.lock()
    tasks.removeValue(forKey: id)
    lock.unlock()
  }

  /// One thumbnail item is one File Provider callback outcome. The item
  /// identifier remains only in the stable log token; aggregate health
  /// receives no token or source-derived value.
  private func record(
    identifier: NSFileProviderItemIdentifier,
    startedAt: UInt64,
    classification: ProviderFetchClassification
  ) {
    let elapsedMs = (DispatchTime.now().uptimeNanoseconds - startedAt) / 1_000_000
    telemetry.record(
      ProviderFetchTelemetryRecord(
        callback: "fetchThumbnails",
        itemToken: ProviderFetchTelemetry.itemToken(for: identifier),
        outcome: classification.outcome,
        retryable: classification.retryable,
        elapsedMs: elapsedMs,
        engineFailure: classification.engineFailure,
        providerMapping: classification.providerMapping,
        noSuchItem: classification.noSuchItem))
  }

  private static let noThumbnailClassification = ProviderFetchClassification(
    outcome: "no-thumbnail",
    retryable: false,
    engineFailure: false,
    providerMapping: false,
    noSuchItem: false)
}
