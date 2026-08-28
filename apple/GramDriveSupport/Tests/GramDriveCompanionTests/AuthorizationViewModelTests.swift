import Foundation
import Testing

@testable import GramDriveCompanion

/// Awaits the view model's flow to quiescence — the scripted session finishes
/// its stream, so this returns deterministically once every emitted state has
/// been applied.
@MainActor
private func settle(_ model: AuthorizationViewModel) async {
    await model.waitForCompletion()
}

@MainActor
@Suite struct AuthorizationScreenStateTests {
    // Every screen state renders from exactly one reported state — the "unit
    // test for each screen state" requirement, driven through the real
    // consume path rather than a private setter.
    @Test func eachReportedStateBecomesTheRenderedState() async {
        let cases: [CompanionAuthState] = [
            .starting,
            .configuring,
            .waitPhoneNumber,
            .waitCode(CompanionCodeInfo(phoneNumber: "+1 555 0100", codeLength: 5)),
            .waitQrConfirmation(link: "tg://login?token=abc"),
            .waitPassword(CompanionPasswordInfo(hint: "birthday", hasRecoveryEmail: true)),
            .ready,
            .loggingOut,
            .closing,
            .closed,
            .unsupported(kind: "authorizationStateWaitRegistration"),
        ]
        for expected in cases {
            let session = ScriptedAuthorizationSession()
            let backend = InMemoryCompanionBackend(session: { session })
            let model = AuthorizationViewModel(backend: backend)
            await model.begin()
            session.emit(expected)
            session.finish()
            await settle(model)
            #expect(model.state == expected)
        }
    }

    @Test func fullPhoneCodePasswordFlowProgresses() async {
        let session = ScriptedAuthorizationSession()
        let backend = InMemoryCompanionBackend(session: { session })
        let model = AuthorizationViewModel(backend: backend)
        await model.begin()
        session.emit(.configuring)
        session.emit(.waitPhoneNumber)
        session.emit(.waitCode(CompanionCodeInfo(phoneNumber: "+1 555 0100")))
        session.emit(.waitPassword(CompanionPasswordInfo(hint: "", hasRecoveryEmail: false)))
        session.emit(.ready)
        session.finish()
        await settle(model)
        #expect(model.isAuthorized)
        #expect(
            model.stateHistory.map(\.kind) == [
                "starting", "configuring", "wait-phone-number", "wait-code", "wait-password",
                "ready",
            ])
    }

    @Test func qrPathProgressesToReady() async {
        let session = ScriptedAuthorizationSession()
        let backend = InMemoryCompanionBackend(session: { session })
        let model = AuthorizationViewModel(backend: backend)
        await model.begin()
        session.emit(.waitPhoneNumber)
        session.emit(.waitQrConfirmation(link: "tg://login?token=a"))
        session.emit(.waitQrConfirmation(link: "tg://login?token=b"))  // refreshed link
        session.emit(.ready)
        session.finish()
        await settle(model)
        #expect(model.isAuthorized)
    }

    @Test func repeatedQrRestartCoalescesUntilTheExistingSessionCloses() async {
        let first = DelayedClosingAuthorizationSession(
            initialState: .waitQrConfirmation(link: "tg://login?token=synthetic-first"))
        let second = RecordingAuthorizationSession(states: [.waitPhoneNumber])
        let unexpectedThird = RecordingAuthorizationSession(states: [.waitPhoneNumber])
        let sessions = SessionSequence([first, second, unexpectedThird])
        let backend = InMemoryCompanionBackend(session: { sessions.next() })
        let model = AuthorizationViewModel(backend: backend)

        await model.begin()
        for _ in 0..<100 {
            if model.state.kind == "wait-qr-confirmation" { break }
            await Task.yield()
        }
        #expect(model.state.kind == "wait-qr-confirmation")

        var stalls = first.cancelStalls.makeAsyncIterator()
        let restart = Task { @MainActor in await model.begin() }
        _ = await stalls.next()
        #expect(model.isSubmitting)
        #expect(sessions.creationCount == 1)

        // A second keyboard or mouse activation while the close barrier is
        // pending is coalesced by the model, even if a caller bypasses the
        // view's disabled button state.
        await model.begin()
        #expect(model.isSubmitting)
        #expect(sessions.creationCount == 1)

        first.releaseAfterClose()
        await restart.value
        await model.waitForCompletion()
        #expect(sessions.creationCount == 2)
        #expect(second.startCount == 1)
        #expect(unexpectedThird.startCount == 0)
        #expect(!model.isSubmitting)
        #expect(model.state == .waitPhoneNumber)
    }

    @Test func restartAcknowledgesStartingBeforeStalledNamespaceTeardownTimesOut() async {
        let first = DelayedClosingAuthorizationSession(
            initialState: .waitQrConfirmation(link: "tg://login?token=stalled"))
        let backend = InMemoryCompanionBackend(session: { first })
        let model = AuthorizationViewModel(backend: backend, teardownTimeout: .milliseconds(20))

        await model.begin()
        for _ in 0..<100 where model.state.kind != "wait-qr-confirmation" {
            await Task.yield()
        }
        #expect(model.state.kind == "wait-qr-confirmation")

        var stalls = first.cancelStalls.makeAsyncIterator()
        let restart = Task { @MainActor in await model.begin() }
        _ = await stalls.next()
        #expect(model.state == .starting, "the click must acknowledge before teardown completes")
        #expect(model.isSubmitting)

        await restart.value
        #expect(model.unavailable == .timedOut)
        #expect(model.state == .idle)
        #expect(!model.isSubmitting)
        first.releaseAfterClose()
    }

    @Test func repeatedCancellationKeepsSubmissionStateUntilStalledTeardownExpires() async {
        let session = DelayedClosingAuthorizationSession(initialState: .waitPhoneNumber)
        let backend = InMemoryCompanionBackend(session: { session })
        let deadline = SuspendedAuthorizationDeadline()
        let model = AuthorizationViewModel(
            backend: backend,
            teardownTimeout: .milliseconds(20),
            teardownSleep: { duration in await deadline.sleep(for: duration) }
        )

        await model.begin()
        var stalls = session.cancelStalls.makeAsyncIterator()
        var deadlineWaits = deadline.waits.makeAsyncIterator()
        let firstCancellation = Task { @MainActor in await model.cancel() }
        _ = await stalls.next()
        _ = await deadlineWaits.next()

        // Invoke directly on MainActor after both waits are installed. Creating
        // another unstructured task here lets the deadline continuation race
        // that task under full-suite load and observes scheduling, not state.
        await model.cancel()

        #expect(model.isSubmitting, "a coalesced cancel must not clear the first cancel's progress")
        deadline.expire()
        await firstCancellation.value
        #expect(model.unavailable == .timedOut)
        #expect(model.state == .idle)
        #expect(!model.isSubmitting)
        session.releaseAfterClose()
    }
}

