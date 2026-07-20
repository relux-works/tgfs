import Foundation
import GramDriveAgentCore
import GramDriveSupport
import Testing

@testable import GramDriveCompanion

/// The live command path end to end (BUG-260720-3i74u1): the ensurer's
/// probe-start-wait contract, and the live backend + authorization session
/// against a real control/health server pair with scripted engine seams.
@Suite struct LiveControlTests {
    // MARK: - Fixtures

    private static func tempLayout() throws -> AgentRuntimeLayout {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-livectl-\(UUID().uuidString.prefix(8))")
        let layout = AgentRuntimeLayout(dataRoot: url)
        try layout.ensureDirectories()
        return layout
    }

    private static func snapshot(accounts: [AccountHealthSummary]? = nil) -> AgentHealthSnapshot {
        AgentHealthSnapshot(
            payloadVersion: 1,
            agentVersion: AgentVersion.current,
            contractVersion: "0.6.0",
            pid: 7,
            state: .running,
            startedAtMs: 0,
            launchAtLogin: nil,
            stateSchemaVersion: nil,
            dataVersion: nil,
            pendingTransferCount: 0,
            lastSourceUpdateMs: nil,
            changeCursor: nil,
            cachePressure: nil,
            providerRegistrationState: nil,
            lastSleepMs: nil,
            lastWakeMs: nil,
            recentEvents: [],
            accounts: accounts)
    }

    /// A running agent stand-in: real health + control servers over the
    /// layout's sockets, with scripted seams.
    fileprivate final class FakeAgent: @unchecked Sendable {
        let health: AgentHealthServer
        let control: ControlServer

        init(
            layout: AgentRuntimeLayout,
            accounts: [AccountHealthSummary]? = nil,
            authorizer: (any AgentAuthorizing)? = nil,
            remover: (any AgentAccountRemoving)? = nil,
            repairer: (any AgentRepairing)? = nil
        ) throws {
            health = try AgentHealthServer.start(socketURL: layout.healthSocket) {
                LiveControlTests.snapshot(accounts: accounts)
            }
            control = try ControlServer.start(
                socketURL: layout.controlSocket,
                handlers: ControlServerHandlers(
                    status: { LiveControlTests.snapshot(accounts: accounts) },
                    reloadSettings: { AgentSettings() },
                    authorizer: authorizer,
                    remover: remover,
                    repairer: repairer))
        }

        func stop() {
            control.stop()
            health.stop()
        }
    }

    /// A starter the tests script: records the preference it was asked to
    /// honor and runs a closure (typically bringing a ``FakeAgent`` up).
    fileprivate final class ScriptedStarter: AgentStarting, @unchecked Sendable {
        private let lock = NSLock()
        private var preferences: [Bool] = []
        private let onStart: @Sendable () throws -> Void

        init(onStart: @escaping @Sendable () throws -> Void = {}) {
            self.onStart = onStart
        }

        var askedPreferences: [Bool] {
            lock.lock()
            defer { lock.unlock() }
            return preferences
        }

        func startAgent(loginItemPreferred: Bool) throws {
            lock.lock()
            preferences.append(loginItemPreferred)
            lock.unlock()
            try onStart()
        }
    }

    // MARK: - The ensurer

    @Test func ensurerReportsAnAlreadyRunningAgentWithoutStarting() async {
        let starter = ScriptedStarter()
        let ensurer = AgentEnsurer(
            probe: { .running(Self.snapshot()) },
            starter: starter,
            loginItemPreferred: { true })
        #expect(await ensurer.ensureRunning() == .alreadyRunning)
        #expect(starter.askedPreferences.isEmpty)
    }

    @Test func ensurerStartsAndWaitsForHealth() async {
        let flag = FlagBox()
        let starter = ScriptedStarter(onStart: { flag.set() })
        let ensurer = AgentEnsurer(
            probe: { flag.isSet ? .running(Self.snapshot()) : .notRunning },
            starter: starter,
            loginItemPreferred: { false },
            pollInterval: .milliseconds(10),
            startupTimeout: .seconds(5))
        #expect(await ensurer.ensureRunning() == .started)
        #expect(starter.askedPreferences == [false], "the preference is honored, not upgraded")
    }

