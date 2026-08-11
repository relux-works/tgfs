import Foundation

/// Durable, host-owned agent preferences.
///
/// These are *lifecycle* preferences, not drive state: the engine's SQLite
/// database is written by the engine in-process only (DEC-006), so the
/// host's own knobs live in a small JSON document under the agent runtime
/// directory instead. The app writes it, the agent reads it; atomic
/// replacement keeps a concurrent reader from ever seeing a half-written
/// document.
///
/// Decoding tolerates a missing key as its default, the same additive-
/// evolution rule the FFI contract and health payload follow: a settings
/// document written by an older shell (only `launchAtLogin`, or none of the
/// keys) still loads, so a shell update never orphans an agent's settings
/// and vice versa.
public struct AgentSettings: Codable, Equatable, Sendable {
  /// POL-2 / DEC-014 default managed-cache quota: 10 GB (base-10, the unit
  /// the product promise is stated in). Unpinned content is evicted LRU to
  /// stay under it; pinned content and Archive Mode are quota-exempt.
  public static let defaultCacheQuotaBytes: UInt64 = 10_000_000_000

  /// Whether the user wants the agent registered as a login item.
  ///
  /// Off by default: registering a background item without the user's
  /// explicit choice would not be honoring a preference, it would be
  /// inventing one.
  public var launchAtLogin: Bool

  /// Managed-cache quota in bytes (POL-2). The app owns this knob; the
  /// engine's cache/eviction work (TASK-260715-11abx8) reads it. Carried
  /// durably here whether or not the engine acts on it yet — an honest
  /// preference the user set, not a value invented by whoever reads it.
  public var cacheQuotaBytes: UInt64

  /// Legacy pre-policy-channel Archive preference.
  ///
  /// Retained only so settings written by older companions round-trip
  /// without loss. Current per-account Archive Mode is authoritative engine
  /// state changed through the control channel; new UI must not use this
  /// field as its displayed or requested state.
  public var archiveModeEnabled: Bool

  public init(
    launchAtLogin: Bool = false,
    cacheQuotaBytes: UInt64 = AgentSettings.defaultCacheQuotaBytes,
    archiveModeEnabled: Bool = false
  ) {
    self.launchAtLogin = launchAtLogin
    self.cacheQuotaBytes = cacheQuotaBytes
    self.archiveModeEnabled = archiveModeEnabled
  }

  private enum CodingKeys: String, CodingKey {
    case launchAtLogin
    case cacheQuotaBytes
    case archiveModeEnabled
  }

  public init(from decoder: Decoder) throws {
    let container = try decoder.container(keyedBy: CodingKeys.self)
    self.launchAtLogin =
      try container.decodeIfPresent(Bool.self, forKey: .launchAtLogin) ?? false
    self.cacheQuotaBytes =
      try container.decodeIfPresent(UInt64.self, forKey: .cacheQuotaBytes)
      ?? AgentSettings.defaultCacheQuotaBytes
    self.archiveModeEnabled =
      try container.decodeIfPresent(Bool.self, forKey: .archiveModeEnabled) ?? false
  }
}

/// Loads and stores ``AgentSettings`` at a fixed file location.
public struct AgentSettingsStore: Sendable {
  /// The settings document location (``AgentRuntimeLayout/settingsFile``).
  public let fileURL: URL

  public init(fileURL: URL) {
    self.fileURL = fileURL
  }

  /// Reads the settings; a missing file is the defaults, not an error.
  /// A present-but-undecodable file throws — the caller decides whether
  /// to fall back to defaults, and with what visibility.
  public func load() throws -> AgentSettings {
    let data: Data
    do {
      data = try Data(contentsOf: fileURL)
    } catch let error as NSError
      where error.domain == NSCocoaErrorDomain
      && error.code == NSFileReadNoSuchFileError
    {
      return AgentSettings()
    }
    return try JSONDecoder().decode(AgentSettings.self, from: data)
  }

  /// Writes the settings atomically (write-to-temporary, then rename), so
  /// a reader in another process sees either the old document or the new
  /// one, never a torn one.
  public func save(_ settings: AgentSettings) throws {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
    let data = try encoder.encode(settings)
    try data.write(to: fileURL, options: [.atomic])
  }
}
