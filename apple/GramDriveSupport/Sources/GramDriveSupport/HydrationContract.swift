import Foundation

/// The wire contract of the agent's hydration endpoint — the second, and
/// still narrow, agent IPC channel (TASK-260715-kkglhx; PLAT-MAC-002's
/// "narrow native service", DEC-006).
///
/// The File Provider extension never runs TDLib or the engine; when the
/// system asks it for bytes it does not have, it asks the one process that
/// owns transfers — the companion agent — over this channel. The channel is
/// control-plane only: the request names an item and pins a content
/// version, the reply streams progress and ends in exactly one terminal
/// event, and the bytes themselves never cross the socket. Verified content
/// is staged by the engine inside the shared container; the terminal `done`
/// event carries the staged file's path, and the extension clones it out.
/// That keeps memory bounded on both sides by construction (NFR-021): no
/// process ever holds file content in memory to move it.
///
/// Bounded in every dimension, like the health channel: one request per
/// connection, one JSON line each way (size-capped), a fixed three-event
/// vocabulary, and cancellation with no verb at all — the client closing
/// its end *is* the cancel, observed by the server as EOF. There is no
/// other client-to-server traffic to parse, so the request line stays the
/// entire request-handling attack surface.
///
/// Both processes derive the socket's location from the shared data root by
/// the same rule, exactly as they derive the shared-state layout.
public enum HydrationContract {
    /// Version of this wire contract. The request carries it; a server
    /// refuses a request from a different major (`internal` failure), which
    /// can only happen across a mismatched install.
    public static let protocolVersion = 1

    /// Size cap on the single request line. A request is an identifier and
    /// a version token; kilobytes mean a bug.
    public static let maxRequestLineBytes = 16 * 1024

    /// Size cap on any single event line. Events carry counters, category
    /// strings, and one path; kilobytes mean a bug.
    public static let maxEventLineBytes = 64 * 1024

    /// The hydration endpoint's UNIX socket under a shared data root:
    /// `<root>/agent/hydration.sock` — beside the agent's health socket
    /// (`AgentRuntimeLayout` derives its paths from the same rule).
    public static func socketURL(dataRoot: URL) -> URL {
        dataRoot
            .appendingPathComponent("agent", isDirectory: true)
            .appendingPathComponent("hydration.sock", isDirectory: false)
    }
}

/// One hydration request: make this item's bytes materialized in the shared
/// container, pinned to the content version the requester observed.
public struct HydrationRequest: Codable, Equatable, Sendable {
    /// The requester's ``HydrationContract/protocolVersion``.
    public var protocolVersion: Int
    /// The account the item belongs to.
    public var accountId: Int64
    /// The item's stable core identifier (text form).
    public var itemId: String
    /// The content version the requester observed and pins (DOM-003).
    /// `nil` when the metadata carries no token yet; the engine then
    /// resolves the current version and reports it in `done`.
    public var contentVersion: String?

    public init(
        protocolVersion: Int = HydrationContract.protocolVersion,
        accountId: Int64,
        itemId: String,
        contentVersion: String?
    ) {
        self.protocolVersion = protocolVersion
        self.accountId = accountId
        self.itemId = itemId
        self.contentVersion = contentVersion
    }
}

/// A point-in-time progress report for one running hydration. Mirrors the
/// FFI `TransferProgress` shape so the engine-backed hydrator forwards its
/// accounting verbatim.
public struct HydrationProgress: Codable, Equatable, Sendable {
    /// Bytes staged so far; monotonically non-decreasing within one
    /// hydration.
    public var bytesTransferred: UInt64
    /// Total expected bytes, when known.
    public var bytesTotal: UInt64?

    public init(bytesTransferred: UInt64, bytesTotal: UInt64?) {
        self.bytesTransferred = bytesTransferred
        self.bytesTotal = bytesTotal
    }
}

/// The successful terminal event: verified content is fully staged in the
/// shared container.
///
/// The staged file is engine-owned cache content (SYNC-042: promoted
/// atomically, only after version and integrity checks). The server keeps
/// it valid at least until the connection closes; the client must copy or
/// clone it out before disconnecting and must never move, modify, or delete
/// it.
public struct HydratedContent: Codable, Equatable, Sendable {
    /// Absolute path of the staged, fully verified file inside the shared
    /// container.
    public var stagedPath: String
    /// The content version the staged bytes belong to (DOM-003). Equals the
    /// pinned request version whenever one was pinned.
    public var contentVersion: String?
    /// Exact byte count of the staged file; the client verifies its copy
    /// against it (never publish partial content — PRD-043).
    public var byteCount: UInt64

