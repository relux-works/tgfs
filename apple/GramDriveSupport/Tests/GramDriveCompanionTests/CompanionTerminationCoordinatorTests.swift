import Darwin
import Foundation
import GramDriveAgentCore
@testable import GramDriveCompanion
import GramDriveSupport
import Testing

@Suite struct UpdateTerminationIntentProjectionTests {
    @Test func ordinaryQuitWithoutAStagedUpdateRemainsAUserQuit() {
        #expect(
            CompanionTerminationCoordinator.Intent.fromPendingUpdateBuild(nil)
                == .userQuit)
    }

    @Test func stagedUpdatePreservesTheTargetBuildWithoutChangingRelaunchSemantics() {
        #expect(
            CompanionTerminationCoordinator.Intent.fromPendingUpdateBuild("137")
                == .update(targetBuild: "137"))
    }
}

private final class TerminationProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var prepared: [ControlTerminationRequest] = []
    private var cancelled: [ControlTerminationRequest] = []
    private var committed: [ControlTerminationRequest] = []
    private var prepareOutcomes: [CommandOutcome]
    private var readings: [HealthReadout]
    private var healthRequests = 0
    private let fallback: HealthReadout
    private var onPrepare: (@Sendable (ControlTerminationRequest) -> Void)?

    init(
        readings: [HealthReadout],
        prepareOutcomes: [CommandOutcome] = [.completed],
        fallback: HealthReadout = .running(CompanionTerminationCoordinatorTests.drainingSnapshot()),
        onPrepare: (@Sendable (ControlTerminationRequest) -> Void)? = nil
    ) {
        self.readings = readings
        self.prepareOutcomes = prepareOutcomes
        self.fallback = fallback
        self.onPrepare = onPrepare
    }

    func prepare(_ request: ControlTerminationRequest) async -> CommandOutcome {
        let (outcome, handler) = lock.withLock {
            prepared.append(request)
            let outcome = prepareOutcomes.isEmpty ? .completed : prepareOutcomes.removeFirst()
            return (outcome, onPrepare)
        }
        handler?(request)
        return outcome
    }

    func setOnPrepare(_ handler: @escaping @Sendable (ControlTerminationRequest) -> Void) {
        lock.withLock { onPrepare = handler }
    }

    func cancel(_ request: ControlTerminationRequest) async -> CommandOutcome {
        lock.withLock { cancelled.append(request) }
        return .completed
    }

    func commit(_ request: ControlTerminationRequest) async -> CommandOutcome {
        lock.withLock { committed.append(request) }
        return .completed
    }

    func health() async -> HealthReadout {
        lock.withLock {
            healthRequests += 1
            if readings.isEmpty, healthRequests == 1, onPrepare != nil {
                return .running(CompanionTerminationCoordinatorTests.drainingSnapshot(state: .running))
            }
            return readings.isEmpty ? fallback : readings.removeFirst()
        }
    }

    func append(_ reading: HealthReadout) {
        lock.withLock { readings.append(reading) }
    }

    var requests: [ControlTerminationRequest] {
        lock.lock()
        defer { lock.unlock() }
        return prepared
    }

    var cancellationRequests: [ControlTerminationRequest] {
        lock.lock()
        defer { lock.unlock() }
        return cancelled
    }

    var commitmentRequests: [ControlTerminationRequest] {
        lock.lock()
        defer { lock.unlock() }
        return committed
    }

    var healthRequestCount: Int {
        lock.withLock { healthRequests }
    }
}

private final class SuspendedCancellationProbe: @unchecked Sendable {
    private let lock = NSLock()
    let drainReconciliationStarted: AsyncStream<Void>
    let cancellationStarted: AsyncStream<Void>
    let cancellationJoined: AsyncStream<Void>

    private let drainContinuation: AsyncStream<Void>.Continuation
    private let cancellationContinuation: AsyncStream<Void>.Continuation
    private let joinContinuation: AsyncStream<Void>.Continuation
    private var recorded: [ControlTerminationRequest] = []
    private var cancellationInvocations = 0
    private var prepared = false
    private var drainWaiter: CheckedContinuation<Void, Never>?
    private var cancellationWaiter: CheckedContinuation<Void, Never>?
    private var currentSnapshot = CompanionTerminationCoordinatorTests.drainingSnapshot(
        state: .running
    )