    @Test func ensurerReportsAStartFailureTyped() async {
        struct Boom: Error {}
        let starter = ScriptedStarter(onStart: { throw Boom() })
        let ensurer = AgentEnsurer(
            probe: { .notRunning },
            starter: starter,
            loginItemPreferred: { false },
            pollInterval: .milliseconds(10),
            startupTimeout: .milliseconds(100))
        guard case .failed = await ensurer.ensureRunning() else {
            Issue.record("a throwing starter must fail the ensure")
            return
        }
    }

    @Test func ensurerTimesOutWhenTheAgentNeverAnswers() async {
        let ensurer = AgentEnsurer(
            probe: { .notRunning },
            starter: ScriptedStarter(),
            loginItemPreferred: { false },
            pollInterval: .milliseconds(10),
            startupTimeout: .milliseconds(80))
        guard case .failed = await ensurer.ensureRunning() else {
            Issue.record("an agent that never answers must fail the ensure")
            return
        }
    }

    // MARK: - The live authorization session

    @Test func liveSessionMapsWireStatesAndResults() async throws {
        let layout = try Self.tempLayout()
        let hosted = ScriptedCompanionHostedSession()
        let agent = try FakeAgent(
            layout: layout, authorizer: ScriptedCompanionAuthorizer(session: hosted))
        defer { agent.stop() }

        hosted.emit(ControlAuthState(kind: "starting"))
        let controlSocket = layout.controlSocket
        let session = LiveAuthorizationSession(openChannel: {
            .opened(try! ControlAuthChannel.open(socketURL: controlSocket))
        })
        let states = StateCollector(session.states)
        #expect(await session.start() == .started)
        #expect(await states.next() == .starting)

        hosted.emit(ControlAuthState(kind: "wait-phone-number"))
        #expect(await states.next() == .waitPhoneNumber)

        let accepted = await session.submit(.submitPhoneNumber("+9996612222"))
        #expect(accepted == .accepted)
        #expect(hosted.submitted == [.submitPhoneNumber("+9996612222")])

        hosted.answer = AgentAuthSubmitAnswer(
            outcome: "rejected",
            rejection: ControlAuthRejection(kind: "rate-limited", retryAfterSeconds: 17))
        let rejected = await session.submit(.submitCode("00000"))
        #expect(rejected == .rejected(.rateLimited(retryAfterSeconds: 17)))

        // The code step's rendering material crosses whole.
        hosted.emit(
            ControlAuthState(
                kind: "wait-code",
                codeInfo: ControlAuthCodeInfo(
                    phoneNumber: "+9996612222", codeLength: 5, resendTimeoutSeconds: 60)))
        #expect(
            await states.next()
                == .waitCode(
                    CompanionCodeInfo(
                        phoneNumber: "+9996612222", codeLength: 5, resendTimeoutSeconds: 60)))

        // Finalizing renders as machinery; ready carries through; a foreign
        // state fails safe.
        hosted.emit(ControlAuthState(kind: "finalizing"))
        #expect(await states.next() == .configuring)
        hosted.emit(
            ControlAuthState(
                kind: "ready",
                account: ControlAccountIdentity(accountId: 777, displayName: "Test User")))
        #expect(await states.next() == .ready)
        hosted.emit(ControlAuthState(kind: "brand-new-step"))
        #expect(await states.next() == .unsupported(kind: "brand-new-step"))

