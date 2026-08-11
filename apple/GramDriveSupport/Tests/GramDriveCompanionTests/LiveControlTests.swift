import Darwin
import Foundation
import GramDriveAgentCore
@testable import GramDriveCompanion
import GramDriveSupport
import Testing

/// The live command path end to end (BUG-260720-3i74u1): the ensurer's
/// probe-start-wait contract, and the live backend + authorization session
/// against a real control/health server pair with scripted engine seams.
struct LiveControlTests {
    // MARK: - Fixtures

    @Test func onlyAnOlderNumericAgentIsEligibleForReplacement() {
        #expect(LiveCompanionBackend.isOlderBuild("136", than: "137"))
        #expect(!LiveCompanionBackend.isOlderBuild("137", than: "137"))
        #expect(!LiveCompanionBackend.isOlderBuild("138", than: "137"))
        #expect(!LiveCompanionBackend.isOlderBuild("legacy", than: "137"))
        #expect(LiveCompanionBackend.buildCompatibility(agent: "137", app: "137") == .matching)
        #expect(LiveCompanionBackend.buildCompatibility(agent: "138", app: "137") == .incompatible)
        #expect(LiveCompanionBackend.buildCompatibility(agent: "legacy", app: "137") == .incompatible)
    }

    @Test func replacementRequiresMatchingBuildAndAnEnumeratedReadyHierarchy() {
        var snapshot = Self.snapshot()
        snapshot.finderContentState = .ready
        snapshot.finderFirstPageItemCount = 0
        #expect(LiveCompanionBackend.isReadyReplacement(snapshot))

        snapshot.finderFirstPageItemCount = nil
        #expect(!LiveCompanionBackend.isReadyReplacement(snapshot))
        snapshot.finderFirstPageItemCount = 1
        snapshot.finderContentState = .preparing
        #expect(!LiveCompanionBackend.isReadyReplacement(snapshot))
        snapshot.finderContentState = .ready
        snapshot.bundleVersion = "not-a-build"
        #expect(!LiveCompanionBackend.isReadyReplacement(snapshot))
        snapshot.bundleVersion = "999999"
        #expect(!LiveCompanionBackend.isReadyReplacement(snapshot))
    }

    @Test func replacementCommitsTheOlderAgentBeforeStartingTheMatchingHierarchy() async throws {
        let layout = try Self.tempLayout()
        let appBuild = "137"
        let transport = ReplacementTransport(oldBuild: "136", replacementBuild: appBuild)
        try transport.startOldAgent(layout: layout)
        defer {
            transport.stop()
            try? FileManager.default.removeItem(at: layout.dataRoot)
        }
        let replacementStarted = LockedBool()
        let starter = ScriptedStarter {
            replacementStarted.set(true)
            try transport.startReplacement(layout: layout)
        }
        let backend = LiveCompanionBackend(
            layout: layout,
            healthTimeout: .milliseconds(50),
            starter: starter,
            startupTimeout: .seconds(1),
            controlRetryInterval: .milliseconds(1),
            appBuild: appBuild,
            matchingAgentReady: { transport.didObserveMatchingHierarchy() }
        )

        let result = await backend.fetchHealth()

        guard case let .running(snapshot) = result else {
            Issue.record("replacement did not report a running matching agent: \(result)")
            return
        }
        #expect(LiveCompanionBackend.isReadyReplacement(snapshot, appBuild: appBuild))
        #expect(transport.terminationActions == [.prepare, .commit])
        #expect(transport.terminationRequestIDs.count == 2)
        #expect(transport.terminationRequestIDs[0] == transport.terminationRequestIDs[1])
        #expect(replacementStarted.value)
        #expect(transport.matchingHierarchyObserved)
    }

    @Test func replacementReconcilesADroppedCommitAcknowledgementBeforeStarting() async throws {
        let layout = try Self.tempLayout()
        let appBuild = "137"
        let transport = ReplacementTransport(
            oldBuild: "136", replacementBuild: appBuild, dropCommitAcknowledgement: true
        )
        try transport.startOldAgent(layout: layout)
        defer {
            transport.stop()
            try? FileManager.default.removeItem(at: layout.dataRoot)
        }
        let replacementStarted = LockedBool()
        let starter = ScriptedStarter {
            replacementStarted.set(true)
            try transport.startReplacement(layout: layout)
        }
        let backend = LiveCompanionBackend(
            layout: layout,
            healthTimeout: .milliseconds(50),
            starter: starter,
            startupTimeout: .seconds(1),
            controlRetryInterval: .milliseconds(1),
            appBuild: appBuild,
            matchingAgentReady: { transport.didObserveMatchingHierarchy() }
        )

        let result = await backend.fetchHealth()

        guard case let .running(snapshot) = result else {
            Issue.record("replacement did not reconcile the dropped commit acknowledgement: \(result)")
            return
        }
        #expect(LiveCompanionBackend.isReadyReplacement(snapshot, appBuild: appBuild))
        #expect(transport.terminationActions == [.prepare, .commit])
        #expect(replacementStarted.value)
        #expect(transport.matchingHierarchyObserved)
    }