    init() {
        (drainReconciliationStarted, drainContinuation) = AsyncStream.makeStream(of: Void.self)
        (cancellationStarted, cancellationContinuation) = AsyncStream.makeStream(of: Void.self)
        (cancellationJoined, joinContinuation) = AsyncStream.makeStream(of: Void.self)
    }

    var requests: [ControlTerminationRequest] {
        lock.withLock { recorded }
    }

    var cancellationInvocationCount: Int {
        lock.withLock { cancellationInvocations }
    }

    func prepare(_ request: ControlTerminationRequest) async -> CommandOutcome {
        lock.withLock {
            recorded.append(request)
            currentSnapshot.terminationRequestID = request.requestID
            currentSnapshot.state = .draining
            prepared = true
        }
        return .completed
    }

    func cancel(_ request: ControlTerminationRequest) async -> CommandOutcome {
        let invocation = lock.withLock {
            cancellationInvocations += 1
            return cancellationInvocations
        }
        if invocation == 1 {
            await withCheckedContinuation { continuation in
                lock.withLock { cancellationWaiter = continuation }
                cancellationContinuation.yield(())
            }
        }
        lock.withLock {
            recorded.append(request)
            currentSnapshot.terminationRequestID = request.requestID
            currentSnapshot.state = .terminationCancelled
        }
        return .completed
    }

    func health() async -> HealthReadout {
        let shouldHold = lock.withLock { prepared && cancellationInvocations == 0 }
        if shouldHold {
            await withCheckedContinuation { continuation in
                lock.withLock { drainWaiter = continuation }
                drainContinuation.yield(())
            }
        }
        return lock.withLock { .running(currentSnapshot) }
    }

    func releaseDrainReconciliation() {
        let waiter = lock.withLock {
            let waiter = drainWaiter
            drainWaiter = nil
            return waiter
        }
        waiter?.resume()
    }

    func releaseCancellation() {
        let waiter = lock.withLock {
            let waiter = cancellationWaiter
            cancellationWaiter = nil
            return waiter
        }
        waiter?.resume()
    }

    func observeCancellationJoin() {
        joinContinuation.yield(())
    }
}

@MainActor struct CompanionTerminationCoordinatorTests {
    private nonisolated static let fixtureIdentity = AgentProcessIdentity(
        instanceID: UUID(), pid: Int32.max, kernelStartSeconds: 1, kernelStartMicroseconds: 1
    )

