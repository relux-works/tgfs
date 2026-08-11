import CryptoKit
import FileProvider
import Foundation
import GramDriveSupport
import OSLog

/// The one privacy-safe record emitted for each provider fetch item.
///
/// The stable token is a truncated SHA-256 digest of the opaque provider
/// identifier. It supports correlating retries in a signed-process log while
/// excluding the identifier itself, titles, filenames, account ids and bytes.
struct ProviderFetchTelemetryRecord: Equatable, Sendable {
  let callback: String
  let itemToken: String
  let outcome: String
  let retryable: Bool
  let elapsedMs: UInt64
  let engineFailure: Bool
  let providerMapping: Bool
  let noSuchItem: Bool
}

protocol ProviderFetchObserving: Sendable {
  func record(_ record: ProviderFetchTelemetryRecord)
}

struct ProviderFetchClassification: Equatable, Sendable {
  let outcome: String
  let retryable: Bool
  let engineFailure: Bool
  let providerMapping: Bool
  let noSuchItem: Bool
}

/// Production observer: logs redacted callback facts and forwards aggregate
/// health to the coordinator without an item token.
final class ProviderFetchTelemetry: ProviderFetchObserving, @unchecked Sendable {
  private let logger = Logger(
    subsystem: "com.reluxworks.gramdrive", category: "file-provider.fetch")
  private let health: (any ProviderFetchHealthSignaling)?

  init(health: (any ProviderFetchHealthSignaling)? = nil) {
    self.health = health
  }

  func record(_ record: ProviderFetchTelemetryRecord) {
    logger.notice("\(Self.logMessage(for: record), privacy: .public)")
    health?.signal(
      Self.healthReport(
        for: record,
        observedAtMs: Int64((Date().timeIntervalSince1970 * 1000).rounded())))
  }

  /// Exact signed-process payload, kept pure so privacy tests exercise the
  /// production rendering rather than a parallel test-only description.
  static func logMessage(for record: ProviderFetchTelemetryRecord) -> String {
    "callback=\(record.callback) token=\(record.itemToken) outcome=\(record.outcome) "
      + "retryable=\(record.retryable) elapsed_ms=\(record.elapsedMs)"
  }

  /// Aggregate-only durable payload. Identity is absent by construction.
  static func healthReport(
    for record: ProviderFetchTelemetryRecord,
    observedAtMs: Int64
  ) -> ProviderFetchHealthReport {
    ProviderFetchHealthReport(
      succeeded: record.outcome == "success",
      engineFailure: record.engineFailure,
      providerMapping: record.providerMapping,
      noSuchItem: record.noSuchItem,
      retryable: record.retryable,
      observedAtMs: observedAtMs)
  }

  static func itemToken(for identifier: NSFileProviderItemIdentifier) -> String {
    let digest = SHA256.hash(data: Data(identifier.rawValue.utf8))
    return "fp-" + digest.prefix(12).map { String(format: "%02x", $0) }.joined()
  }

  static func classification(for error: (any Error)?) -> ProviderFetchClassification {
    guard let error else {
      return ProviderFetchClassification(
        outcome: "success", retryable: false, engineFailure: false,
        providerMapping: false, noSuchItem: false)
    }
    if let failure = error as? HydrationFailure {
      let retryable =
        switch failure.category {
        case .notFound, .versionConflict, .rateLimited, .sourceUnavailable, .draining, .busy:
          true
        case .restricted, .authRequired, .cancelled,
          .storage, .integrity, .internalError:
          false
        }
      return ProviderFetchClassification(
        outcome: "engine-\(engineCategory(failure.category))", retryable: retryable,
        engineFailure: true, providerMapping: true,
        noSuchItem: false)
    }
    let nsError = error as NSError
    if nsError.domain == NSFileProviderError.errorDomain {
      if nsError.code == NSFileProviderError.Code.noSuchItem.rawValue {
        return ProviderFetchClassification(
          outcome: "no-such-item", retryable: false, engineFailure: false,
          providerMapping: true, noSuchItem: true)
      }
      if nsError.code == NSFileProviderError.Code.serverUnreachable.rawValue {
        return ProviderFetchClassification(
          outcome: "provider-retryable", retryable: true, engineFailure: false,
          providerMapping: true, noSuchItem: false)
      }
      return ProviderFetchClassification(
        outcome: "provider-error", retryable: false, engineFailure: false,
        providerMapping: true, noSuchItem: false)
    }
    if error is HydrationTransportError || error is UnixSocketError {
      return ProviderFetchClassification(
        outcome: "engine-transport", retryable: true, engineFailure: true,
        providerMapping: true, noSuchItem: false)
    }
    if let cocoa = error as? CocoaError, cocoa.code == .userCancelled {
      return ProviderFetchClassification(
        outcome: "cancelled", retryable: false, engineFailure: false,
        providerMapping: false, noSuchItem: false)
    }
    return ProviderFetchClassification(
      outcome: "provider-error", retryable: false, engineFailure: false,
      providerMapping: true, noSuchItem: false)
  }

  private static func engineCategory(_ category: HydrationFailureCategory) -> String {
    switch category {
    case .notFound: "not-found"
    case .restricted: "restricted"
    case .versionConflict: "version-conflict"
    case .authRequired: "auth-required"
    case .rateLimited: "rate-limited"
    case .sourceUnavailable: "source-unavailable"
    case .storage: "storage"
    case .integrity: "integrity"
    case .cancelled: "cancelled"
    case .draining: "draining"
    case .busy: "busy"
    case .internalError: "internal"
    }
  }
}