        hosted.finishStates()
        #expect(await states.next() == nil, "the state stream ends with the session")
    }

    @Test func liveSessionReportsAnUnopenableChannel() async {
        let session = LiveAuthorizationSession(openChannel: {
            .unavailable(.agentNotRunning)
        })
        #expect(await session.start() == .unavailable(.agentNotRunning))
        #expect(await session.submit(.cancel) == .unavailable(.dropped))
    }

    // MARK: - The live backend

    @Test func backendStartsTheAgentThenRunsCommands() async throws {
        let layout = try Self.tempLayout()
        let agentBox = AgentBox()
        let repairer = RecordingRepairer()
        let starter = ScriptedStarter(onStart: {
            agentBox.agent = try FakeAgent(layout: layout, repairer: repairer)
        })
        defer { agentBox.agent?.stop() }
        let backend = LiveCompanionBackend(
            layout: layout, healthTimeout: .seconds(2), starter: starter,
            startupTimeout: .seconds(5))

        #expect(await backend.requestRepair() == .completed)
        #expect(repairer.runCount == 1)
        #expect(starter.askedPreferences == [false], "no settings file: login item defaults off")
    }

    @Test func backendReportsAgentNotRunningWhenStartFails() async throws {
        let layout = try Self.tempLayout()
        struct Boom: Error {}
        let backend = LiveCompanionBackend(
            layout: layout,
            healthTimeout: .seconds(1),
            starter: ScriptedStarter(onStart: { throw Boom() }),
            startupTimeout: .milliseconds(100))

        #expect(await backend.requestRepair() == .unavailable(.agentNotRunning))
        let removal = await backend.removeAccount(
            RemovalConfirmation(
                accountLabel: "A", typedConfirmation: "A", acknowledgedIrreversible: true))
        #expect(removal == .unavailable(.agentNotRunning))
        let auth = backend.makeAuthorizationSession()
        #expect(await auth.start() == .unavailable(.agentNotRunning))
    }

    @Test func backendRemovalResolvesTheAccountAndRunsBothHalves() async throws {
        let layout = try Self.tempLayout()
        let remover = RecordingRemover()
        let cleanup = CleanupRecorder()
        let agent = try FakeAgent(
            layout: layout,
            accounts: [
                AccountHealthSummary(
                    accountId: 777_000_123, displayName: "Test User", authState: "authorized")
            ],
            remover: remover)
        defer { agent.stop() }
        let backend = LiveCompanionBackend(
            layout: layout,
            healthTimeout: .seconds(2),
            starter: ScriptedStarter(),
            startupTimeout: .seconds(5),
            accountDomainCleanup: { cleanup.record($0) })

        let outcome = await backend.removeAccount(
            RemovalConfirmation(
                accountLabel: "This account",
                typedConfirmation: "this account",
                acknowledgedIrreversible: true))
        #expect(outcome == .completed)
        #expect(
            remover.requests
                == [ControlRemovalRequest(accountId: 777_000_123, revokeSession: true)])
        #expect(cleanup.accountIds == [777_000_123], "the domain half runs after the engine half")
    }

    @Test func backendRemovalWithNoAccountsIsNotFound() async throws {
        let layout = try Self.tempLayout()
        let agent = try FakeAgent(layout: layout, accounts: [])
        defer { agent.stop() }
        let backend = LiveCompanionBackend(
            layout: layout, healthTimeout: .seconds(2), starter: ScriptedStarter(),
            startupTimeout: .seconds(5))

        let outcome = await backend.removeAccount(
            RemovalConfirmation(
                accountLabel: "A", typedConfirmation: "A", acknowledgedIrreversible: true))
        #expect(outcome == .failed(.notFound))
    }
}

// MARK: - Small recorders

private final class FlagBox: @unchecked Sendable {
    private let lock = NSLock()
    private var flag = false
    var isSet: Bool {
        lock.lock()
        defer { lock.unlock() }
        return flag
    }
    func set() {
        lock.lock()
        flag = true
        lock.unlock()
    }
}

private final class AgentBox: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: LiveControlTests.FakeAgent?
    var agent: LiveControlTests.FakeAgent? {
        get {
            lock.lock()
            defer { lock.unlock() }
            return stored
        }
        set {
            lock.lock()
            stored = newValue
            lock.unlock()
        }
    }
}