    @Test func exactProcessObservationDistinguishesDeathReuseAndIndeterminateReads() {
        let identity = Self.fixtureIdentity
        #expect(
            AgentProcessIdentity.classifyObservation(
                expected: identity,
                observedStartSeconds: identity.kernelStartSeconds,
                observedStartMicroseconds: identity.kernelStartMicroseconds
            ) == .matching
        )
        #expect(
            AgentProcessIdentity.classifyObservation(
                expected: identity,
                observedStartSeconds: identity.kernelStartSeconds + 1,
                observedStartMicroseconds: identity.kernelStartMicroseconds
            ) == .replaced
        )
        #expect(
            AgentProcessIdentity.classifyObservation(
                expected: identity, observedStartSeconds: nil, observedStartMicroseconds: nil
            ) == .indeterminate
        )
        #expect(AgentProcessObservation.absent.provesCapturedProcessExited)
        #expect(AgentProcessObservation.replaced.provesCapturedProcessExited)
        #expect(!AgentProcessObservation.indeterminate.provesCapturedProcessExited)
    }

    @Test func applicationReplyGateRepliesExactlyOnceAcrossJoinedAndCancelledPaths() {
        let gate = ApplicationTerminationReplyGate()
        #expect(gate.begin())
        #expect(!gate.begin())
        #expect(gate.takeReply(false) == false)
        #expect(gate.takeReply(true) == nil)
        #expect(gate.begin())
        #expect(gate.takeReply(true) == true)
    }

    @Test func applicationTerminationDriverRepliesOnceForTheRealDelegateSeam() async throws {
        var replies: [Bool] = []
        let driver = ApplicationTerminationRequestDriver(
            requestTermination: { _ in
                try? await Task.sleep(for: .milliseconds(10))
                return true
            },
            cancelTermination: { false }
        )

        #expect(driver.applicationShouldTerminate(intent: .userQuit) { replies.append($0) })
        #expect(!driver.applicationShouldTerminate(intent: .userQuit) { replies.append($0) })
        driver.cancelPendingTermination { replies.append($0) }
        try await Task.sleep(for: .milliseconds(20))

        #expect(replies == [false])
        #expect(!driver.isPending)
    }

    @Test func updateDrainWaitsForTheAgentToDisappear() async {
        let running = AgentHealthSnapshot(
            payloadVersion: 4,
            agentVersion: "0.1.0",
            bundleVersion: "136",
            contractVersion: "1.0.0",
            pid: 1,
            processIdentity: Self.drainingSnapshot().processIdentity,
            state: .draining,
            startedAtMs: 1,
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
            finderContentState: .ready,
            finderFirstPageItemCount: 0
        )
        let probe = TerminationProbe(readings: [], fallback: .timedOut)
        probe.setOnPrepare { request in
            var draining = running
            draining.terminationRequestID = request.requestID
            probe.append(.running(draining))
            probe.append(.notRunning)
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            health: { await probe.health() },
            recoverCurrentBuild: { true },
            pollInterval: .milliseconds(1),
            timeout: .seconds(1)
        )

        #expect(await coordinator.requestTermination(.update(targetBuild: "137")))
        #expect(probe.requests.count == 1)
        #expect(probe.requests.first?.reason == .update)
        #expect(probe.requests.first?.targetBuild == "137")
    }

    @Test func joinedRequestsIssueOneControlDrain() async {
        let probe = TerminationProbe(readings: [
            .running(Self.drainingSnapshot(state: .running)), .notRunning,
        ])
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            health: { await probe.health() }
        )

        async let first = coordinator.requestTermination(.userQuit)
        async let second = coordinator.requestTermination(.update(targetBuild: "137"))
        #expect(await first)
        #expect(await second)
        #expect(probe.requests.count == 1)
        // The first actor scheduled by Swift owns the shared drain. `async let`
        // has no source-order execution guarantee, so assert the coalescing
        // invariant rather than accidentally making the test scheduler-bound.
        #expect(probe.requests.first?.reason == .userQuit || probe.requests.first?.reason == .update)
    }

    @Test func aQuitWithoutAnAgentIsImmediatelyAllowed() async {
        let coordinator = CompanionTerminationCoordinator(
            prepare: { _ in .unavailable(.agentNotRunning) },
            health: { .notRunning }
        )

        #expect(await coordinator.requestTermination(.userQuit))
    }

    @Test func timeoutCancelsAndAllowsASubsequentRetryCycle() async {
        let probe = TerminationProbe(readings: [], fallback: .timedOut)
        probe.setOnPrepare { request in
            var draining = Self.drainingSnapshot()
            draining.terminationRequestID = request.requestID
            probe.append(.running(draining))
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            cancel: { request in
                let outcome = await probe.cancel(request)
                var cancelled = Self.drainingSnapshot(state: .terminationCancelled)
                cancelled.terminationRequestID = request.requestID
                probe.append(.running(cancelled))
                return outcome
            },
            health: { await probe.health() },
            pollInterval: .milliseconds(1),
            timeout: .milliseconds(5),
            cancellationTimeout: .seconds(1)
        )

        #expect(!(await coordinator.requestTermination(.userQuit)))
        #expect(coordinator.lastFailureMessage?.contains("try quitting again") == true)
        #expect(probe.cancellationRequests.count == 1)
        #expect(probe.cancellationRequests.first?.action == .cancel)

        // A reply-false cycle is terminal for that request, not for the
        // coordinator: a later quit can request one fresh bounded drain.
        probe.append(.running(Self.drainingSnapshot(state: .running)))
        #expect(!(await coordinator.requestTermination(.userQuit)))
        #expect(probe.requests.count == 2)
    }

    @Test func cancellationResponseLossUsesTheFiniteLeaseSafeFalseReply() async {
        let probe = TerminationProbe(readings: [], fallback: .timedOut)
        probe.setOnPrepare { request in
            var draining = Self.drainingSnapshot()
            draining.terminationRequestID = request.requestID
            probe.append(.running(draining))
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            cancel: { request in
                let outcome = await probe.cancel(request)
                for _ in 0 ..< 12 {
                    probe.append(.timedOut)
                }
                for _ in 0 ..< 3 {
                    probe.append(.error("health socket stalled"))
                }
                return outcome == .completed ? .unavailable(.dropped) : outcome
            },
            health: { await probe.health() },
            recoverCurrentBuild: { true },
            pollInterval: .milliseconds(1),
            timeout: .milliseconds(2),
            cancellationTimeout: .milliseconds(2)
        )
        let replyGate = ApplicationTerminationReplyGate()

        #expect(replyGate.begin())
        let allowed = await coordinator.requestTermination(.userQuit)
        #expect(!allowed)
        #expect(probe.healthRequestCount >= 2)
        #expect(probe.healthRequestCount < 16)
        #expect(probe.commitmentRequests.isEmpty)
        #expect(replyGate.takeReply(allowed) == false)
        #expect(replyGate.takeReply(true) == nil)
        #expect(coordinator.lastFailureMessage?.contains("restored the current agent") == true)
    }

    @Test func preparedDrainCommitsOnlyAfterCorrelatedReady() async {
        let probe = TerminationProbe(readings: [])
        probe.setOnPrepare { request in
            var ready = Self.drainingSnapshot(state: .terminationReady)
            ready.terminationRequestID = request.requestID
            probe.append(.running(ready))
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            commit: { await probe.commit($0) },
            health: { await probe.health() },
            pollInterval: .milliseconds(1),
            timeout: .milliseconds(2),
            cancellationTimeout: .milliseconds(2)
        )
        let replyGate = ApplicationTerminationReplyGate()

        #expect(replyGate.begin())
        let allowed = await coordinator.requestTermination(.update(targetBuild: "137"))
        #expect(allowed)
        #expect(probe.commitmentRequests.count == 1)
        #expect(probe.commitmentRequests.first?.action == .commit)
        #expect(probe.commitmentRequests.first?.requestID == probe.requests.first?.requestID)
        #expect(replyGate.takeReply(allowed) == true)
        #expect(replyGate.takeReply(false) == nil)
    }

    @Test func rejectedCommitCancelsThePreparedDrainAndNeverRepliesTrue() async {
        let probe = TerminationProbe(readings: [])
        probe.setOnPrepare { request in
            var ready = Self.drainingSnapshot(state: .terminationReady)
            ready.terminationRequestID = request.requestID
            probe.append(.running(ready))
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            cancel: { request in
                let outcome = await probe.cancel(request)
                var cancelled = Self.drainingSnapshot(state: .terminationCancelled)
                cancelled.terminationRequestID = request.requestID
                probe.append(.running(cancelled))
                return outcome
            },
            commit: { _ in .failed(.sourceUnavailable) },
            health: { await probe.health() },
            pollInterval: .milliseconds(1),
            timeout: .seconds(1),
            cancellationTimeout: .seconds(1)
        )

        #expect(!(await coordinator.requestTermination(.update(targetBuild: "137"))))
        #expect(probe.cancellationRequests.count == 1)
    }

    @Test func droppedCommitResponseWaitsForTheEndpointToDisappearBeforeAllowing() async {
        let probe = TerminationProbe(readings: [])
        probe.setOnPrepare { request in
            var ready = Self.drainingSnapshot(state: .terminationReady)
            ready.terminationRequestID = request.requestID
            var stopped = ready
            stopped.state = .stopped
            probe.append(.running(ready))
            probe.append(.running(stopped))
            probe.append(.notRunning)
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            commit: { _ in .unavailable(.dropped) },
            health: { await probe.health() },
            pollInterval: .milliseconds(1),
            timeout: .seconds(1),
            cancellationTimeout: .seconds(1)
        )
        let replyGate = ApplicationTerminationReplyGate()

        #expect(replyGate.begin())
        let allowed = await coordinator.requestTermination(.userQuit)
        #expect(allowed)
        #expect(probe.healthRequestCount >= 3)
        #expect(replyGate.takeReply(allowed) == true)
        #expect(replyGate.takeReply(false) == nil)
    }

    @Test func liveCompositionRoutesPrepareCancelAndCommitWithTheSameRequestIdentity() async throws {
        let layout = try Self.tempLayout()
        let transport = LiveTerminationTransport()
        let health = try AgentHealthServer.start(socketURL: layout.healthSocket) {
            transport.snapshot
        }
        let control = try ControlServer.start(
            socketURL: layout.controlSocket,
            handlers: ControlServerHandlers(
                status: { transport.snapshot },
                reloadSettings: { AgentSettings() },
                prepareForTermination: { transport.record($0) },
                acceptTerminationCommit: { transport.acceptCommit($0) },
                finishAcceptedTerminationCommit: { _ in health.stop() }
            )
        )
        let hydration = try SocketListener(socketURL: layout.hydrationSocket)
        defer {
            hydration.stop()
            control.stop()
            health.stop()
            try? FileManager.default.removeItem(at: layout.dataRoot)
        }

        let coordinator = CompanionTerminationCoordinator.live(
            layout: layout,
            healthTimeout: .seconds(1),
            pollInterval: .milliseconds(1),
            timeout: .seconds(1),
            cancellationTimeout: .seconds(1)
        )
        let replies = TerminationReplyProbe()
        let driver = ApplicationTerminationRequestDriver(
            requestTermination: { intent in await coordinator.requestTermination(intent) },
            cancelTermination: { await coordinator.cancelTermination() }
        )

        #expect(driver.applicationShouldTerminate(intent: .update(targetBuild: "137")) {
            replies.append($0)
        })
        #expect(!driver.applicationShouldTerminate(intent: .userQuit) { replies.append($0) })
        try await Self.waitUntil { replies.values.count == 1 }

        let committed = transport.requests
        #expect(replies.values == [true])
        #expect(committed.count == 2)
        #expect(committed.map(\.action) == [.prepare, .commit])
        guard committed.count == 2 else { return }
        #expect(committed[0].requestID == committed[1].requestID)
        #expect(committed[0].targetBuild == "137")

        transport.resetForCancellation()
        // The first committed handoff deliberately removed its health
        // endpoint before the true reply. A fresh serving stand-in models
        // the next launch for the independent Keep GramDrive Open cycle.
        let recoveredHealth = try AgentHealthServer.start(socketURL: layout.healthSocket) {
            transport.snapshot
        }
        defer { recoveredHealth.stop() }
        #expect(driver.applicationShouldTerminate(intent: .userQuit) { replies.append($0) })
        try await Self.waitUntil { transport.requests.count >= 3 }
        driver.cancelPendingTermination { replies.append($0) }
        try await Self.waitUntil { replies.values.count == 2 }

        let allRequests = transport.requests
        let cancelled = Array(allRequests.suffix(2))
        #expect(replies.values == [true, false])
        #expect(cancelled.map(\.action) == [.prepare, .cancel])
        guard cancelled.count == 2 else { return }
        #expect(cancelled[0].requestID == cancelled[1].requestID)
    }

    @Test func realAgentCoordinatorRepliesOnceOnlyAfterTheObservedProcessExits() async throws {
        let layout = try Self.tempLayout()
        let agent = try Self.startAgent(layout: layout)
        defer {
            if agent.isRunning { agent.terminate() }
            try? FileManager.default.removeItem(at: layout.dataRoot)
        }
        let initial = try Self.waitForAgentHealth(socketURL: layout.healthSocket)
        let identity = try #require(initial.processIdentity)
        let coordinator = CompanionTerminationCoordinator.live(
            layout: layout,
            healthTimeout: .milliseconds(100),
            pollInterval: .milliseconds(5),
            timeout: .seconds(3),
            cancellationTimeout: .seconds(3)
        )
        let replies = TerminationReplyProbe()
        let driver = ApplicationTerminationRequestDriver(
            requestTermination: { intent in await coordinator.requestTermination(intent) },
            cancelTermination: { await coordinator.cancelTermination() }
        )

        #expect(driver.applicationShouldTerminate(intent: .userQuit) { replies.append($0) })
        try await Self.waitUntil { replies.values.count == 1 }
        #expect(replies.values == [true])
        #expect(!Self.processStillMatches(identity))
    }

    @Test func anUncorrelatedReadyStateCannotCommitANewerDrain() async {
        let probe = TerminationProbe(readings: [])
        probe.setOnPrepare { request in
            var stale = Self.drainingSnapshot(state: .terminationReady)
            stale.terminationRequestID = UUID()
            probe.append(.running(stale))
            var fresh = Self.drainingSnapshot(state: .terminationReady)
            fresh.terminationRequestID = request.requestID
            probe.append(.running(fresh))
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            commit: { await probe.commit($0) },
            health: { await probe.health() },
            pollInterval: .milliseconds(1),
            timeout: .seconds(1)
        )

        #expect(await coordinator.requestTermination(.userQuit))
        #expect(probe.commitmentRequests.count == 1)
        #expect(probe.commitmentRequests.first?.requestID == probe.requests.first?.requestID)
    }

    @Test func abandonedDrainCancelsWithoutTreatingHealthAsSuccessfulExit() async {
        let probe = TerminationProbe(readings: [])
        probe.setOnPrepare { request in
            var cancelled = Self.drainingSnapshot(state: .terminationCancelled)
            cancelled.terminationRequestID = request.requestID
            probe.append(.running(cancelled))
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            health: { await probe.health() }
        )

        #expect(!(await coordinator.requestTermination(.update(targetBuild: "137"))))
        #expect(coordinator.lastFailureMessage?.contains("Force Quit") == true)
    }

    @Test func lostAcknowledgementReconcilesTheAcceptedDrainBeforeReplying() async {
        let probe = TerminationProbe(
            readings: [], prepareOutcomes: [.unavailable(.dropped)]
        )
        probe.setOnPrepare { request in
            var draining = Self.drainingSnapshot()
            draining.terminationRequestID = request.requestID
            probe.append(.running(draining))
            probe.append(.notRunning)
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            health: { await probe.health() }
        )

        #expect(await coordinator.requestTermination(.userQuit))
        #expect(probe.requests.count == 1)
    }

    @Test func explicitCancellationJoinsTheDrainAndRepliesFalseAfterRecovery() async {
        let probe = TerminationProbe(readings: [], fallback: .timedOut)
        probe.setOnPrepare { request in
            var draining = Self.drainingSnapshot()
            draining.terminationRequestID = request.requestID
            probe.append(.running(draining))
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            cancel: { request in
                var recovered = Self.drainingSnapshot(state: .terminationCancelled)
                recovered.terminationRequestID = request.requestID
                probe.append(.running(recovered))
                return .completed
            },
            health: { await probe.health() },
            pollInterval: .milliseconds(1),
            timeout: .seconds(1)
        )

        async let request = coordinator.requestTermination(.userQuit)
        try? await Task.sleep(for: .milliseconds(5))
        #expect(!(await coordinator.cancelTermination()))
        #expect(!(await request))
        #expect(probe.requests.count == 1)
    }

    @Test func explicitCancellationSharesOneInFlightCommandWithDrainReconciliation() async {
        let probe = SuspendedCancellationProbe()
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            cancel: { await probe.cancel($0) },
            health: { await probe.health() },
            pollInterval: .milliseconds(1),
            timeout: .seconds(1),
            cancellationTimeout: .seconds(1),
            explicitCancellationJoinObserver: { probe.observeCancellationJoin() }
        )

        async let terminationAllowed = coordinator.requestTermination(.userQuit)
        var drainStarts = probe.drainReconciliationStarted.makeAsyncIterator()
        _ = await drainStarts.next()
        async let cancellationAllowed = coordinator.cancelTermination()
        var cancellationStarts = probe.cancellationStarted.makeAsyncIterator()
        var cancellationJoins = probe.cancellationJoined.makeAsyncIterator()
        _ = await cancellationStarts.next()
        probe.releaseDrainReconciliation()
        _ = await cancellationJoins.next()
        #expect(probe.cancellationInvocationCount == 1)
        probe.releaseCancellation()

        let cancellationResult = await cancellationAllowed
        let terminationResult = await terminationAllowed
        #expect(!cancellationResult)
        #expect(!terminationResult)
        #expect(probe.cancellationInvocationCount == 1)
        #expect(probe.requests.map(\.action) == [.prepare, .cancel])
        guard probe.requests.count == 2 else { return }
        #expect(probe.requests[0].requestID == probe.requests[1].requestID)
    }

    @Test func aLiveStoppedSnapshotWaitsForEndpointDisappearance() async {
        let probe = TerminationProbe(readings: [])
        probe.setOnPrepare { request in
            var ready = Self.drainingSnapshot(state: .terminationReady)
            ready.terminationRequestID = request.requestID
            var stopped = ready
            stopped.state = .stopped
            probe.append(.running(ready))
            probe.append(.running(stopped))
            probe.append(.notRunning)
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            commit: { await probe.commit($0) },
            health: { await probe.health() },
            pollInterval: .milliseconds(1),
            timeout: .seconds(1)
        )

        #expect(await coordinator.requestTermination(.userQuit))
        #expect(probe.commitmentRequests.count == 1)
    }

    @Test func staleCancellationCannotCancelANewerRetryDrain() async {
        let probe = TerminationProbe(readings: [])
        probe.setOnPrepare { request in
            let requests = probe.requests
            if requests.count == 1 {
                var cancelled = Self.drainingSnapshot(state: .terminationCancelled)
                cancelled.terminationRequestID = request.requestID
                probe.append(.running(cancelled))
            } else {
                var stale = Self.drainingSnapshot(state: .terminationCancelled)
                stale.terminationRequestID = requests[0].requestID
                var fresh = Self.drainingSnapshot()
                fresh.terminationRequestID = request.requestID
                probe.append(.running(stale))
                probe.append(.running(fresh))
                probe.append(.notRunning)
            }
        }
        let coordinator = CompanionTerminationCoordinator(
            prepare: { await probe.prepare($0) },
            health: { await probe.health() },
            pollInterval: .milliseconds(1),
            timeout: .seconds(1)
        )

        #expect(!(await coordinator.requestTermination(.userQuit)))
        #expect(await coordinator.requestTermination(.userQuit))
        #expect(probe.requests.count == 2)
    }

    fileprivate nonisolated static func drainingSnapshot(
        state: AgentRunState = .draining
    ) -> AgentHealthSnapshot {
        AgentHealthSnapshot(
            payloadVersion: 3,
            agentVersion: "0.1.0",
            bundleVersion: "136",
            contractVersion: "1.0.0",
            pid: 1,
            // Synthetic health must never identify the XCTest process: the
            // production coordinator registers a real process-exit witness
            // before commit and its TERM/KILL fallback is intentionally exact.
            processIdentity: fixtureIdentity,
            state: state,
            servingGeneration: 2,
            transferAdmissionOpen: true,
            namespaceOwnersRestored: true,
            startedAtMs: 1,
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
            finderContentState: .ready,
            finderFirstPageItemCount: 0
        )
    }

    private nonisolated static func tempLayout() throws -> AgentRuntimeLayout {
        let dataRoot = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-termination-\(UUID().uuidString)")
        let layout = AgentRuntimeLayout(dataRoot: dataRoot)
        try layout.ensureDirectories()
        return layout
    }

    private nonisolated static func waitUntil(
        _ predicate: @escaping @Sendable () -> Bool
    ) async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < deadline {
            if predicate() { return }
            try await Task.sleep(for: .milliseconds(1))
        }
        Issue.record("condition did not become true within the bounded test wait")
    }

    private nonisolated static func startAgent(layout: AgentRuntimeLayout) throws -> Process {
        let source = URL(fileURLWithPath: #filePath)
        let packageRoot = source
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let executable = try FileManager.default.contentsOfDirectory(
            at: packageRoot.appendingPathComponent(".build", isDirectory: true),
            includingPropertiesForKeys: nil
        )
        .map { $0.appendingPathComponent("debug/gramdrive-agent") }
        .first { FileManager.default.isExecutableFile(atPath: $0.path) }
        guard let executable else { throw RealAgentTestError.executableMissing }
        let process = Process()
        process.executableURL = executable
        process.arguments = [
            "run", "--data-root", layout.dataRoot.path,
            "--drain-grace-ms", "25", "--drain-cancel-wait-ms", "25",
        ]
        process.standardOutput = Pipe()
        process.standardError = Pipe()
        try process.run()
        return process
    }

    private nonisolated static func waitForAgentHealth(socketURL: URL) throws -> AgentHealthSnapshot {
        let deadline = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < deadline {
            if let snapshot = try? AgentHealthClient.fetch(socketURL: socketURL, timeout: .milliseconds(100)) {
                return snapshot
            }
            Thread.sleep(forTimeInterval: 0.02)
        }
        throw RealAgentTestError.healthUnavailable
    }

    private nonisolated static func processStillMatches(_ identity: AgentProcessIdentity) -> Bool {
        var info = proc_bsdinfo()
        let count = proc_pidinfo(
            identity.pid, PROC_PIDTBSDINFO, 0, &info, Int32(MemoryLayout<proc_bsdinfo>.size)
        )
        return count == MemoryLayout<proc_bsdinfo>.size
            && Int64(info.pbi_start_tvsec) == identity.kernelStartSeconds
            && Int64(info.pbi_start_tvusec) == identity.kernelStartMicroseconds
    }

    private enum RealAgentTestError: Error {
        case executableMissing
        case healthUnavailable
    }
}

