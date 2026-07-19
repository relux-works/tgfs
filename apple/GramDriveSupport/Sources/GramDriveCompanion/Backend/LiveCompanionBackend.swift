import Foundation
import GramDriveAgentCore
import GramDriveSupport

/// The production backend over what the agent exposes today.
///
/// Reads are real: health over the bounded socket, settings over the durable
/// document — both derived from the same agent runtime layout every GramDrive
/// process computes from the shared container. Commands (authorization,
/// repair, removal) report ``ControlChannelUnavailable/notWired`` until the
/// agent grows a control channel: the shell states that plainly instead of
/// faking a Telegram operation it cannot perform.
public struct LiveCompanionBackend: CompanionBackend {
    private let layout: AgentRuntimeLayout
    private let healthTimeout: Duration

    /// Builds a backend over an explicit agent runtime layout (the App
    /// Group data root in production, a substitute root for tools/tests).
    public init(layout: AgentRuntimeLayout, healthTimeout: Duration = .seconds(5)) {
        self.layout = layout
        self.healthTimeout = healthTimeout
    }

    /// Builds a backend over the App Group container's data root. Throws if
    /// the container cannot be resolved (missing entitlement, sandbox).
    public init(healthTimeout: Duration = .seconds(5)) throws {
        let dataRoot = AppGroup.dataRootURL(containerURL: try AppGroup.containerURL())
        self.init(layout: AgentRuntimeLayout(dataRoot: dataRoot), healthTimeout: healthTimeout)
    }

    public func fetchHealth() async -> HealthReadout {
        let socketURL = layout.healthSocket
        let timeout = healthTimeout
        // The health read is a blocking socket round-trip; keep it off the
        // caller's thread (the UI's main actor in production).
        return await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .utility).async {
                continuation.resume(
                    returning: Self.readHealth(socketURL: socketURL, timeout: timeout))
            }
        }
    }

    private static func readHealth(socketURL: URL, timeout: Duration) -> HealthReadout {
        do {
            let snapshot = try AgentHealthClient.fetch(socketURL: socketURL, timeout: timeout)
            return .running(snapshot)
        } catch AgentHealthClientError.agentUnavailable {
            return .notRunning
        } catch AgentHealthClientError.timedOut {
            return .timedOut
        } catch {
            return .error(String(describing: error))
        }
    }

    public func loadSettings() throws -> AgentSettings {
        try AgentSettingsStore(fileURL: layout.settingsFile).load()
    }

    public func saveSettings(_ settings: AgentSettings) throws {
        try layout.ensureDirectories()
        try AgentSettingsStore(fileURL: layout.settingsFile).save(settings)
    }

    public func makeAuthorizationSession() -> any AuthorizationSession {
        UnavailableAuthorizationSession(reason: .notWired)
    }

    public func requestRepair() async -> CommandOutcome {
        .unavailable(.notWired)
    }

    public func removeAccount(_ confirmation: RemovalConfirmation) async -> CommandOutcome {
        .unavailable(.notWired)
    }
}

/// An authorization session that has no channel to drive: `start` reports the
/// reason, its state stream is empty, and every input is unavailable. What
/// ``LiveCompanionBackend`` hands out until the agent control channel exists —
/// so the authorization screen renders an honest "unavailable" state rather
/// than a dead flow.
public struct UnavailableAuthorizationSession: AuthorizationSession {
    private let reason: ControlChannelUnavailable

    public init(reason: ControlChannelUnavailable) {
        self.reason = reason
    }

    public var states: AsyncStream<CompanionAuthState> {
        AsyncStream { $0.finish() }
    }

    public func start() async -> AuthStartResult { .unavailable(reason) }

    public func submit(_ input: CompanionAuthInput) async -> AuthSubmitResult {
        .unavailable(reason)
    }
}