private final class CleanupRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var ids: [Int64] = []
    var accountIds: [Int64] {
        lock.lock()
        defer { lock.unlock() }
        return ids
    }
    func record(_ id: Int64) {
        lock.lock()
        ids.append(id)
        lock.unlock()
    }
}

private final class RecordingRepairer: AgentRepairing, @unchecked Sendable {
    private let lock = NSLock()
    private var runs = 0
    var runCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return runs
    }
    func repair() async -> ControlCommandOutcome {
        recordRun()
        return .completed
    }
    private func recordRun() {
        lock.lock()
        runs += 1
        lock.unlock()
    }
}

private final class RecordingRemover: AgentAccountRemoving, @unchecked Sendable {
    private let lock = NSLock()
    private var received: [ControlRemovalRequest] = []
    var requests: [ControlRemovalRequest] {
        lock.lock()
        defer { lock.unlock() }
        return received
    }
    func remove(_ request: ControlRemovalRequest) async -> ControlCommandOutcome {
        record(request)
        return .completed
    }
    private func record(_ request: ControlRemovalRequest) {
        lock.lock()
        received.append(request)
        lock.unlock()
    }
}

private struct ScriptedCompanionAuthorizer: AgentAuthorizing {
    let session: ScriptedCompanionHostedSession
    func makeSession() throws -> any AgentAuthSessionHosting {
        session
    }
}

/// A hand-scripted hosted session (the companion-test twin of the agent
/// suite's fixture).
private final class ScriptedCompanionHostedSession: AgentAuthSessionHosting, @unchecked Sendable {
    private let lock = NSLock()
    private let stream: AsyncStream<ControlAuthState>
    private let continuation: AsyncStream<ControlAuthState>.Continuation
    private var inputs: [ControlAuthInput] = []

    var answer: AgentAuthSubmitAnswer = .accepted

    init() {
        (stream, continuation) = AsyncStream.makeStream(of: ControlAuthState.self)
    }

    var states: AsyncStream<ControlAuthState> { stream }

    var submitted: [ControlAuthInput] {
        lock.lock()
        defer { lock.unlock() }
        return inputs
    }

    func emit(_ state: ControlAuthState) {
        continuation.yield(state)
    }

    func finishStates() {
        continuation.finish()
    }

    func submit(_ input: ControlAuthInput) async -> AgentAuthSubmitAnswer {
        record(input)
    }

    func close() {
        continuation.finish()
    }

    private func record(_ input: ControlAuthInput) -> AgentAuthSubmitAnswer {
        lock.lock()
        defer { lock.unlock() }
        inputs.append(input)
        return answer
    }
}

/// Pumps companion auth states into a buffer for bounded assertions.
private final class StateCollector: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [CompanionAuthState] = []
    private var finished = false
    private var cursor = 0

    init(_ stream: AsyncStream<CompanionAuthState>) {
        Task {
            for await state in stream {
                self.append(state)
            }
            self.markFinished()
        }
    }

    func next(
        within bound: Duration = .seconds(5),
        sourceLocation: Testing.SourceLocation = #_sourceLocation
    ) async -> CompanionAuthState? {
        let deadline = ContinuousClock.now + bound
        while ContinuousClock.now < deadline {
            if let (state, done) = poll() {
                if done { return nil }
                return state
            }
            try? await Task.sleep(for: .milliseconds(10))
        }
        Issue.record("no state arrived within the bound", sourceLocation: sourceLocation)
        return nil
    }

    private func poll() -> (CompanionAuthState?, Bool)? {
        lock.lock()
        defer { lock.unlock() }
        if cursor < items.count {
            let state = items[cursor]
            cursor += 1
            return (state, false)
        }
        if finished {
            return (nil, true)
        }
        return nil
    }

    private func append(_ state: CompanionAuthState) {
        lock.lock()
        items.append(state)
        lock.unlock()
    }

    private func markFinished() {
        lock.lock()
        finished = true
        lock.unlock()
    }
}