private final class TerminationReplyProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var recorded: [Bool] = []

    var values: [Bool] {
        lock.withLock { recorded }
    }

    func append(_ value: Bool) {
        lock.withLock { recorded.append(value) }
    }
}

/// A minimal hydration protocol fixture. It intentionally rejects an
/// incompatible protocol version after reading the request, which is the
/// same non-fetching serving probe used by the production rollback check.
private final class SocketListener {
    private let descriptor: Int32
    private let path: String

    init(socketURL: URL) throws {
        path = socketURL.path
        descriptor = socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else {
            throw UnixSocketError.failed(operation: "socket", code: errno)
        }
        unlink(path)
        do {
            try UnixSocketAddress.bind(descriptor: descriptor, path: path)
            guard listen(descriptor, 1) == 0 else {
                throw UnixSocketError.failed(operation: "listen", code: errno)
            }
            let listeningDescriptor = descriptor
            DispatchQueue.global(qos: .utility).async {
                while true {
                    let connection = accept(listeningDescriptor, nil, nil)
                    guard connection >= 0 else { return }
                    var request = [UInt8](repeating: 0, count: 4096)
                    if read(connection, &request, request.count) > 0,
                       let response = try? HydrationWire.encodeLine(
                           HydrationEvent.failure(
                               HydrationFailure(
                                   category: .internalError,
                                   detail: "protocol version mismatch"
                               )
                           )
                       )
                    {
                        _ = response.withUnsafeBytes { bytes in
                            Darwin.write(connection, bytes.baseAddress, bytes.count)
                        }
                    }
                    Darwin.close(connection)
                }
            }
        } catch {
            Darwin.close(descriptor)
            throw error
        }
    }

