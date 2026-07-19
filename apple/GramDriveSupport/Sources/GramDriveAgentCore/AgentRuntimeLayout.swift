import Foundation

/// Host-owned runtime files of the companion agent, beside — never inside —
/// the core-owned shared-state layout (`sharedStateLayout` fixes
/// `state/`, `cache/`; this type fixes `agent/`).
///
/// Everything here is process-coordination plumbing the engine never reads:
/// the single-instance lock, the health socket, and the agent settings.
/// Deriving all of it from the same data root keeps the rule of the shared
/// container intact: every GramDrive process computes identical paths from
/// identical inputs.
public struct AgentRuntimeLayout: Equatable, Sendable {
    /// The data root the layout was derived from (the same root handed to
    /// the core's `sharedStateLayout`).
    public let dataRoot: URL

    /// Directory of the agent's runtime files: `<root>/agent`.
    public var agentDirectory: URL {
        dataRoot.appendingPathComponent("agent", isDirectory: true)
    }

    /// The single-instance lock file: `<root>/agent/agent.lock`.
    ///
    /// The lock is an OS `flock`, so a crashed or killed agent releases it
    /// with its last file descriptor — no stale-lock cleanup exists, by
    /// design.
    public var lockFile: URL {
        agentDirectory.appendingPathComponent("agent.lock", isDirectory: false)
    }

    /// The health endpoint's UNIX socket: `<root>/agent/health.sock`.
    public var healthSocket: URL {
        agentDirectory.appendingPathComponent("health.sock", isDirectory: false)
    }

    /// The agent settings document: `<root>/agent/settings.json`.
    public var settingsFile: URL {
        agentDirectory.appendingPathComponent("settings.json", isDirectory: false)
    }

    public init(dataRoot: URL) {
        self.dataRoot = dataRoot
    }

    /// Creates the agent directory (and the data root above it) if missing.
    public func ensureDirectories(fileManager: FileManager = .default) throws {
        try fileManager.createDirectory(at: agentDirectory, withIntermediateDirectories: true)
    }
}