private final class SessionSequence: @unchecked Sendable {
    private let lock = NSLock()
    private var sessions: [any AuthorizationSession]

    init(_ sessions: [any AuthorizationSession]) {
        self.sessions = sessions
    }

    func next() -> any AuthorizationSession {
        lock.lock()
        defer { lock.unlock() }
        creationCountStorage += 1
        return sessions.removeFirst()
    }

    private var creationCountStorage = 0

    var creationCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return creationCountStorage
    }
}

private final class DelayedClosingAuthorizationSession: AuthorizationSession, @unchecked Sendable {
    let states: AsyncStream<CompanionAuthState>
    let cancelStalls: AsyncStream<Void>

    private let stateContinuation: AsyncStream<CompanionAuthState>.Continuation
    private let stallContinuation: AsyncStream<Void>.Continuation
    private let lock = NSLock()
    private var closeWaiter: CheckedContinuation<Void, Never>?
    private var released = false

    init(initialState: CompanionAuthState) {
        (states, stateContinuation) = AsyncStream.makeStream(of: CompanionAuthState.self)
        (cancelStalls, stallContinuation) =
            AsyncStream.makeStream(of: Void.self)
        stateContinuation.yield(initialState)
    }

    func start() async -> AuthStartResult { .started }

    func submit(_ input: CompanionAuthInput) async -> AuthSubmitResult { .accepted }

    func cancel() async -> ControlChannelUnavailable? {
        // Models the production ordering: TDLib accepts cancel first, then
        // reports terminal closure after the auth pump releases its slot.
        await withCheckedContinuation { continuation in
            lock.lock()
            if released {
                lock.unlock()
                continuation.resume()
            } else {
                closeWaiter = continuation
                lock.unlock()
                stallContinuation.yield(())
            }
        }
        return nil
    }

    func releaseAfterClose() {
        stateContinuation.yield(.closed)
        stateContinuation.finish()
        stallContinuation.finish()
        lock.lock()
        released = true
        let waiter = closeWaiter
        closeWaiter = nil
        lock.unlock()
        waiter?.resume()
    }
}

private final class SuspendedAuthorizationDeadline: @unchecked Sendable {
    let waits: AsyncStream<Void>

    private let waitContinuation: AsyncStream<Void>.Continuation
    private let lock = NSLock()
    private var sleepers: [CheckedContinuation<Void, Never>] = []
    private var expired = false

    init() {
        (waits, waitContinuation) = AsyncStream.makeStream(of: Void.self)
    }

    func sleep(for _: Duration) async {
        await withCheckedContinuation { continuation in
            let shouldResume = lock.withLock {
                guard !expired else { return true }
                sleepers.append(continuation)
                return false
            }
            if shouldResume {
                continuation.resume()
            } else {
                waitContinuation.yield(())
            }
        }
    }

