import Foundation
import GramDriveAgentCore

/// In-memory ``CompanionBackend`` for SwiftUI previews and tests — no agent,
/// no sockets, every outcome scripted. Mirrors the Rust `gramdrive-testkit`
/// idea: a substitutable seam implementation that makes every screen state
/// reachable deterministically.
public final class InMemoryCompanionBackend: CompanionBackend, @unchecked Sendable {
    private let lock = NSLock()
    private var health: HealthReadout
    private var settings: AgentSettings
    private var repairOutcome: CommandOutcome
    private var removalOutcome: CommandOutcome
    private var saveError: (any Error)?
    private var loadError: (any Error)?
    private let sessionFactory: @Sendable () -> any AuthorizationSession

    public init(
        health: HealthReadout = .notRunning,
        settings: AgentSettings = AgentSettings(),
        repairOutcome: CommandOutcome = .unavailable(.notWired),
        removalOutcome: CommandOutcome = .unavailable(.notWired),
        saveError: (any Error)? = nil,
        loadError: (any Error)? = nil,
        session: @escaping @Sendable () -> any AuthorizationSession = {
            UnavailableAuthorizationSession(reason: .notWired)
        }
    ) {
        self.health = health
        self.settings = settings
        self.repairOutcome = repairOutcome
        self.removalOutcome = removalOutcome
        self.saveError = saveError
        self.loadError = loadError
        self.sessionFactory = session
    }

    /// The settings last saved (or the seed), for assertions.
    public var storedSettings: AgentSettings {
        lock.withLock { settings }
    }

    public func setHealth(_ readout: HealthReadout) {
        lock.withLock { health = readout }
    }

    public func fetchHealth() async -> HealthReadout {
        lock.withLock { health }
    }

    public func loadSettings() throws -> AgentSettings {
        try lock.withLock {
            if let loadError { throw loadError }
            return settings
        }
    }

    public func saveSettings(_ settings: AgentSettings) throws {
        try lock.withLock {
            if let saveError { throw saveError }
            self.settings = settings
        }
    }

    public func makeAuthorizationSession() -> any AuthorizationSession {
        sessionFactory()
    }

    public func requestRepair() async -> CommandOutcome {
        lock.withLock { repairOutcome }
    }

    public func removeAccount(_ confirmation: RemovalConfirmation) async -> CommandOutcome {
        lock.withLock { removalOutcome }
    }
}

/// A hand-driven ``AuthorizationSession``: the test/preview emits states on
/// its stream and programs the reply to each ``submit(_:)``. The state stream
/// is buffered (`AsyncStream`'s default), so states emitted before the view
/// model subscribes are not lost — the flow can be fully scripted up front.
public final class ScriptedAuthorizationSession: AuthorizationSession, @unchecked Sendable {
    private let startResult: AuthStartResult
    private let submitHandler: @Sendable (CompanionAuthInput) -> AuthSubmitResult
    private let stream: AsyncStream<CompanionAuthState>
    private let continuation: AsyncStream<CompanionAuthState>.Continuation

    public init(
        startResult: AuthStartResult = .started,
        onSubmit: @escaping @Sendable (CompanionAuthInput) -> AuthSubmitResult = { _ in .accepted }
    ) {
        self.startResult = startResult
        self.submitHandler = onSubmit
        (self.stream, self.continuation) = AsyncStream.makeStream(of: CompanionAuthState.self)
    }

    public var states: AsyncStream<CompanionAuthState> { stream }

    public func start() async -> AuthStartResult { startResult }

    public func submit(_ input: CompanionAuthInput) async -> AuthSubmitResult {
        submitHandler(input)
    }

    /// Pushes one reported state onto the stream.
    public func emit(_ state: CompanionAuthState) {
        continuation.yield(state)
    }

    /// Ends the stream (the flow reached a terminal or the channel closed).
    public func finish() {
        continuation.finish()
    }
}

/// A ``DiskSpaceProbe`` that reports a fixed capacity (or none). For tests
/// and previews of the Archive Mode preflight.
public struct FixedDiskSpaceProbe: DiskSpaceProbe {
    private let available: UInt64?

    public init(available: UInt64?) {
        self.available = available
    }

    public func availableCapacityBytes() -> UInt64? { available }
}

/// A representative health snapshot for previews and tests.
public func previewSnapshot(
    state: AgentRunState = .running,
    providerRegistrationState: String? = nil,
    recentEvents: [String] = ["started", "wake"]
) -> AgentHealthSnapshot {
    AgentHealthSnapshot(
        payloadVersion: 1,
        agentVersion: AgentVersion.current,
        contractVersion: "0.2.0",
        pid: 4242,
        state: state,
        startedAtMs: 1_700_000_000_000,
        launchAtLogin: true,
        stateSchemaVersion: 2,
        dataVersion: 17,
        pendingTransferCount: 0,
        lastSourceUpdateMs: nil,
        changeCursor: nil,
        cachePressure: nil,
        providerRegistrationState: providerRegistrationState,
        lastSleepMs: nil,
        lastWakeMs: 1_700_000_100_000,
        recentEvents: recentEvents)
}

extension NSLock {
    fileprivate func withLock<T>(_ body: () throws -> T) rethrows -> T {
        lock()
        defer { unlock() }
        return try body()
    }
}
