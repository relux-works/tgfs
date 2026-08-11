import Foundation

/// Shared location/version of the agent control endpoint. The full command
/// vocabulary stays in `GramDriveAgentCore`; this small common definition lets
/// the thin File Provider send one scheduling hint without linking agent code.
public enum AgentControlEndpoint {
    public static let protocolVersion = 1

    public static func socketURL(dataRoot: URL) -> URL {
        dataRoot
            .appendingPathComponent("agent", isDirectory: true)
            .appendingPathComponent("control.sock", isDirectory: false)
    }
}

/// Foreground importance of one chat's resumable history crawl.
public enum HistoryPriorityHint: String, Codable, Equatable, Sendable {
    case background
    case requested
    case visible
}

/// One privacy-safe scheduling hint from File Provider to the owned agent.
public struct HistoryPriorityRequest: Codable, Equatable, Sendable {
    public var accountId: Int64
    public var chatId: Int64
    public var priority: HistoryPriorityHint

    public init(accountId: Int64, chatId: Int64, priority: HistoryPriorityHint) {
        self.accountId = accountId
        self.chatId = chatId
        self.priority = priority
    }
}

/// Non-blocking hint seam used by provider callbacks.
public protocol HistoryPrioritySignaling: Sendable {
  func signal(_ request: HistoryPriorityRequest)
}

/// Aggregate-only telemetry from one File Provider fetch callback.
///
/// `itemToken` and all user-derived data are intentionally absent. The token
/// belongs to the signed-process log only; durable health retains counts.
public struct ProviderFetchHealthReport: Codable, Equatable, Sendable {
  public var succeeded: Bool
  public var engineFailure: Bool
  public var providerMapping: Bool
  public var noSuchItem: Bool
  public var retryable: Bool
  public var observedAtMs: Int64

  public init(
    succeeded: Bool,
    engineFailure: Bool,
    providerMapping: Bool,
    noSuchItem: Bool,
    retryable: Bool,
    observedAtMs: Int64
  ) {
    self.succeeded = succeeded
    self.engineFailure = engineFailure
    self.providerMapping = providerMapping
    self.noSuchItem = noSuchItem
    self.retryable = retryable
    self.observedAtMs = observedAtMs
  }
}

/// Best-effort sink for aggregate provider callback health.
public protocol ProviderFetchHealthSignaling: Sendable {
  func signal(_ report: ProviderFetchHealthReport)
}

/// Wire-compatible subset of the agent's first control request line.
struct HistoryPriorityControlRequest: Encodable, Sendable {
    var protocolVersion = AgentControlEndpoint.protocolVersion
    var operation = "historyPriority"
    var historyPriority: HistoryPriorityRequest
}

/// Wire-compatible request sent by the File Provider's non-blocking health
/// telemetry client.
struct ProviderFetchHealthControlRequest: Encodable, Sendable {
  var protocolVersion = AgentControlEndpoint.protocolVersion
  var operation = "providerFetchHealth"
  var providerFetchHealth: ProviderFetchHealthReport
}
