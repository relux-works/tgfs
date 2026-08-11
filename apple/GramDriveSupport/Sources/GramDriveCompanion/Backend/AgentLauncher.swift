import Foundation
import GramDriveAgentCore

#if canImport(AppKit)
import AppKit
#endif

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
/// companion-owned spawn of the bundled agent binary otherwise.
///
/// The direct child is retained for the companion session, repeated starts
/// are coalesced, and only that owned child receives termination when the
/// companion exits. An agent that was already running remains external and
/// is never stopped here.
///
/// `@unchecked Sendable`: mutable child-process state is protected by
/// ``processLock``. The held `LoginItemService` (SMAppService in production)
/// is used only on the persistent launch-at-login path.
public final class BundledAgentStarter: AgentStarting, @unchecked Sendable {
    private let loginItem: any LoginItemService
    private let agentExecutable: URL?
    private let processLock = NSLock()
    private var ownedProcess: Process?
    #if canImport(AppKit)
    private var terminationObserver: NSObjectProtocol?
    #endif

    /// - Parameters:
    ///   - loginItem: the login-item service (the app's registration right;
    ///     the plist lives in the app bundle).
    ///   - agentExecutable: the bundled `gramdrive-agent` binary; defaults
    ///     to the sibling of the running executable (the app-bundle layout:
    ///     both live in `Contents/MacOS/`).
    public init(
        loginItem: any LoginItemService = SMAppServiceAgentLoginItem(),
        agentExecutable: URL? = BundledAgentStarter.bundledAgentExecutable(
            relativeTo: Bundle.main.executableURL)
    ) {
        self.loginItem = loginItem
        self.agentExecutable = agentExecutable
        #if canImport(AppKit)
        self.terminationObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.willTerminateNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            self?.stopOwnedAgent()
        }
        #endif
    }

    deinit {
        #if canImport(AppKit)
        if let terminationObserver {
            NotificationCenter.default.removeObserver(terminationObserver)
        }
        #endif
        stopOwnedAgent()
    }

    /// Resolves the packaging contract shared with
    /// `.scripts/apple-app/build_app_bundle.py`: the app shell and agent are
    /// sibling executables in `Contents/MacOS`. Kept as a pure function so a
    /// release path can be regression-tested without depending on the test
    /// host's `Bundle.main`.
    public static func bundledAgentExecutable(relativeTo appExecutable: URL?) -> URL? {
        appExecutable?
            .deletingLastPathComponent()
            .appendingPathComponent("gramdrive-agent", isDirectory: false)
    }

    public func startAgent(loginItemPreferred: Bool) throws {
        if loginItemPreferred {
            // Registration is still the durable login preference. `RunAtLoad`
            // normally starts it immediately, but a planned updater exit is
            // intentionally not restarted by launchd. The ensurer only calls
            // us after health was absent, so also launch the bundled current
            // session executable without toggling the user's preference.
            _ = try LaunchAtLoginPolicy.reconcile(preference: true, service: loginItem)
        }
        try startBundledAgentForCurrentSession()
    }

    private func startBundledAgentForCurrentSession() throws {
        guard let agentExecutable,
            FileManager.default.isExecutableFile(atPath: agentExecutable.path)
        else {
            throw CocoaError(.fileNoSuchFile)
        }

        processLock.lock()
        defer { processLock.unlock() }
        if let ownedProcess, ownedProcess.isRunning {
            return
        }

        let process = Process()
        process.executableURL = agentExecutable
        process.arguments = ["run"]
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try process.run()
        ownedProcess = process
        // Deliberately not waited on: health is the readiness signal. The
        // retained Process is session ownership, not a second readiness
        // channel or a persistent launch-at-login registration.
    }

    /// Stops only the direct child this starter launched. Idempotent and
    /// intentionally non-blocking so app termination cannot be held hostage
    /// by the agent's bounded drain.
    public func stopOwnedAgent() {
        processLock.lock()
        let process = ownedProcess
        processLock.unlock()
        if let process, process.isRunning {
            process.terminate()
        }
    }

    /// Test/diagnostic projection of the currently live owned child.
    var ownedProcessIdentifier: Int32? {
        processLock.lock()
        defer { processLock.unlock() }
        guard let ownedProcess, ownedProcess.isRunning else { return nil }
        return ownedProcess.processIdentifier
    }
}

/// Ensures the agent is running before a control operation, turning the
/// old dead-end ("no channel") into an explicit starting phase with a
/// bounded wait.
public actor AgentEnsurer {
    private let probe: @Sendable () async -> HealthReadout
    private let starter: any AgentStarting
    private let loginItemPreferred: @Sendable () -> Bool
    private let pollInterval: Duration
    private let startupTimeout: Duration
    private var inFlight: (id: UUID, task: Task<AgentEnsureOutcome, Never>)?

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
        if let inFlight {
            return await inFlight.task.value
        }
        let id = UUID()
        let probe = self.probe
        let starter = self.starter
        let loginItemPreferred = self.loginItemPreferred
        let pollInterval = self.pollInterval
        let startupTimeout = self.startupTimeout
        let task = Task {
            await Self.performEnsure(
                probe: probe,
                starter: starter,
                loginItemPreferred: loginItemPreferred,
                pollInterval: pollInterval,
                startupTimeout: startupTimeout)
        }
        inFlight = (id, task)
        let outcome = await task.value
        if inFlight?.id == id {
            inFlight = nil
        }
        return outcome
    }

    private static func performEnsure(
        probe: @Sendable () async -> HealthReadout,
        starter: any AgentStarting,
        loginItemPreferred: @Sendable () -> Bool,
        pollInterval: Duration,
        startupTimeout: Duration
    ) async -> AgentEnsureOutcome {
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