    public init(stagedPath: String, contentVersion: String?, byteCount: UInt64) {
        self.stagedPath = stagedPath
        self.contentVersion = contentVersion
        self.byteCount = byteCount
    }
}

/// Why a hydration failed, as a stable category the extension maps onto the
/// provider error surface. Mirrors the FFI `DriveError` categories
/// (NFR-030) plus the transfer-layer outcomes that have no `DriveError`
/// spelling: `versionConflict` (SYNC-042's stale-version refusal),
/// `restricted` (POL-4), and the agent-side admission refusals `draining`
/// and `busy`.
public enum HydrationFailureCategory: String, Codable, Equatable, Sendable {
    /// The account or item does not exist (or no longer exists).
    case notFound
    /// The source withholds the content (POL-4); bytes are never fetched.
    case restricted
    /// The pinned content version is no longer the item's current version;
    /// re-resolve and re-request (SYNC-042 — stale bytes are never
    /// published).
    case versionConflict
    /// The source has no usable authorization (re-authorize in the app).
    case authRequired
    /// The source is throttling; `retryAfterMs` when it stated a delay.
    case rateLimited
    /// The source cannot be reached right now.
    case sourceUnavailable
    /// Local persistence failed (state database or cache storage).
    case storage
    /// Staged content failed an integrity check (NFR-012).
    case integrity
    /// The hydration was cancelled.
    case cancelled
    /// The agent is shutting down; no new work is admitted.
    case draining
    /// The agent is at its concurrent-hydration bound; retry later.
    case busy
    /// A bug on the agent's side of the contract.
    case internalError = "internal"

    /// Decodes leniently: an unknown category (a newer agent) folds to
    /// ``internalError`` rather than failing the whole event — the
    /// extension's mapping treats both as "report and retry later".
    public init(from decoder: Decoder) throws {
        let raw = try decoder.singleValueContainer().decode(String.self)
        self = Self(rawValue: raw) ?? .internalError
    }
}

/// The failing terminal event.
public struct HydrationFailure: Error, Codable, Equatable, Sendable {
    /// The stable category the client branches on.
    public var category: HydrationFailureCategory
    /// Diagnostic text for logs; never contractual, never parsed.
    public var detail: String
    /// Source-stated minimum backoff, for ``HydrationFailureCategory/rateLimited``.
    public var retryAfterMs: UInt64?

    public init(category: HydrationFailureCategory, detail: String, retryAfterMs: UInt64? = nil) {
        self.category = category
        self.detail = detail
        self.retryAfterMs = retryAfterMs
    }
}

/// One server-to-client event line: zero or more `progress`, then exactly
/// one terminal `done` or `failure`.
public enum HydrationEvent: Equatable, Sendable {
    case progress(HydrationProgress)
    case done(HydratedContent)
    case failure(HydrationFailure)
}

extension HydrationEvent: Codable {
    private enum CodingKeys: String, CodingKey {
        case event, progress, content, failure
    }

    private enum Kind: String, Codable {
        case progress, done, failure
    }

    public init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        switch try container.decode(Kind.self, forKey: .event) {
        case .progress:
            self = .progress(try container.decode(HydrationProgress.self, forKey: .progress))
        case .done:
            self = .done(try container.decode(HydratedContent.self, forKey: .content))
        case .failure:
            self = .failure(try container.decode(HydrationFailure.self, forKey: .failure))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .progress(let progress):
            try container.encode(Kind.progress, forKey: .event)
            try container.encode(progress, forKey: .progress)
        case .done(let content):
            try container.encode(Kind.done, forKey: .event)
            try container.encode(content, forKey: .content)
        case .failure(let failure):
            try container.encode(Kind.failure, forKey: .event)
            try container.encode(failure, forKey: .failure)
        }
    }
}

/// Line framing shared by both ends: one JSON document per `\n`-terminated
/// line, decoded under a caller-stated size cap.
public enum HydrationWire {
    /// Encodes one value as a single line (JSON + `\n`). JSON string
    /// escaping guarantees no interior newline.
    public static func encodeLine<T: Encodable>(_ value: T) throws -> Data {
        var data = try JSONEncoder().encode(value)
        data.append(0x0A)
        return data
    }

    /// Decodes one line's payload (without the terminator).
    public static func decodeLine<T: Decodable>(_ type: T.Type, from data: Data) throws -> T {
        try JSONDecoder().decode(type, from: data)
    }
}
