import Darwin
import Foundation
import GramDriveSupport
import Testing

@testable import GramDriveAgentCore

/// The control channel end to end: the real server and the real client
/// over a substitute socket, with scripted seams playing the engine
/// (BUG-260720-3i74u1).
@Suite struct ControlChannelTests {
    // MARK: - Fixtures

    /// A per-test socket home under the system temp dir.
    private static func tempRoot() throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-control-\(UUID().uuidString.prefix(8))")
        try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        return url
    }

    private static func snapshot(accounts: [AccountHealthSummary]? = nil) -> AgentHealthSnapshot {
        AgentHealthSnapshot(
            payloadVersion: 1,
            agentVersion: AgentVersion.current,
            contractVersion: "0.6.0",
            pid: 42,
            state: .running,
            startedAtMs: 1_000,
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
            recentEvents: ["started"],
            accounts: accounts)
    }

    private static func handlers(
        authorizer: (any AgentAuthorizing)? = nil,
        remover: (any AgentAccountRemoving)? = nil,
        repairer: (any AgentRepairing)? = nil,
        accounts: [AccountHealthSummary]? = nil
    ) -> ControlServerHandlers {
        ControlServerHandlers(
            status: { snapshot(accounts: accounts) },
            reloadSettings: { AgentSettings(launchAtLogin: true, cacheQuotaBytes: 7) },
            authorizer: authorizer,
            remover: remover,
            repairer: repairer)
    }

    // (bounded event consumption lives in `EventCollector` below)

    // MARK: - Commands

    @Test func statusAnswersTheLifecycleSnapshot() throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let server = try ControlServer.start(socketURL: socket, handlers: Self.handlers())
        defer { server.stop() }

        let event = try ControlClient.command(
            ControlRequest(operation: .status), socketURL: socket, timeout: .seconds(5))
        #expect(event == .status(Self.snapshot()))
    }

    @Test func reloadSettingsAnswersTheAppliedDocument() throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let server = try ControlServer.start(socketURL: socket, handlers: Self.handlers())
        defer { server.stop() }

        let event = try ControlClient.command(
            ControlRequest(operation: .reloadSettings), socketURL: socket, timeout: .seconds(5))
        #expect(event == .settings(AgentSettings(launchAtLogin: true, cacheQuotaBytes: 7)))
    }

    @Test func repairRunsTheSeamAndReportsItsOutcome() async throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let repairer = ScriptedRepairer(
            outcome: .failed(
                ControlCommandFailure(category: .authRequired, detail: "sign in first")))
        let server = try ControlServer.start(
            socketURL: socket, handlers: Self.handlers(repairer: repairer))
        defer { server.stop() }

        let event = try ControlClient.command(
            ControlRequest(operation: .repair), socketURL: socket, timeout: .seconds(5))
        #expect(
            event
                == .commandFailed(
                    ControlCommandFailure(category: .authRequired, detail: "sign in first")))
        #expect(repairer.runCount == 1)
    }

    @Test func removalRunsTheSeamWithItsParameters() async throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let remover = ScriptedRemover(outcome: .completed)
        let server = try ControlServer.start(
            socketURL: socket, handlers: Self.handlers(remover: remover))
        defer { server.stop() }

        let event = try ControlClient.command(
            ControlRequest(
                operation: .removeAccount,
                removal: ControlRemovalRequest(accountId: 777, revokeSession: true)),
            socketURL: socket,
            timeout: .seconds(5))
        #expect(event == .commandDone)
        #expect(remover.requests == [ControlRemovalRequest(accountId: 777, revokeSession: true)])
    }

    @Test func removalWithoutParametersIsRefusedTyped() throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let server = try ControlServer.start(
            socketURL: socket, handlers: Self.handlers(remover: ScriptedRemover(outcome: .completed)))
        defer { server.stop() }

        let event = try ControlClient.command(
            ControlRequest(operation: .removeAccount), socketURL: socket, timeout: .seconds(5))
        guard case .commandFailed(let failure) = event else {
            Issue.record("expected a typed refusal, got \(event)")
            return
        }
        #expect(failure.category == .invalidArgument)
    }

    @Test func aMissingSeamAnswersSourceUnavailable() throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let server = try ControlServer.start(socketURL: socket, handlers: Self.handlers())
        defer { server.stop() }

        let event = try ControlClient.command(
            ControlRequest(operation: .repair), socketURL: socket, timeout: .seconds(5))
        guard case .commandFailed(let failure) = event else {
            Issue.record("expected a typed refusal, got \(event)")
            return
        }
        #expect(failure.category == .sourceUnavailable)
    }

    @Test func aVersionMismatchIsRefusedTyped() throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let server = try ControlServer.start(socketURL: socket, handlers: Self.handlers())
        defer { server.stop() }

        let event = try ControlClient.command(
            ControlRequest(protocolVersion: 99, operation: .status),
            socketURL: socket,
            timeout: .seconds(5))
        guard case .commandFailed(let failure) = event else {
            Issue.record("expected a typed refusal, got \(event)")
            return
        }
        #expect(failure.category == .invalidArgument)
        #expect(failure.detail.contains("protocol version"))
    }

    @Test func noAgentIsATypedTransportError() throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        #expect(throws: ControlTransportError.agentUnavailable(path: socket.path)) {
            _ = try ControlClient.command(
                ControlRequest(operation: .status), socketURL: socket, timeout: .seconds(1))
        }
    }

    // MARK: - The auth session

    @Test func authSessionStreamsStatesAndCorrelatesSubmits() async throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let session = ScriptedHostedSession()
        let server = try ControlServer.start(
            socketURL: socket,
            handlers: Self.handlers(authorizer: ScriptedAuthorizer(session: session)))
        defer { server.stop() }

        session.emit(ControlAuthState(kind: "starting"))
        let channel = try ControlAuthChannel.open(socketURL: socket)
        defer { channel.close() }

        let events = EventCollector(channel.events)
        #expect(await events.next() == .authState(ControlAuthState(kind: "starting")))

        session.emit(ControlAuthState(kind: "wait-phone-number"))
        #expect(
            await events.next()
                == .authState(ControlAuthState(kind: "wait-phone-number")))

        try channel.send(
            ControlAuthInputFrame(seq: 7, input: .submitPhoneNumber("+9996612222")))
        #expect(
            await events.next()
                == .authSubmitResult(ControlAuthSubmitResult(seq: 7, outcome: "accepted")))
        #expect(session.submitted == [.submitPhoneNumber("+9996612222")])

        // A rejection answer keeps its classification and the caller's seq.
        session.answer = .rejected(
            ControlAuthRejection(kind: "invalid-code"))
        try channel.send(ControlAuthInputFrame(seq: 9, input: .submitCode("00000")))
        #expect(
            await events.next()
                == .authSubmitResult(
                    ControlAuthSubmitResult(
                        seq: 9,
                        outcome: "rejected",
                        rejection: ControlAuthRejection(kind: "invalid-code"))))
    }

    @Test func authSessionEndingFinishesTheChannel() async throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let session = ScriptedHostedSession()
        let server = try ControlServer.start(
            socketURL: socket,
            handlers: Self.handlers(authorizer: ScriptedAuthorizer(session: session)))
        defer { server.stop() }

        session.emit(ControlAuthState(kind: "starting"))
        let channel = try ControlAuthChannel.open(socketURL: socket)
        defer { channel.close() }
        let events = EventCollector(channel.events)
        #expect(await events.next() == .authState(ControlAuthState(kind: "starting")))

        session.emit(ControlAuthState(kind: "closed"))
        session.finishStates()
        #expect(await events.next() == .authState(ControlAuthState(kind: "closed")))
        #expect(await events.next() == nil, "the stream ends with the session")
    }

    @Test func clientDisconnectClosesTheHostedSession() async throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let session = ScriptedHostedSession()
        let server = try ControlServer.start(
            socketURL: socket,
            handlers: Self.handlers(authorizer: ScriptedAuthorizer(session: session)))
        defer { server.stop() }

        session.emit(ControlAuthState(kind: "starting"))
        let channel = try ControlAuthChannel.open(socketURL: socket)
        let events = EventCollector(channel.events)
        #expect(await events.next() == .authState(ControlAuthState(kind: "starting")))

        channel.close()
        let deadline = ContinuousClock.now + .seconds(5)
        while !session.isClosed, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        #expect(session.isClosed, "EOF must close the hosted session")
    }

    @Test func withoutAnAuthorizerTheUpgradeIsRefused() async throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let server = try ControlServer.start(socketURL: socket, handlers: Self.handlers())
        defer { server.stop() }

        let channel = try ControlAuthChannel.open(socketURL: socket)
        defer { channel.close() }
        let events = EventCollector(channel.events)
        guard case .commandFailed(let failure) = await events.next() else {
            Issue.record("expected a refusal event")
            return
        }
        #expect(failure.category == .sourceUnavailable)
    }

    @Test func stopClosesActiveSessions() async throws {
        let root = try Self.tempRoot()
        let socket = ControlContract.socketURL(dataRoot: root)
        try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
        let session = ScriptedHostedSession()
        let server = try ControlServer.start(
            socketURL: socket,
            handlers: Self.handlers(authorizer: ScriptedAuthorizer(session: session)))

        session.emit(ControlAuthState(kind: "starting"))
        let channel = try ControlAuthChannel.open(socketURL: socket)
        defer { channel.close() }
        let events = EventCollector(channel.events)
        #expect(await events.next() == .authState(ControlAuthState(kind: "starting")))

        server.stop()
        let deadline = ContinuousClock.now + .seconds(5)
        while !session.isClosed, ContinuousClock.now < deadline {
            try await Task.sleep(for: .milliseconds(20))
        }
        #expect(session.isClosed, "stop() must close hosted sessions")
    }
}

