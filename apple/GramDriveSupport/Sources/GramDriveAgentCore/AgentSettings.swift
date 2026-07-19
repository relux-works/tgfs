import Foundation

/// Durable, host-owned agent preferences.
///
/// These are *lifecycle* preferences, not drive state: the engine's SQLite
/// database is written by the engine in-process only (DEC-006), so the
/// host's own knobs live in a small JSON document under the agent runtime
/// directory instead. The app writes it, the agent reads it; atomic
/// replacement keeps a concurrent reader from ever seeing a half-written
/// document.
public struct AgentSettings: Codable, Equatable, Sendable {
    /// Whether the user wants the agent registered as a login item.
    ///
    /// Off by default: registering a background item without the user's
    /// explicit choice would not be honoring a preference, it would be
    /// inventing one.
    public var launchAtLogin: Bool

    public init(launchAtLogin: Bool = false) {
        self.launchAtLogin = launchAtLogin
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
