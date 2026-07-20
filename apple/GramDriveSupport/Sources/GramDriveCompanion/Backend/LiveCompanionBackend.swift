import Foundation
import GramDriveAgentCore
import GramDriveSupport

/// The production backend over the agent's IPC surface (BUG-260720-3i74u1).
///
/// Reads: health over the bounded socket, settings over the durable
/// document. Commands — authorization, repair, account removal — run over
/// the live control channel: the backend first *ensures the agent is
/// running* (``AgentEnsurer``: probe, then start via `SMAppService` or a
/// direct spawn per the launch-at-login preference, then a bounded
/// readiness wait), then speaks ``ControlContract``. A command against an
/// agent that cannot be started reports
/// ``ControlChannelUnavailable/agentNotRunning`` — `notWired` is not a
/// state this backend can produce.
public struct LiveCompanionBackend: CompanionBackend {
    private let layout: AgentRuntimeLayout
    private let healthTimeout: Duration
    private let ensurer: AgentEnsurer
    /// The app half of the SEC-004 removal: File Provider domain
    /// deregistration, which can only run in the app that embeds the
    /// extension. Injected by the executable (`CompanionMain`); `nil` in
    /// harnesses that assemble no File Provider stack.
    private let accountDomainCleanup: (@Sendable (Int64) async -> Void)?

    /// Builds a backend over an explicit agent runtime layout (the App
    /// Group data root in production, a substitute root for tools/tests).
    public init(
        layout: AgentRuntimeLayout,
        healthTimeout: Duration = .seconds(5),
        starter: (any AgentStarting)? = nil,
        startupTimeout: Duration = .seconds(15),
        accountDomainCleanup: (@Sendable (Int64) async -> Void)? = nil
    ) {
        self.layout = layout
        self.healthTimeout = healthTimeout
        self.accountDomainCleanup = accountDomainCleanup
        let settingsFile = layout.settingsFile
        let probeTimeout: Duration = .seconds(1)
        let socketURL = layout.healthSocket
        self.ensurer = AgentEnsurer(
            probe: {
                await Self.health(socketURL: socketURL, timeout: probeTimeout)
            },
            starter: starter ?? BundledAgentStarter(),
            loginItemPreferred: {
                (try? AgentSettingsStore(fileURL: settingsFile).load().launchAtLogin) ?? false
            },
            startupTimeout: startupTimeout)
    }

    /// Builds a backend over the App Group container's data root. Throws if
    /// the container cannot be resolved (missing entitlement, sandbox).
    public init(
        healthTimeout: Duration = .seconds(5),
        accountDomainCleanup: (@Sendable (Int64) async -> Void)? = nil
    ) throws {
        let dataRoot = AppGroup.dataRootURL(containerURL: try AppGroup.containerURL())
        self.init(
            layout: AgentRuntimeLayout(dataRoot: dataRoot),
            healthTimeout: healthTimeout,
            accountDomainCleanup: accountDomainCleanup)
    }

    // MARK: - Reads

    public func fetchHealth() async -> HealthReadout {
        await Self.health(socketURL: layout.healthSocket, timeout: healthTimeout)
    }

    private static func health(socketURL: URL, timeout: Duration) async -> HealthReadout {
        // The health read is a blocking socket round-trip; keep it off the
        // caller's thread (the UI's main actor in production).
        await withCheckedContinuation { continuation in
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
        // Ask a running agent to apply the new document; best-effort — an
        // agent that is not running reads the document at its next start.
        let controlSocket = layout.controlSocket
        DispatchQueue.global(qos: .utility).async {
            _ = try? ControlClient.command(
                ControlRequest(operation: .reloadSettings),
                socketURL: controlSocket,
                timeout: .seconds(5))
        }
    }

    // MARK: - Commands

    public func makeAuthorizationSession() -> any AuthorizationSession {
        let ensurer = self.ensurer
        let controlSocket = layout.controlSocket
        return LiveAuthorizationSession(openChannel: {
            if case .failed = await ensurer.ensureRunning() {
                return .unavailable(.agentNotRunning)
            }
            do {
                return .opened(try ControlAuthChannel.open(socketURL: controlSocket))
            } catch {
                return .unavailable(.agentNotRunning)
            }
        })
    }

    public func requestRepair() async -> CommandOutcome {
        // Repair probes every account's stored session; each probe is
        // bounded agent-side, so the command timeout scales generously.
        await command(ControlRequest(operation: .repair), timeout: .seconds(150))
    }

    public func removeAccount(_ confirmation: RemovalConfirmation) async -> CommandOutcome {
        guard confirmation.isValid else {
            return .failed(.invalidArgument)
        }
        if case .failed = await ensurer.ensureRunning() {
            return .unavailable(.agentNotRunning)
        }
        // Resolve which account: the durable rows the agent reports. V1 is
        // effectively single-account; a label match disambiguates several.
        guard case .running(let snapshot) = await fetchHealth() else {
            return .unavailable(.agentNotRunning)
        }
        guard let accounts = snapshot.accounts, !accounts.isEmpty else {
            return .failed(.notFound)
        }
        let target: AccountHealthSummary
        if accounts.count == 1 {
            target = accounts[0]
        } else if let matched = accounts.first(where: {
            $0.displayName.caseInsensitiveEquals(confirmation.accountLabel.trimmed)
        }) {
            target = matched
        } else {
            return .failed(.invalidArgument)
        }

        let outcome = await command(
            ControlRequest(
                operation: .removeAccount,
                removal: ControlRemovalRequest(
                    accountId: target.accountId, revokeSession: true)),
            ensureAgent: false,
            timeout: .seconds(180))
        guard case .completed = outcome else {
            return outcome
        }
        // The app half of SEC-004: deregister the account's File Provider
        // domain, after the engine dropped the canonical row.
        await accountDomainCleanup?(target.accountId)
        return .completed
    }

    // MARK: - Command plumbing

    private func command(
        _ request: ControlRequest,
        ensureAgent: Bool = true,
        timeout: Duration
    ) async -> CommandOutcome {
        if ensureAgent, case .failed = await ensurer.ensureRunning() {
            return .unavailable(.agentNotRunning)
        }
        let controlSocket = layout.controlSocket
        let event: ControlEvent
        do {
            event = try await withCheckedThrowingContinuation { continuation in
                DispatchQueue.global(qos: .utility).async {
                    continuation.resume(
                        with: Result {
                            try ControlClient.command(
                                request, socketURL: controlSocket, timeout: timeout)
                        })
                }
            }
        } catch ControlTransportError.agentUnavailable {
            return .unavailable(.agentNotRunning)
        } catch {
            return .unavailable(.dropped)
        }
        switch event {
        case .commandDone:
            return .completed
        case .commandFailed(let failure):
            return .failed(Self.commandFailure(failure))
        case .status, .settings, .authState, .authSubmitResult:
            return .unavailable(.dropped)
        }
    }

    static func commandFailure(_ failure: ControlCommandFailure) -> CommandFailure {
        switch failure.category {
        case .invalidArgument: return .invalidArgument
        case .notFound: return .notFound
        case .authRequired: return .authRequired
        case .rateLimited: return .rateLimited(retryAfterMs: failure.retryAfterMs)
        case .sourceUnavailable: return .sourceUnavailable
        case .storage: return .storage
        case .integrity: return .integrity
        case .cancelled: return .cancelled
        case .internalError: return .internalError
        }
    }
}