// MARK: - Bounded event consumption

/// Pumps a channel's events into a buffer so tests can await the next one
/// under a deadline — a wedged stream is a failure, never a hang.
final class EventCollector: @unchecked Sendable {
    private let lock = NSLock()
    private var items: [ControlEvent] = []
    private var finished = false
    private var cursor = 0

    init(_ stream: AsyncStream<ControlEvent>) {
        Task {
            for await event in stream {
                self.append(event)
            }
            self.markFinished()
        }
    }

    /// The next unseen event, `nil` once the stream finished. Fails the
    /// test on timeout.
    func next(
        within bound: Duration = .seconds(5),
        sourceLocation: Testing.SourceLocation = #_sourceLocation
    ) async -> ControlEvent? {
        let deadline = ContinuousClock.now + bound
        while ContinuousClock.now < deadline {
            if let (event, done) = poll() {
                if done { return nil }
                return event
            }
            try? await Task.sleep(for: .milliseconds(10))
        }
        Issue.record("no event arrived within the bound", sourceLocation: sourceLocation)
        return nil
    }

    private func poll() -> (ControlEvent?, Bool)? {
        lock.lock()
        defer { lock.unlock() }
        if cursor < items.count {
            let event = items[cursor]
            cursor += 1
            return (event, false)
        }
        if finished {
            return (nil, true)
        }
        return nil
    }

