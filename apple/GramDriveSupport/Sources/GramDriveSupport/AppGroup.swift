import Foundation

/// The shared App Group container identity and the data-root rule every
/// GramDrive process derives its paths from (DEC-019 / POL-7;
/// `.spec/platform-requirements.md` § Identifier and naming convention).
///
/// The container root is Apple's; the *data root* handed to the core is a
/// fixed subdirectory of it. Everything below the data root — state
/// database, quarantine, managed cache — is fixed by the core's
/// `sharedStateLayout` so the app, the agent, and the File Provider
/// extension can never disagree about where shared files live.
public enum AppGroup {
    /// The team-prefixed App Group identifier — the entitlement form v1
    /// ships (macOS 14 deployment target, Developer ID signing; needs no
    /// portal registration). The `group.`-prefixed form applies only once
    /// iOS or macOS 15+ enters scope.
    public static let identifier = "262RZ595FP.com.reluxworks.gramdrive"

    /// Resolves the shared container root for ``identifier``.
    ///
    /// Throws ``AppGroupError/containerUnavailable(identifier:)`` when the
    /// system cannot provide the container — for a properly signed and
    /// entitled GramDrive process that is a configuration bug, not a
    /// runtime state to handle.
    public static func containerURL(fileManager: FileManager = .default) throws -> URL {
        guard
            let url = fileManager.containerURL(
                forSecurityApplicationGroupIdentifier: identifier
            )
        else {
            throw AppGroupError.containerUnavailable(identifier: identifier)
        }
        return url
    }

    /// The data root inside a container root: `Library/Application
    /// Support/GramDrive`.
    ///
    /// `Application Support` is the durable, never-system-purged location
    /// macOS pre-creates inside group containers; the managed content
    /// cache lives under this root too (not `Library/Caches`), because the
    /// engine's quota accounting owns that space and a system cache purge
    /// would silently invalidate it.
    ///
    /// Split from ``containerURL(fileManager:)`` so tests and tooling can
    /// apply the same rule to a substitute container.
    public static func dataRootURL(containerURL: URL) -> URL {
        containerURL
            .appendingPathComponent("Library", isDirectory: true)
            .appendingPathComponent("Application Support", isDirectory: true)
            .appendingPathComponent("GramDrive", isDirectory: true)
    }
}

/// Why the App Group container could not be resolved.
public enum AppGroupError: Error, Equatable {
    /// The system returned no container for the identifier — the process
    /// is missing its App Groups entitlement or is not signed as a
    /// GramDrive process.
    case containerUnavailable(identifier: String)
}