    func expire() {
        let pendingSleepers = lock.withLock {
            expired = true
            let pendingSleepers = sleepers
            sleepers.removeAll()
            return pendingSleepers
        }
        for sleeper in pendingSleepers {
            sleeper.resume()
        }
    }
}

private final class RecordingAuthorizationSession: AuthorizationSession, @unchecked Sendable {
    let states: AsyncStream<CompanionAuthState>
    private let lock = NSLock()
    private var inputKinds: [String] = []
    private var startCountStorage = 0

    init(states: [CompanionAuthState]) {
        self.states = AsyncStream { continuation in
            for state in states { continuation.yield(state) }
            continuation.finish()
        }
    }

    func start() async -> AuthStartResult {
        recordStart()
        return .started
    }

    func submit(_ input: CompanionAuthInput) async -> AuthSubmitResult {
        record(input.kind)
        return .accepted
    }

    func cancel() async -> ControlChannelUnavailable? { nil }

    private func recordStart() {
        lock.lock()
        startCountStorage += 1
        lock.unlock()
    }

    private func record(_ kind: String) {
        lock.lock()
        inputKinds.append(kind)
        lock.unlock()
    }

    var submittedInputKinds: [String] {
        lock.lock()
        defer { lock.unlock() }
        return inputKinds
    }

    var startCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return startCountStorage
    }
}

@MainActor
@Suite struct AuthorizationInputTests {
    @Test func unavailableChannelIsSurfacedNotFailed() async {
        let backend = InMemoryCompanionBackend(
            session: { UnavailableAuthorizationSession(reason: .notWired) })
        let model = AuthorizationViewModel(backend: backend)
        await model.begin()
        #expect(model.unavailable == .notWired)
        #expect(model.state == .idle)
    }

    @Test func signInUnavailableReasonsRemainDistinctAndActionable() async {
        let cases: [(ControlChannelUnavailable, String)] = [
            (.agentNotRunning, "The GramDrive agent is not running. Open GramDrive and try again."),
            (.busy, "A sign-in is already in progress — try again in a moment."),
            (.dropped, "Lost the connection to the GramDrive agent. Try signing in again."),
        ]
        for (reason, message) in cases {
            let backend = InMemoryCompanionBackend(
                session: { UnavailableAuthorizationSession(reason: reason) })
            let model = AuthorizationViewModel(backend: backend)
            await model.begin()
            #expect(model.unavailable == reason)
            #expect(model.unavailable?.message == message)
        }
    }

    @Test func aRejectionIsClassifiedWithAdvice() async {
        let session = ScriptedAuthorizationSession(onSubmit: { input in
            if case .submitCode = input { return .rejected(.expiredCode) }
            return .accepted
        })
        let backend = InMemoryCompanionBackend(session: { session })
        let model = AuthorizationViewModel(backend: backend)
        await model.begin()
        session.emit(.waitCode(CompanionCodeInfo(phoneNumber: "+1 555 0100")))
        session.finish()
        await model.waitForCompletion()  // waitCode is applied before we submit
        await model.submit(.submitCode("00000"))
        #expect(model.lastRejection == .expiredCode)
        #expect(model.advice == .requestNewCode)
    }

    @Test func aStructurallyInvalidInputIsRefusedLocally() async {
        let session = ScriptedAuthorizationSession(onSubmit: { _ in
            Issue.record("submit must not reach the session for an invalid input")
            return .accepted
        })
        let backend = InMemoryCompanionBackend(session: { session })
        let model = AuthorizationViewModel(backend: backend)
        await model.begin()
        session.emit(.waitPhoneNumber)
        session.finish()
        await model.waitForCompletion()
        // A code is not valid while waiting for a phone number.
        await model.submit(.submitCode("123"))
        #expect(model.lastInvalidInput == .submitCode("123"))
        #expect(model.state == .waitPhoneNumber)
    }

    @Test func cancelIsValidEverywhereButClosed() {
        #expect(CompanionAuthInput.cancel.isValid(in: .waitPassword(
            CompanionPasswordInfo(hint: "", hasRecoveryEmail: false))))
        #expect(CompanionAuthInput.cancel.isValid(in: .unsupported(kind: "x")))
        #expect(!CompanionAuthInput.cancel.isValid(in: .closed))
    }

    @Test func adviceMappingMatchesTheCoreVocabulary() {
        #expect(CompanionAuthRejection.network.advice == .retrySameInput)
        #expect(CompanionAuthRejection.invalidPassword.advice == .reviseInput)
        #expect(CompanionAuthRejection.expiredCode.advice == .requestNewCode)
        #expect(
            CompanionAuthRejection.rateLimited(retryAfterSeconds: 30).advice
                == .waitThenRetry(afterSeconds: 30))
        #expect(CompanionAuthRejection.phoneNumberBanned.advice == .abort)
        #expect(CompanionAuthRejection.sessionEnded.advice == .abort)
    }
}