    private func append(_ event: ControlEvent) {
        lock.lock()
        items.append(event)
        lock.unlock()
    }

    private func markFinished() {
        lock.lock()
        finished = true
        lock.unlock()
    }
}

// MARK: - Scripted seams

private final class ScriptedRepairer: AgentRepairing, @unchecked Sendable {
    private let lock = NSLock()
    private let outcome: ControlCommandOutcome
    private var runs = 0

    init(outcome: ControlCommandOutcome) {
        self.outcome = outcome
    }

    var runCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return runs
    }

    func repair() async -> ControlCommandOutcome {
        recordRun()
        return outcome
    }

    private func recordRun() {
        lock.lock()
        runs += 1
        lock.unlock()
    }
}

private final class ScriptedRemover: AgentAccountRemoving, @unchecked Sendable {
    private let lock = NSLock()
    private let outcome: ControlCommandOutcome
    private var received: [ControlRemovalRequest] = []

    init(outcome: ControlCommandOutcome) {
        self.outcome = outcome
    }

    var requests: [ControlRemovalRequest] {
        lock.lock()
        defer { lock.unlock() }
        return received
    }

    func remove(_ request: ControlRemovalRequest) async -> ControlCommandOutcome {
        record(request)
        return outcome
    }

    private func record(_ request: ControlRemovalRequest) {
        lock.lock()
        received.append(request)
        lock.unlock()
    }
}

private struct ScriptedAuthorizer: AgentAuthorizing {
    let session: ScriptedHostedSession

    func makeSession() throws -> any AgentAuthSessionHosting {
        session
    }
}

/// A hand-scripted hosted session: tests emit states and pick the answer
/// each submit receives.
final class ScriptedHostedSession: AgentAuthSessionHosting, @unchecked Sendable {
    private let lock = NSLock()
    private let stream: AsyncStream<ControlAuthState>
    private let continuation: AsyncStream<ControlAuthState>.Continuation
    private var inputs: [ControlAuthInput] = []
    private var closed = false

    /// The answer the next submit receives; tests mutate between inputs.
    var answer: AgentAuthSubmitAnswer = .accepted

    init() {
        (stream, continuation) = AsyncStream.makeStream(of: ControlAuthState.self)
    }

    var states: AsyncStream<ControlAuthState> {
        stream
    }

    var submitted: [ControlAuthInput] {
        lock.lock()
        defer { lock.unlock() }
        return inputs
    }

    var isClosed: Bool {
        lock.lock()
        defer { lock.unlock() }
        return closed
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

    private func record(_ input: ControlAuthInput) -> AgentAuthSubmitAnswer {
        lock.lock()
        defer { lock.unlock() }
        inputs.append(input)
        return answer
    }

    func close() {
        lock.lock()
        closed = true
        lock.unlock()
        continuation.finish()
    }
}