    func stop() {
        Darwin.close(descriptor)
        unlink(path)
    }
}

private final class LiveTerminationTransport: @unchecked Sendable {
    private let lock = NSLock()
    private var recorded: [ControlTerminationRequest] = []
    private var currentSnapshot = CompanionTerminationCoordinatorTests.drainingSnapshot(state: .running)
    private var preparesBecomeReady = true

    var snapshot: AgentHealthSnapshot {
        lock.withLock { currentSnapshot }
    }

    var requests: [ControlTerminationRequest] {
        lock.withLock { recorded }
    }

    func record(_ request: ControlTerminationRequest) {
        lock.withLock {
            recorded.append(request)
            currentSnapshot.terminationRequestID = request.requestID
            switch request.action {
            case .prepare:
                currentSnapshot.state = preparesBecomeReady ? .terminationReady : .draining
            case .cancel:
                currentSnapshot.state = .terminationCancelled
            case .commit:
                currentSnapshot.state = .stopped
            }
        }
    }

    func acceptCommit(_ request: ControlTerminationRequest) -> Bool {
        lock.withLock {
            guard currentSnapshot.terminationRequestID == request.requestID,
                  currentSnapshot.state == .terminationReady
            else { return false }
            recorded.append(request)
            currentSnapshot.state = .stopped
            return true
        }
    }

    func resetForCancellation() {
        lock.withLock {
            currentSnapshot = CompanionTerminationCoordinatorTests.drainingSnapshot(state: .running)
            preparesBecomeReady = false
        }
    }
}