    @Test func replacementEscalationRevalidatesIdentityBeforeEverySignal() async throws {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        // Ignore TERM so the production fallback must reach its exact-identity
        // SIGKILL phase. The loop avoids a child process that could outlive this
        // focused process-identity test.
        process.arguments = ["-c", "trap '' TERM; while :; do :; done"]
        try process.run()
        defer {
            if process.isRunning { process.terminate() }
        }

        let identity = try Self.processIdentity(for: process.processIdentifier)
        var reused = identity
        reused.kernelStartMicroseconds += 1
        #expect(await LiveCompanionBackend.terminateExactProcess(reused, pollInterval: .milliseconds(1)))
        #expect(Self.processStillMatches(identity), "a start-time mismatch must never signal a reused PID")

        #expect(await LiveCompanionBackend.terminateExactProcess(identity, pollInterval: .milliseconds(5)))
        #expect(!Self.processStillMatches(identity))
    }

    @Test func realOlderAgentReplacementWaitsForTheCommittedExitContractThenEscalates() async throws {
        let layout = try Self.tempLayout()
        let appBuild = "137"
        let preservedDomainInput = layout.dataRoot.appendingPathComponent("file-provider-domain-id")
        try Data("domain-preserved".utf8).write(to: preservedDomainInput)
        try AgentSettingsStore(fileURL: layout.settingsFile).save(AgentSettings(launchAtLogin: true))

        let agentExecutable = try Self.agentExecutable()
        let oldProcess = try Self.startRealAgent(
            executable: agentExecutable,
            layout: layout,
            reportedBuild: "136",
            extraArguments: [
                "--test-committed-exit-delay-ms", "10000",
                "--test-termination-hard-exit-watchdog-ms", "10000",
            ]
        )
        let successor = ProcessBox()
        defer {
            Self.stopIfNeeded(oldProcess)
            Self.stopIfNeeded(successor.process)
            try? FileManager.default.removeItem(at: layout.dataRoot)
        }

        let oldSnapshot = try Self.waitForRealHealth(socketURL: layout.healthSocket)
        let oldIdentity = try #require(oldSnapshot.processIdentity)
        #expect(oldIdentity.isValidTerminationIdentity)
        #expect(oldSnapshot.bundleVersion == "136")

        var reusedIdentity = oldIdentity
        reusedIdentity.kernelStartMicroseconds += 1
        #expect(
            await LiveCompanionBackend.terminateExactProcess(
                reusedIdentity, pollInterval: .milliseconds(1)
            )
        )
        #expect(
            Self.processStillMatches(oldIdentity),
            "a mismatched kernel start identity must never signal the old agent"
        )

        let replacementStarted = LockedBool()
        let escalationUsed = LockedBool()
        let observationClock = ManualObservationClock()
        let starter = ProcessStarter {
            replacementStarted.set(true)
            let process = try Self.startRealAgent(
                executable: agentExecutable,
                layout: layout,
                reportedBuild: appBuild
            )
            successor.process = process
        }
        let hierarchyReady = LockedBool()
        let backend = LiveCompanionBackend(
            layout: layout,
            healthTimeout: .milliseconds(50),
            starter: starter,
            // This is intentionally much longer than the committed-exit
            // contract. A successor is allowed only after the terminator seam
            // records exact-identity escalation, not after startup patience.
            startupTimeout: .seconds(10),
            controlRetryInterval: .milliseconds(5),
            appBuild: appBuild,
            accountDomainCleanup: nil,
            matchingAgentReady: { hierarchyReady.set(true) },
            committedExitObservationClock: observationClock.asCommittedExitClock(),
            replacementProcessTerminator: { identity, pollInterval in
                escalationUsed.set(true)
                return await LiveCompanionBackend.terminateExactProcess(
                    identity, pollInterval: pollInterval
                )
            }
        )

        let result = await backend.fetchHealth()

        guard case let .running(snapshot) = result else {
            Issue.record("real replacement did not report a matching running agent: \(result)")
            return
        }
        let successorIdentity = try #require(snapshot.processIdentity)
        #expect(!Self.processStillMatches(oldIdentity))
        #expect(successorIdentity.isValidTerminationIdentity)
        #expect(successorIdentity != oldIdentity)
        #expect(snapshot.bundleVersion == appBuild)
        #expect(LiveCompanionBackend.isReadyReplacement(snapshot, appBuild: appBuild))
        #expect(snapshot.launchAtLogin == true)
        #expect(try Data(contentsOf: preservedDomainInput) == Data("domain-preserved".utf8))
        #expect(replacementStarted.value)
        #expect(starter.startCount == 1)
        #expect(hierarchyReady.value)
        #expect(escalationUsed.value)
        // The old agent delays both ordinary exit and its own watchdog for ten
        // seconds. This fake observation clock advances only when the backend
        // observes the still-matching old identity, so it deterministically
        // proves that escalation consumed exactly the shared two-second
        // committed-exit budget, not the ten-second new-agent startup budget.
        #expect(observationClock.elapsed == CommitExitWatchdog.committedExitDeadline)
    }

    @Test func currentBuildRollbackRecoveryAcceptsAnAlreadyServingMatchingAgent() async throws {
        let layout = try Self.tempLayout()
        var readySnapshot = Self.snapshot()
        readySnapshot.bundleVersion = "137"
        readySnapshot.finderContentState = .ready
        readySnapshot.finderFirstPageItemCount = 0
        let snapshot = readySnapshot
        let health = try AgentHealthServer.start(socketURL: layout.healthSocket) { snapshot }
        defer {
            health.stop()
            try? FileManager.default.removeItem(at: layout.dataRoot)
        }
        let starter = ScriptedStarter()
        let backend = LiveCompanionBackend(
            layout: layout,
            healthTimeout: .milliseconds(50),
            starter: starter,
            startupTimeout: .seconds(1),
            controlRetryInterval: .milliseconds(1),
            appBuild: "137"
        )

        #expect(await backend.recoverCurrentBuildForTerminationRollback())
        #expect(starter.askedPreferences.isEmpty)
    }

    private static func tempLayout() throws -> AgentRuntimeLayout {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-livectl-\(UUID().uuidString.prefix(8))")
        let layout = AgentRuntimeLayout(dataRoot: url)
        try layout.ensureDirectories()
        return layout
    }

    private static func processIdentity(for pid: Int32) throws -> AgentProcessIdentity {
        var info = proc_bsdinfo()
        let count = proc_pidinfo(
            pid, PROC_PIDTBSDINFO, 0, &info, Int32(MemoryLayout<proc_bsdinfo>.size)
        )
        guard count == MemoryLayout<proc_bsdinfo>.size else {
            throw ProcessIdentityError.unavailable
        }
        return AgentProcessIdentity(
            instanceID: UUID(), pid: pid,
            kernelStartSeconds: Int64(info.pbi_start_tvsec),
            kernelStartMicroseconds: Int64(info.pbi_start_tvusec)
        )
    }

    private static func processStillMatches(_ identity: AgentProcessIdentity) -> Bool {
        var info = proc_bsdinfo()
        let count = proc_pidinfo(
            identity.pid, PROC_PIDTBSDINFO, 0, &info, Int32(MemoryLayout<proc_bsdinfo>.size)
        )
        return count == MemoryLayout<proc_bsdinfo>.size
            && Int64(info.pbi_start_tvsec) == identity.kernelStartSeconds
            && Int64(info.pbi_start_tvusec) == identity.kernelStartMicroseconds
    }

    private static func agentExecutable() throws -> URL {
        let source = URL(fileURLWithPath: #filePath)
        let packageRoot = source
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let buildRoot = packageRoot.appendingPathComponent(".build", isDirectory: true)
        let candidates = try FileManager.default.contentsOfDirectory(
            at: buildRoot, includingPropertiesForKeys: nil
        )
        .map { $0.appendingPathComponent("debug/gramdrive-agent") }
        guard let executable = candidates.first(where: {
            FileManager.default.isExecutableFile(atPath: $0.path)
        }) else {
            throw ProcessIdentityError.agentExecutableMissing
        }
        return executable
    }

    private static func startRealAgent(
        executable: URL,
        layout: AgentRuntimeLayout,
        reportedBuild: String,
        extraArguments: [String] = []
    ) throws -> Process {
        let process = Process()
        process.executableURL = executable
        process.arguments = [
            "run",
            "--data-root", layout.dataRoot.path,
            "--drain-grace-ms", "25",
            "--drain-cancel-wait-ms", "25",
            "--test-reported-bundle-version", reportedBuild,
            "--test-finder-hierarchy-ready", "true",
        ] + extraArguments
        process.standardOutput = Pipe()
        process.standardError = Pipe()
        try process.run()
        return process
    }

    private static func waitForRealHealth(socketURL: URL) throws -> AgentHealthSnapshot {
        let deadline = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < deadline {
            if let snapshot = try? AgentHealthClient.fetch(socketURL: socketURL, timeout: .milliseconds(100)) {
                return snapshot
            }
            Thread.sleep(forTimeInterval: 0.02)
        }
        throw ProcessIdentityError.agentHealthUnavailable
    }

    private static func stopIfNeeded(_ process: Process?) {
        guard let process, process.isRunning else { return }
        _ = Darwin.kill(process.processIdentifier, SIGKILL)
    }

    private enum ProcessIdentityError: Error {
        case unavailable
        case agentExecutableMissing
        case agentHealthUnavailable
    }

    private static func snapshot(accounts: [AccountHealthSummary]? = nil) -> AgentHealthSnapshot {
        AgentHealthSnapshot(
            payloadVersion: 1,
            agentVersion: AgentVersion.current,
            bundleVersion: AgentBuildVersion.current,
            contractVersion: "0.6.0",
            pid: 7,
            processIdentity: AgentProcessIdentity(
                instanceID: UUID(), pid: Int32.max, kernelStartSeconds: 1, kernelStartMicroseconds: 1
            ),
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
            accounts: accounts
        )
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
                    repairer: repairer
                )
            )
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
            loginItemPreferred: { true }
        )
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
            startupTimeout: .seconds(5)
        )
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
            startupTimeout: .milliseconds(100)
        )
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
            startupTimeout: .milliseconds(80)
        )
        guard case .failed = await ensurer.ensureRunning() else {
            Issue.record("an agent that never answers must fail the ensure")
            return
        }
    }

    @Test func bundledAgentPathMatchesThePackagedContentsMacOSLayout() {
        let appExecutable = URL(
            fileURLWithPath: "/Applications/GramDrive.app/Contents/MacOS/GramDrive"
        )
        #expect(
            BundledAgentStarter.bundledAgentExecutable(relativeTo: appExecutable)?.path
                == "/Applications/GramDrive.app/Contents/MacOS/gramdrive-agent"
        )
        #expect(BundledAgentStarter.bundledAgentExecutable(relativeTo: nil) == nil)
    }

    @Test func bundledStarterOwnsOneDirectSessionProcessAcrossRepeatedStarts() throws {
        let starter = BundledAgentStarter(
            loginItem: PassiveLoginItemService(),
            agentExecutable: URL(fileURLWithPath: "/usr/bin/yes")
        )
        defer { starter.stopOwnedAgent() }

        try starter.startAgent(loginItemPreferred: false)
        let firstPID = try #require(starter.ownedProcessIdentifier)
        try starter.startAgent(loginItemPreferred: false)

        #expect(starter.ownedProcessIdentifier == firstPID)
        starter.stopOwnedAgent()
        let deadline = ContinuousClock.now + .seconds(5)
        while starter.ownedProcessIdentifier != nil, ContinuousClock.now < deadline {
            Thread.sleep(forTimeInterval: 0.01)
        }
        #expect(starter.ownedProcessIdentifier == nil)

        try starter.startAgent(loginItemPreferred: false)
        let relaunchedPID = try #require(starter.ownedProcessIdentifier)
        #expect(relaunchedPID != firstPID)
    }

    @Test func enabledLoginItemAlsoStartsTheMissingCurrentSessionAgent() throws {
        let loginItem = PassiveLoginItemService()
        loginItem.status = .enabled
        let starter = BundledAgentStarter(
            loginItem: loginItem,
            agentExecutable: URL(fileURLWithPath: "/usr/bin/yes")
        )
        defer { starter.stopOwnedAgent() }

        try starter.startAgent(loginItemPreferred: true)

        #expect(loginItem.status == .enabled)
        #expect(starter.ownedProcessIdentifier != nil)
    }

    // MARK: - The live authorization session

    @Test func liveSessionMapsWireStatesAndResults() async throws {
        let layout = try Self.tempLayout()
        let hosted = ScriptedCompanionHostedSession()
        let agent = try FakeAgent(
            layout: layout, authorizer: ScriptedCompanionAuthorizer(session: hosted)
        )
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
            rejection: ControlAuthRejection(kind: "rate-limited", retryAfterSeconds: 17)
        )
        let rejected = await session.submit(.submitCode("00000"))
        #expect(rejected == .rejected(.rateLimited(retryAfterSeconds: 17)))

        // The code step's rendering material crosses whole.
        hosted.emit(
            ControlAuthState(
                kind: "wait-code",
                codeInfo: ControlAuthCodeInfo(
                    phoneNumber: "+9996612222", codeLength: 5, resendTimeoutSeconds: 60
                )
            )
        )
        #expect(
            await states.next()
                == .waitCode(
                    CompanionCodeInfo(
                        phoneNumber: "+9996612222", codeLength: 5, resendTimeoutSeconds: 60
                    )
                )
        )

        // Finalizing renders as machinery; ready carries through; a foreign
        // state fails safe.
        hosted.emit(ControlAuthState(kind: "finalizing"))
        #expect(await states.next() == .configuring)
        hosted.emit(
            ControlAuthState(
                kind: "ready",
                account: ControlAccountIdentity(accountId: 777, displayName: "Test User")
            )
        )
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

    @Test func liveSessionTimesOutWaitingForItsFirstAuthEventAndClosesTheChannel() async throws {
        let layout = try Self.tempLayout()
        let hosted = ScriptedCompanionHostedSession()
        let agent = try FakeAgent(
            layout: layout, authorizer: ScriptedCompanionAuthorizer(session: hosted)
        )
        defer { agent.stop() }

        let session = LiveAuthorizationSession(
            openChannel: { .opened(try! ControlAuthChannel.open(socketURL: layout.controlSocket)) },
            firstEventTimeout: .milliseconds(20)
        )
        let states = StateCollector(session.states)

        #expect(await session.start() == .unavailable(.timedOut))
        #expect(await states.next(within: .seconds(1)) == nil)
        #expect(hosted.isClosed)
    }

    @Test func liveSessionClassifiesFirstEventEOFAsDropped() async throws {
        let layout = try Self.tempLayout()
        let hosted = ScriptedCompanionHostedSession()
        hosted.close()
        let agent = try FakeAgent(
            layout: layout, authorizer: ScriptedCompanionAuthorizer(session: hosted)
        )
        defer { agent.stop() }

        let session = LiveAuthorizationSession(
            openChannel: { .opened(try! ControlAuthChannel.open(socketURL: layout.controlSocket)) },
            firstEventTimeout: .seconds(1)
        )

        #expect(await session.start() == .unavailable(.dropped))
    }

    @Test func liveSessionCancellationDeadlineClosesTheChannelAndClearsModelSubmission() async throws {
        let layout = try Self.tempLayout()
        let hosted = StalledCompanionHostedSession()
        hosted.emit(ControlAuthState(kind: "wait-phone-number"))
        let agent = try FakeAgent(
            layout: layout, authorizer: ScriptedCompanionAuthorizer(session: hosted)
        )
        defer { agent.stop() }

        let session = LiveAuthorizationSession(
            openChannel: { .opened(try! ControlAuthChannel.open(socketURL: layout.controlSocket)) },
            completionTimeout: .milliseconds(20)
        )
        let backend = InMemoryCompanionBackend(session: { session })
        let model = await MainActor.run {
            AuthorizationViewModel(backend: backend, teardownTimeout: .seconds(1))
        }

        await model.begin()
        for _ in 0..<100 where await MainActor.run(body: { model.state != .waitPhoneNumber }) {
            await Task.yield()
        }
        #expect(await MainActor.run { model.state == .waitPhoneNumber })

        await model.cancel()
        #expect(await MainActor.run { model.unavailable == .timedOut })
        #expect(await MainActor.run { !model.isSubmitting })
        let deadline = ContinuousClock.now + .seconds(1)
        while !hosted.isClosed, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(10))
        }
        #expect(hosted.isClosed)
    }

    @Test func liveSessionSubmissionTimeoutClosesAStalledAgentChannel() async throws {
        let layout = try Self.tempLayout()
        let hosted = StalledCompanionHostedSession()
        hosted.emit(ControlAuthState(kind: "wait-phone-number"))
        let agent = try FakeAgent(
            layout: layout, authorizer: ScriptedCompanionAuthorizer(session: hosted)
        )
        defer { agent.stop() }

        let session = LiveAuthorizationSession(
            openChannel: { .opened(try! ControlAuthChannel.open(socketURL: layout.controlSocket)) },
            submitTimeout: .milliseconds(20)
        )
        let states = StateCollector(session.states)
        #expect(await session.start() == .started)
        #expect(await states.next() == .waitPhoneNumber)

        #expect(await session.submit(.requestQrCode) == .unavailable(.timedOut))
        #expect(await states.next(within: .seconds(1)) == nil)
        let deadline = ContinuousClock.now + .seconds(1)
        while !hosted.isClosed, ContinuousClock.now < deadline {
            try? await Task.sleep(for: .milliseconds(10))
        }
        #expect(hosted.isClosed)
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
            startupTimeout: .seconds(5)
        )

        #expect(await backend.requestRepair() == .completed)
        #expect(repairer.runCount == 1)
        #expect(starter.askedPreferences == [false], "no settings file: login item defaults off")
    }

    @Test func concurrentColdStartStatusReadsStartExactlyOneAgent() async throws {
        let layout = try Self.tempLayout()
        let agentBox = AgentBox()
        let starter = ScriptedStarter(onStart: {
            DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + 0.05) {
                agentBox.agent = try? FakeAgent(layout: layout)
            }
        })
        defer {
            agentBox.agent?.stop()
            try? FileManager.default.removeItem(at: layout.dataRoot)
        }
        let backend = LiveCompanionBackend(
            layout: layout,
            healthTimeout: .seconds(2),
            starter: starter,
            startupTimeout: .seconds(5)
        )

        async let launchRead = backend.fetchHealth()
        async let repeatedActivationRead = backend.fetchHealth()
        let readings = await [launchRead, repeatedActivationRead]

        #expect(readings.allSatisfy { if case .running = $0 { true } else { false } })
        #expect(starter.askedPreferences == [false], "cold-start activation must be coalesced")
    }

    /// Regression for the notarized v0.1.1 clean-first-launch failure: health
    /// could answer before `control.sock` existed, so the first auth connect
    /// failed even though the just-spawned agent stayed healthy. The backend
    /// must wait boundedly for the late control listener, without replaying a
    /// request after it has reached a listener.
    @Test func backendRetriesAuthConnectWhileControlSocketBecomesReady() async throws {
        let layout = try Self.tempLayout()
        let health = try AgentHealthServer.start(socketURL: layout.healthSocket) {
            Self.snapshot()
        }
        let hosted = ScriptedCompanionHostedSession()
        hosted.emit(ControlAuthState(kind: "starting"))
        let control = ControlServerBox()
        defer {
            control.server?.stop()
            health.stop()
            try? FileManager.default.removeItem(at: layout.dataRoot)
        }

        DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + 0.15) {
            control.server = try? ControlServer.start(
                socketURL: layout.controlSocket,
                handlers: ControlServerHandlers(
                    status: { Self.snapshot() },
                    reloadSettings: { AgentSettings() },
                    authorizer: ScriptedCompanionAuthorizer(session: hosted)
                )
            )
        }

        let backend = LiveCompanionBackend(
            layout: layout,
            healthTimeout: .seconds(1),
            starter: ScriptedStarter(),
            startupTimeout: .seconds(2),
            controlRetryInterval: .milliseconds(10)
        )
        let session = backend.makeAuthorizationSession()
        let states = StateCollector(session.states)

        #expect(await session.start() == .started)
        #expect(await states.next() == .starting)
    }

    @Test func backendReportsAgentNotRunningWhenStartFails() async throws {
        let layout = try Self.tempLayout()
        struct Boom: Error {}
        let backend = LiveCompanionBackend(
            layout: layout,
            healthTimeout: .seconds(1),
            starter: ScriptedStarter(onStart: { throw Boom() }),
            startupTimeout: .milliseconds(100)
        )

        #expect(await backend.requestRepair() == .unavailable(.agentNotRunning))
        #expect(
            await backend.fetchContentPolicy(accountId: 777)
                == .unavailable(.agentNotRunning)
        )
        let removal = await backend.removeAccount(
            RemovalConfirmation(
                accountLabel: "A", typedConfirmation: "A", acknowledgedIrreversible: true
            )
        )
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
                    accountId: 777_000_123, displayName: "Test User", authState: "authorized"
                ),
            ],
            remover: remover
        )
        defer { agent.stop() }
        let backend = LiveCompanionBackend(
            layout: layout,
            healthTimeout: .seconds(2),
            starter: ScriptedStarter(),
            startupTimeout: .seconds(5),
            accountDomainCleanup: { cleanup.record($0) }
        )

        let outcome = await backend.removeAccount(
            RemovalConfirmation(
                accountLabel: "This account",
                typedConfirmation: "this account",
                acknowledgedIrreversible: true
            )
        )
        #expect(outcome == .completed)
        #expect(
            remover.requests
                == [ControlRemovalRequest(accountId: 777_000_123, revokeSession: true)]
        )
        #expect(cleanup.accountIds == [777_000_123], "the domain half runs after the engine half")
    }

    @Test func backendRemovalWithNoAccountsIsNotFound() async throws {
        let layout = try Self.tempLayout()
        let agent = try FakeAgent(layout: layout, accounts: [])
        defer { agent.stop() }
        let backend = LiveCompanionBackend(
            layout: layout, healthTimeout: .seconds(2), starter: ScriptedStarter(),
            startupTimeout: .seconds(5)
        )

        let outcome = await backend.removeAccount(
            RemovalConfirmation(
                accountLabel: "A", typedConfirmation: "A", acknowledgedIrreversible: true
            )
        )
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

private final class PassiveLoginItemService: LoginItemService {
    var status: LoginItemStatus = .notRegistered
    func register() throws {
        status = .enabled
    }

    func unregister() throws {
        status = .notRegistered
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

private final class ControlServerBox: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: ControlServer?
    var server: ControlServer? {
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
    let session: any AgentAuthSessionHosting
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
    private var closed = false

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
        lock.withLock { closed }
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
        lock.withLock { closed = true }
        continuation.finish()
    }

    private func record(_ input: ControlAuthInput) -> AgentAuthSubmitAnswer {
        lock.lock()
        defer { lock.unlock() }
        inputs.append(input)
        return answer
    }
}

private final class StalledCompanionHostedSession: AgentAuthSessionHosting, @unchecked Sendable {
    private let lock = NSLock()
    private let stream: AsyncStream<ControlAuthState>
    private let continuation: AsyncStream<ControlAuthState>.Continuation
    private var closed = false
    private var stalledSubmit: CheckedContinuation<AgentAuthSubmitAnswer, Never>?

    init() {
        (stream, continuation) = AsyncStream.makeStream(of: ControlAuthState.self)
    }

    var states: AsyncStream<ControlAuthState> { stream }

    var isClosed: Bool {
        lock.withLock { closed }
    }

    func emit(_ state: ControlAuthState) {
        continuation.yield(state)
    }

    func submit(_: ControlAuthInput) async -> AgentAuthSubmitAnswer {
        if lock.withLock({ closed }) { return .accepted }
        return await withCheckedContinuation { continuation in
            let wasClosed = lock.withLock { () -> Bool in
                if closed { return true }
                stalledSubmit = continuation
                return false
            }
            if wasClosed { continuation.resume(returning: .accepted) }
        }
    }

    func close() {
        lock.lock()
        closed = true
        let submit = stalledSubmit
        stalledSubmit = nil
        lock.unlock()
        continuation.finish()
        submit?.resume(returning: .accepted)
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

private final class LockedBool: @unchecked Sendable {
    private let lock = NSLock()
    private var stored = false

    var value: Bool {
        lock.withLock { stored }
    }

    func set(_ value: Bool) {
        lock.withLock { stored = value }
    }
}

private final class ManualObservationClock: @unchecked Sendable {
    private let lock = NSLock()
    private var current = ContinuousClock.now
    private var totalElapsed: Duration = .zero

    func asCommittedExitClock() -> LiveCompanionBackend.CommittedExitObservationClock {
        LiveCompanionBackend.CommittedExitObservationClock(
            now: { self.now },
            sleep: { duration in self.advance(by: duration) }
        )
    }

    var elapsed: Duration {
        lock.lock()
        defer { lock.unlock() }
        return totalElapsed
    }

    private var now: ContinuousClock.Instant {
        lock.lock()
        defer { lock.unlock() }
        return current
    }

    private func advance(by duration: Duration) {
        lock.lock()
        current = current.advanced(by: duration)
        totalElapsed += duration
        lock.unlock()
    }
}

private final class ProcessBox: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: Process?

    var process: Process? {
        get { lock.withLock { stored } }
        set { lock.withLock { stored = newValue } }
    }
}

private final class ProcessStarter: AgentStarting, @unchecked Sendable {
    private let lock = NSLock()
    private var starts = 0
    private let onStart: @Sendable () throws -> Void

    init(onStart: @escaping @Sendable () throws -> Void) {
        self.onStart = onStart
    }

    var startCount: Int {
        lock.withLock { starts }
    }

    func startAgent(loginItemPreferred _: Bool) throws {
        lock.withLock { starts += 1 }
        try onStart()
    }
}

/// Real socket-hosted old/new agent stand-in for the update handoff. The
/// control callback models the production lifecycle boundary: prepare reaches
/// ready, commit is accepted only for that UUID, then the old endpoints are
/// removed before the bundled replacement starts.
private final class ReplacementTransport: @unchecked Sendable {
    private let lock = NSLock()
    private let oldBuild: String
    private let replacementBuild: String
    private var current = AgentHealthSnapshot(
        payloadVersion: 3,
        agentVersion: "0.1.0",
        bundleVersion: nil,
        contractVersion: "1.0.0",
        pid: 1,
        processIdentity: AgentProcessIdentity(
            instanceID: UUID(), pid: Int32.max, kernelStartSeconds: 1, kernelStartMicroseconds: 1
        ),
        state: .running,
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
        recentEvents: []
    )
    private var requests: [ControlTerminationRequest] = []
    private var oldHealth: AgentHealthServer?
    private var oldControl: ControlServer?
    private var replacementHealth: AgentHealthServer?
    private var hierarchyObserved = false

    private let dropCommitAcknowledgement: Bool

    init(oldBuild: String, replacementBuild: String, dropCommitAcknowledgement: Bool = false) {
        self.oldBuild = oldBuild
        self.replacementBuild = replacementBuild
        self.dropCommitAcknowledgement = dropCommitAcknowledgement
        current.bundleVersion = oldBuild
    }

    var snapshot: AgentHealthSnapshot {
        lock.withLock { current }
    }

    var terminationActions: [ControlTerminationRequest.Action] {
        lock.withLock { requests.map(\.action) }
    }

    var terminationRequestIDs: [UUID] {
        lock.withLock { requests.map(\.requestID) }
    }

    var matchingHierarchyObserved: Bool {
        lock.withLock { hierarchyObserved }
    }

    func startOldAgent(layout: AgentRuntimeLayout) throws {
        let health = try AgentHealthServer.start(socketURL: layout.healthSocket) { self.snapshot }
        let control = try ControlServer.start(
            socketURL: layout.controlSocket,
            handlers: ControlServerHandlers(
                status: { self.snapshot },
                reloadSettings: { AgentSettings() },
                prepareForTermination: { self.prepare($0) },
                acceptTerminationCommit: { self.accept($0) },
                finishAcceptedTerminationCommit: { [weak self] _ in self?.stopOldAgent() }
            )
        )
        lock.withLock {
            oldHealth = health
            oldControl = control
        }
    }

    func startReplacement(layout: AgentRuntimeLayout) throws {
        lock.withLock {
            current = Self.snapshot(build: replacementBuild, state: .running, ready: true)
        }
        let health = try AgentHealthServer.start(socketURL: layout.healthSocket) { self.snapshot }
        lock.withLock { replacementHealth = health }
    }

    func didObserveMatchingHierarchy() {
        lock.withLock { hierarchyObserved = true }
    }

    func stop() {
        let servers = lock.withLock { () -> (AgentHealthServer?, ControlServer?, AgentHealthServer?) in
            defer {
                oldHealth = nil
                oldControl = nil
                replacementHealth = nil
            }
            return (oldHealth, oldControl, replacementHealth)
        }
        servers.0?.stop()
        servers.1?.stop()
        servers.2?.stop()
    }

    private func prepare(_ request: ControlTerminationRequest) {
        lock.withLock {
            requests.append(request)
            current.terminationRequestID = request.requestID
            current.state = request.action == .cancel ? .terminationCancelled : .terminationReady
        }
    }

    private func accept(_ request: ControlTerminationRequest) -> Bool {
        let accepted = lock.withLock {
            guard current.terminationRequestID == request.requestID, current.state == .terminationReady else {
                return false
            }
            requests.append(request)
            current.state = .stopped
            return true
        }
        if accepted, dropCommitAcknowledgement {
            // Close the active connection after the lifecycle claimed the matching
            // commit but before ControlServer can send its event. This is the real
            // response-loss boundary the backend must reconcile by observing the
            // old health endpoint rather than starting a replacement immediately.
            lock.withLock { oldControl }?.stop()
        }
        return accepted
    }

    private func stopOldAgent() {
        let server = lock.withLock { () -> AgentHealthServer? in
            let server = oldHealth
            oldHealth = nil
            return server
        }
        // This callback runs on the old control server's active connection;
        // leave that server to the outer test cleanup after it has finished its
        // own response path. The production lifecycle performs the same ordering.
        server?.stop()
    }

    private static func snapshot(
        build: String,
        state: AgentRunState,
        ready: Bool
    ) -> AgentHealthSnapshot {
        AgentHealthSnapshot(
            payloadVersion: 3,
            agentVersion: "0.1.0",
            bundleVersion: build,
            contractVersion: "1.0.0",
            pid: 1,
            processIdentity: AgentProcessIdentity(
                instanceID: UUID(), pid: Int32.max, kernelStartSeconds: 1, kernelStartMicroseconds: 1
            ),
            state: state,
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
            finderContentState: ready ? .ready : nil,
            finderFirstPageItemCount: ready ? 0 : nil
        )
    }
}
