import Foundation
import GramDriveAgentCore

/// How the companion makes sure the agent process is up before opening the
/// control channel (BUG-260720-3i74u1): probe health; if nothing answers,
/// start the agent — honoring the launch-at-login preference — and wait,
/// bounded, for the health endpoint to appear.
///
/// The preference decides the *mechanism*, not just the login behavior:
///
/// - preference **on** → `SMAppService` registration (the launchd agent:
///   starts now via `RunAtLoad`, restarts on crash, runs at login);
/// - preference **off** → a direct spawn of the bundled `gramdrive-agent`
///   for this session only, so the user's "don't run at login" choice is
///   never silently upgraded to a persistent launchd registration.

/// Starts the agent process one way or the other. Seam so the ensurer is
/// deterministic under test; the live implementation is
/// ``BundledAgentStarter``.
public protocol AgentStarting: Sendable {
    /// Initiates a start, honoring `loginItemPreferred`. Returns once the
    /// start is initiated (not once the agent is up).
    func startAgent(loginItemPreferred: Bool) throws
}

/// The outcome of ``AgentEnsurer/ensureRunning()``.
public enum AgentEnsureOutcome: Equatable, Sendable {
    /// The agent was already answering health.
    case alreadyRunning
    /// The agent was started and now answers health.
    case started
    /// The agent could not be started (or never became healthy in time);
    /// the associated detail is diagnostic.
    case failed(String)
}

/// The live starter: `SMAppService` when the login item is preferred, a
/// detached spawn of the bundled agent binary otherwise.
///
/// `@unchecked Sendable`: the held `LoginItemService` (SMAppService in
/// production) is called at most once per start attempt and the framework
/// call is itself thread-safe; nothing here mutates.
public final class BundledAgentStarter: AgentStarting, @unchecked Sendable {
    private let loginItem: any LoginItemService
    private let agentExecutable: URL?

    /// - Parameters:
    ///   - loginItem: the login-item service (the app's registration right;
    ///     the plist lives in the app bundle).
    ///   - agentExecutable: the bundled `gramdrive-agent` binary; defaults
    ///     to the sibling of the running executable (the app-bundle layout:
    ///     both live in `Contents/MacOS/`).
    public init(
        loginItem: any LoginItemService = SMAppServiceAgentLoginItem(),
        agentExecutable: URL? = Bundle.main.executableURL?
            .deletingLastPathComponent()
            .appendingPathComponent("gramdrive-agent", isDirectory: false)
    ) {
        self.loginItem = loginItem
        self.agentExecutable = agentExecutable
    }

    public func startAgent(loginItemPreferred: Bool) throws {
        if loginItemPreferred {
            // Idempotent: an already-registered agent is a no-op, and
            // launchd starts a registered agent immediately (RunAtLoad).
            _ = try LaunchAtLoginPolicy.reconcile(preference: true, service: loginItem)
            return
        }
        guard let agentExecutable,
            FileManager.default.isExecutableFile(atPath: agentExecutable.path)
        else {
            throw CocoaError(.fileNoSuchFile)
        }
        let process = Process()
        process.executableURL = agentExecutable
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try process.run()
        // Deliberately not waited on: the agent daemonizes itself through
        // its single-instance lock and health endpoint; the ensurer's
        // health polling is the readiness signal.
    }
}

/// Ensures the agent is running before a control operation, turning the
/// old dead-end ("no channel") into an explicit starting phase with a
/// bounded wait.
public struct AgentEnsurer: Sendable {
    private let probe: @Sendable () async -> HealthReadout
    private let starter: any AgentStarting
    private let loginItemPreferred: @Sendable () -> Bool
    private let pollInterval: Duration
    private let startupTimeout: Duration

    /// - Parameters:
    ///   - probe: one bounded health read (the backend's own health path).
    ///   - starter: how a not-running agent is started.
    ///   - loginItemPreferred: the user's durable launch-at-login choice,
    ///     read at ensure time (settings may have changed since launch).
    ///   - pollInterval: delay between readiness probes after a start.
    ///   - startupTimeout: total bound on waiting for readiness.
    public init(
        probe: @escaping @Sendable () async -> HealthReadout,
        starter: any AgentStarting,
        loginItemPreferred: @escaping @Sendable () -> Bool,
        pollInterval: Duration = .milliseconds(250),
        startupTimeout: Duration = .seconds(15)
    ) {
        self.probe = probe
        self.starter = starter
        self.loginItemPreferred = loginItemPreferred
        self.pollInterval = pollInterval
        self.startupTimeout = startupTimeout
    }

    /// Probes, starts when needed, and waits (bounded) for health.
    public func ensureRunning() async -> AgentEnsureOutcome {
        if case .running = await probe() {
            return .alreadyRunning
        }
        do {
            try starter.startAgent(loginItemPreferred: loginItemPreferred())
        } catch {
            return .failed("agent start failed: \(error)")
        }
        let deadline = ContinuousClock.now + startupTimeout
        while ContinuousClock.now < deadline {
            if case .running = await probe() {
                return .started
            }
            try? await Task.sleep(for: pollInterval)
        }
        return .failed("the agent did not become healthy in time")
    }
}
