import Darwin
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
    /// The clock used only while observing an irrevocably committed older
    /// process. Production uses the continuous clock; the internal seam lets
    /// the real-process regression prove this lifecycle budget without
    /// depending on host scheduler latency.
    struct CommittedExitObservationClock: Sendable {
        let now: @Sendable () -> ContinuousClock.Instant
        let sleep: @Sendable (Duration) async -> Void

        static let live = Self(
            now: { ContinuousClock.now },
            sleep: { duration in try? await Task.sleep(for: duration) }
        )
    }

    private let layout: AgentRuntimeLayout
    private let healthTimeout: Duration
    private let ensurer: AgentEnsurer
    private let controlConnectTimeout: Duration
    private let controlRetryInterval: Duration
    /// `startupTimeout` bounds readiness of a new process. It must not also
    /// delay recovery from an already-accepted commit: the old agent has
    /// armed this shorter, shared hard-exit contract.
    private let committedExitObservationTimeout: Duration
    private let committedExitObservationClock: CommittedExitObservationClock
    private let replacementProcessTerminator: @Sendable (AgentProcessIdentity, Duration) async -> Bool
    private let appBuild: String
    /// Fired only after a replacement reports the exact packaged build and a
    /// ready, enumerated hierarchy. The containing app uses it to wake existing
    /// File Provider enumerators without recreating their durable domain.
    private let matchingAgentReady: (@Sendable () async -> Void)?
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
        controlRetryInterval: Duration = .milliseconds(100),
        appBuild: String = AgentBuildVersion.current,
        accountDomainCleanup: (@Sendable (Int64) async -> Void)? = nil,
        matchingAgentReady: (@Sendable () async -> Void)? = nil
    ) {
        self.init(
            layout: layout,
            healthTimeout: healthTimeout,
            starter: starter,
            startupTimeout: startupTimeout,
            controlRetryInterval: controlRetryInterval,
            appBuild: appBuild,
            accountDomainCleanup: accountDomainCleanup,
            matchingAgentReady: matchingAgentReady,
            committedExitObservationClock: .live,
            replacementProcessTerminator: { identity, pollInterval in
                await Self.terminateExactProcess(identity, pollInterval: pollInterval)
            }
        )
    }

    init(
        layout: AgentRuntimeLayout,
        healthTimeout: Duration,
        starter: (any AgentStarting)?,
        startupTimeout: Duration,
        controlRetryInterval: Duration,
        appBuild: String,
        accountDomainCleanup: (@Sendable (Int64) async -> Void)?,
        matchingAgentReady: (@Sendable () async -> Void)?,
        committedExitObservationClock: CommittedExitObservationClock,
        replacementProcessTerminator: @escaping @Sendable (AgentProcessIdentity, Duration) async -> Bool
    ) {
        self.layout = layout
        self.healthTimeout = healthTimeout
        controlConnectTimeout = startupTimeout
        self.controlRetryInterval = controlRetryInterval
        committedExitObservationTimeout = CommitExitWatchdog.committedExitDeadline
        self.committedExitObservationClock = committedExitObservationClock
        self.replacementProcessTerminator = replacementProcessTerminator
        self.appBuild = appBuild
        self.accountDomainCleanup = accountDomainCleanup
        self.matchingAgentReady = matchingAgentReady
        let settingsFile = layout.settingsFile
        let probeTimeout: Duration = .seconds(1)
        let socketURL = layout.healthSocket
        ensurer = AgentEnsurer(
            probe: {
                await Self.health(socketURL: socketURL, timeout: probeTimeout)
            },
            starter: starter ?? BundledAgentStarter(),
            loginItemPreferred: {
                (try? AgentSettingsStore(fileURL: settingsFile).load().launchAtLogin) ?? false
            },
            startupTimeout: startupTimeout
        )
    }

    /// Builds a backend over the App Group container's data root. Throws if
    /// the container cannot be resolved (missing entitlement, sandbox).
    public init(
        healthTimeout: Duration = .seconds(5),
        accountDomainCleanup: (@Sendable (Int64) async -> Void)? = nil,
        matchingAgentReady: (@Sendable () async -> Void)? = nil
    ) throws {
        let dataRoot = try AppGroup.dataRootURL(containerURL: AppGroup.containerURL())
        self.init(
            layout: AgentRuntimeLayout(dataRoot: dataRoot),
            healthTimeout: healthTimeout,
            accountDomainCleanup: accountDomainCleanup,
            matchingAgentReady: matchingAgentReady
        )
    }

    // MARK: - Reads

    public func fetchHealth() async -> HealthReadout {
        // Status is also a launch surface. This makes app/menu/Welcome reads
        // join the same coalesced readiness barrier as control operations,
        // instead of exposing a transient daemon-unavailable state on a cold
        // direct-session launch.
        _ = await ensurer.ensureRunning()
        let initial = await Self.health(socketURL: layout.healthSocket, timeout: healthTimeout)
        guard case let .running(snapshot) = initial else { return initial }
        guard let agentBuild = snapshot.bundleVersion else {
            return .error("agent did not report a packaged CFBundleVersion")
        }
        switch Self.buildCompatibility(agent: agentBuild, app: appBuild) {
        case .matching:
            return initial
        case .incompatible:
            return .error("agent build does not match the packaged app build")
        case .older:
            break
        }
        guard let oldIdentity = snapshot.processIdentity,
              oldIdentity.isValidTerminationIdentity
        else {
            return .error("agent did not report a valid process identity for replacement")
        }

        // Sparkle replaced the outer bundle but a preceding agent survived its
        // process gap. Stop only the older helper, wait for the old sockets to
        // disappear, then start through the existing launch mechanism so the new
        // bundled executable reports the app's exact CFBundleVersion.
        let replacementRequest = ControlTerminationRequest(
            expectedAgentInstanceID: oldIdentity.instanceID,
            reason: .update,
            targetBuild: appBuild
        )
        switch await prepareForTermination(replacementRequest) {
        case .completed, .unavailable(.dropped):
            // The control server only begins preparation after its write attempt.
            // A dropped response must therefore be reconciled through the UUID in
            // health, exactly like a dropped commit response below.
            break
        case .unavailable(.agentNotRunning):
            guard case .failed = await ensurer.ensureRunning() else {
                return await waitForMatchingAgentHierarchy()
            }
            return .notRunning
        case .unavailable, .failed:
            return initial
        }
        guard await waitForPreparedTermination(request: replacementRequest) else {
            return .timedOut
        }
        var commit = replacementRequest
        commit.action = .commit
        switch await prepareForTermination(commit) {
        case .completed, .unavailable(.dropped):
            // A response may be lost after the old agent accepted the exact commit.
            // Treat that as an observation problem: only the old endpoint's
            // disappearance authorizes launching the bundled replacement.
            break
        case .unavailable(.agentNotRunning):
            break
        case .unavailable, .failed:
            return .error("older agent did not accept the replacement termination commit")
        }
        guard await waitForAgentToDisappear(identity: oldIdentity) else { return .timedOut }
        guard case .failed = await ensurer.ensureRunning() else {
            return await waitForMatchingAgentHierarchy()
        }
        return .notRunning
    }

    /// Reads the health socket without starting or replacing an agent. The
    /// application termination gate uses this while waiting for an accepted
    /// drain to make the endpoint disappear.
    public func fetchAgentHealthWithoutRelaunch() async -> HealthReadout {
        let readout = await Self.health(socketURL: layout.healthSocket, timeout: healthTimeout)
        guard case let .running(snapshot) = readout,
              snapshot.state == .terminationCancelled
        else { return readout }
        guard await verifyTerminationRollbackServing(snapshot) else {
            return .error("agent rollback has not restored every serving endpoint")
        }
        return readout
    }

    /// The catastrophic pre-commit recovery branch: after the coordinator has
    /// observed the exact old identity die, start only the agent embedded in
    /// this still-current app and require its existing hierarchy readiness
    /// contract. This preserves the App Group, authorization, and File
    /// Provider domain; it never recreates any durable identity.
    public func recoverCurrentBuildForTerminationRollback() async -> Bool {
        switch await ensurer.ensureRunning() {
        case .started, .alreadyRunning:
            break
        case .failed:
            return false
        }
        guard case let .running(snapshot) = await waitForMatchingAgentHierarchy() else { return false }
        return Self.isReadyReplacement(snapshot, appBuild: appBuild)
    }

    private func waitForAgentToDisappear(identity: AgentProcessIdentity) async -> Bool {
        // The accepted commit has armed `CommitExitWatchdog` for this exact
        // bound. Do not consume the unrelated new-agent startup budget here:
        // a wedged committed predecessor otherwise blocks replacement even
        // though the lifecycle has become irreversible.
        let deadline = committedExitObservationClock.now() + committedExitObservationTimeout
        while committedExitObservationClock.now() < deadline {
            if identity.observe().provesCapturedProcessExited {
                return true
            }
            await committedExitObservationClock.sleep(controlRetryInterval)
        }
        return await replacementProcessTerminator(identity, controlRetryInterval)
    }

    /// A `commit` is irrevocable, so a wedged old helper is not a normal
    /// replacement timeout. Revalidate the captured kernel identity before
    /// every signal; a reused PID proves the old process died and is never
    /// signalled as its predecessor.
    static func terminateExactProcess(
        _ identity: AgentProcessIdentity,
        pollInterval: Duration = .milliseconds(100)
    ) async -> Bool {
        switch identity.observe() {
        case .absent, .replaced:
            return true
        case .indeterminate:
            return false
        case .matching:
            break
        }
        _ = Darwin.kill(identity.pid, SIGTERM)
        let termDeadline = ContinuousClock.now + .milliseconds(500)
        while ContinuousClock.now < termDeadline {
            switch identity.observe() {
            case .absent, .replaced: return true
            case .indeterminate: return false
            case .matching: break
            }
            try? await Task.sleep(for: pollInterval)
        }
        switch identity.observe() {
        case .absent, .replaced: return true
        case .indeterminate: return false
        case .matching: break
        }
        _ = Darwin.kill(identity.pid, SIGKILL)
        let killDeadline = ContinuousClock.now + .seconds(2)
        while ContinuousClock.now < killDeadline {
            switch identity.observe() {
            case .absent, .replaced: return true
            case .indeterminate: return false
            case .matching: break
            }
            try? await Task.sleep(for: pollInterval)
        }
        return identity.observe().provesCapturedProcessExited
    }

    /// A request-correlated cancellation is a terminal `false` proof only
    /// when it restores the same process' transfer admission and namespace
    /// owners *and* each serving IPC endpoint can answer again.
    private func verifyTerminationRollbackServing(_ snapshot: AgentHealthSnapshot) async -> Bool {
        guard snapshot.state == .terminationCancelled,
              snapshot.transferAdmissionOpen == true,
              snapshot.namespaceOwnersRestored == true,
              snapshot.servingGeneration != nil,
              snapshot.finderContentState == .ready
        else { return false }
        let control = await Self.sendControlCommand(
            ControlRequest(operation: .status),
            socketURL: layout.controlSocket,
            commandTimeout: healthTimeout,
            connectTimeout: healthTimeout,
            retryInterval: controlRetryInterval
        )
        guard case let .success(.status(controlSnapshot)) = control,
              controlSnapshot.processIdentity == snapshot.processIdentity,
              controlSnapshot.terminationRequestID == snapshot.terminationRequestID,
              controlSnapshot.state == .terminationCancelled,
              controlSnapshot.transferAdmissionOpen == true,
              controlSnapshot.namespaceOwnersRestored == true,
              controlSnapshot.servingGeneration == snapshot.servingGeneration
        else { return false }
        return await Self.verifyHydrationRoundTrip(socketURL: layout.hydrationSocket, timeout: healthTimeout)
    }

    /// Sends a deliberately incompatible request. A typed internal-error reply
    /// proves that the live hydration server parsed and answered the protocol
    /// without admitting a transfer or fetching user content.
    private static func verifyHydrationRoundTrip(socketURL: URL, timeout: Duration) async -> Bool {
        let client = AgentHydrationClient(socketURL: { socketURL }, idleTimeout: timeout)
        var request = HydrationRequest(
            accountId: 0, itemId: "termination-serving-probe", contentVersion: nil
        )
        request.protocolVersion = HydrationContract.protocolVersion + 1
        do {
            _ = try await client.hydrate(request, onProgress: { _ in })
            return false
        } catch let failure as HydrationFailure {
            return failure.category == .internalError
        } catch {
            return false
        }
    }

    /// Replacement is a two-phase agent handoff: only a UUID-correlated ready
    /// old helper may receive the irreversible commit. An older helper that
    /// rolls back remains serving the current profile/domain and is never
    /// replaced speculatively.
    private func waitForPreparedTermination(request: ControlTerminationRequest) async -> Bool {
        let deadline = ContinuousClock.now + controlConnectTimeout
        while ContinuousClock.now < deadline {
            switch await Self.health(socketURL: layout.healthSocket, timeout: healthTimeout) {
            case let .running(snapshot)
                where snapshot.terminationRequestID == request.requestID
                && snapshot.state == .terminationReady:
                return true
            case let .running(snapshot)
                where snapshot.terminationRequestID == request.requestID
                && snapshot.state == .terminationCancelled:
                return false
            case .notRunning, .timedOut, .error, .running:
                try? await Task.sleep(for: controlRetryInterval)
            }
        }
        return false
    }

    static func isOlderBuild(_ agent: String, than app: String) -> Bool {
        buildCompatibility(agent: agent, app: app) == .older
    }

    enum BuildCompatibility: Equatable {
        case matching
        case older
        case incompatible
    }

    static func buildCompatibility(agent: String, app: String) -> BuildCompatibility {
        guard let agentNumber = UInt64(agent), let appNumber = UInt64(app) else {
            return .incompatible
        }
        if agentNumber == appNumber, agent == app { return .matching }
        return agentNumber < appNumber ? .older : .incompatible
    }

    private func waitForMatchingAgentHierarchy() async -> HealthReadout {
        let deadline = ContinuousClock.now + controlConnectTimeout
        while ContinuousClock.now < deadline {
            let readout = await Self.health(socketURL: layout.healthSocket, timeout: healthTimeout)
            if case let .running(snapshot) = readout, Self.isReadyReplacement(snapshot, appBuild: appBuild) {
                await matchingAgentReady?()
                return readout
            }
            try? await Task.sleep(for: controlRetryInterval)
        }
        return .error(
            "replacement agent did not report the matching build with a ready File Provider hierarchy"
        )
    }

    static func isReadyReplacement(
        _ snapshot: AgentHealthSnapshot,
        appBuild: String = AgentBuildVersion.current
    ) -> Bool {
        buildCompatibility(
            agent: snapshot.bundleVersion ?? "", app: appBuild
        ) == .matching
            && snapshot.state == .running
            && snapshot.finderContentState == .ready
            && snapshot.finderFirstPageItemCount != nil
    }

    private static func health(socketURL: URL, timeout: Duration) async -> HealthReadout {
        // The health read is a blocking socket round-trip; keep it off the
        // caller's thread (the UI's main actor in production).
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .utility).async {
                continuation.resume(
                    returning: Self.readHealth(socketURL: socketURL, timeout: timeout)
                )
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
                timeout: .seconds(5)
            )
        }
    }

    // MARK: - Commands

    public func makeAuthorizationSession() -> any AuthorizationSession {
        let ensurer = self.ensurer
        let controlSocket = layout.controlSocket
        let controlConnectTimeout = self.controlConnectTimeout
        let controlRetryInterval = self.controlRetryInterval
        return LiveAuthorizationSession(openChannel: {
            if case .failed = await ensurer.ensureRunning() {
                return .unavailable(.agentNotRunning)
            }
            return await Self.openAuthorizationChannel(
                socketURL: controlSocket,
                timeout: controlConnectTimeout,
                retryInterval: controlRetryInterval
            )
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
        guard case let .running(snapshot) = await fetchHealth() else {
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
                    accountId: target.accountId, revokeSession: true
                )
            ),
            ensureAgent: false,
            timeout: .seconds(180)
        )
        guard case .completed = outcome else {
            return outcome
        }
        // The app half of SEC-004: deregister the account's File Provider
        // domain, after the engine dropped the canonical row.
        await accountDomainCleanup?(target.accountId)
        return .completed
    }

    public func fetchContentPolicy(
        accountId: Int64
    ) async -> PolicyOutcome<ControlContentPolicyStatus> {
        await policyCommand(
            ControlRequest(
                operation: .contentPolicyStatus,
                contentPolicy: ControlContentPolicyRequest(accountId: accountId)
            )
        ) { event in
            guard case let .contentPolicyStatus(status) = event else { return nil }
            return status
        }
    }

    public func setRetention(
        accountId: Int64,
        target: ControlRetentionMode,
        typedConfirmation: String?
    ) async -> PolicyOutcome<ControlRetentionTransition> {
        await policyCommand(
            ControlRequest(
                operation: .setRetention,
                contentPolicy: ControlContentPolicyRequest(
                    accountId: accountId,
                    retention: target,
                    typedConfirmation: typedConfirmation
                )
            )
        ) { event in
            guard case let .retentionChanged(transition) = event else { return nil }
            return transition
        }
    }

    public func setArchiveMode(
        accountId: Int64,
        enabled: Bool
    ) async -> PolicyOutcome<ControlArchiveModeTransition> {
        await policyCommand(
            ControlRequest(
                operation: .setArchiveMode,
                contentPolicy: ControlContentPolicyRequest(
                    accountId: accountId,
                    archiveModeEnabled: enabled
                )
            )
        ) { event in
            guard case let .archiveModeChanged(transition) = event else { return nil }
            return transition
        }
    }

    public func resumeRetentionPurge(
        accountId: Int64
    ) async -> PolicyOutcome<ControlRetentionPurgeResume> {
        await policyCommand(
            ControlRequest(
                operation: .resumeRetentionPurge,
                contentPolicy: ControlContentPolicyRequest(accountId: accountId)
            )
        ) { event in
            guard case let .retentionPurgeResumed(resume) = event else { return nil }
            return resume
        }
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
        switch await Self.sendControlCommand(
            request,
            socketURL: controlSocket,
            commandTimeout: timeout,
            connectTimeout: controlConnectTimeout,
            retryInterval: controlRetryInterval
        ) {
        case let .success(received):
            event = received
        case let .failure(error as ControlTransportError):
            switch error {
            case .agentUnavailable: return .unavailable(.agentNotRunning)
            case .timedOut, .protocolViolation: return .unavailable(.dropped)
            }
        case .failure:
            return .unavailable(.dropped)
        }
        switch event {
        case .commandDone, .terminationCommitAccepted:
            return .completed
        case let .commandFailed(failure):
            return .failed(Self.commandFailure(failure))
        case .status, .settings, .authState, .authSubmitResult,
             .contentPolicyStatus, .retentionChanged, .archiveModeChanged,
             .retentionPurgeResumed:
            return .unavailable(.dropped)
        }
    }

    private func policyCommand<Value: Equatable & Sendable>(
        _ request: ControlRequest,
        extract: @escaping @Sendable (ControlEvent) -> Value?
    ) async -> PolicyOutcome<Value> {
        if case .failed = await ensurer.ensureRunning() {
            return .unavailable(.agentNotRunning)
        }
        switch await Self.sendControlCommand(
            request,
            socketURL: layout.controlSocket,
            commandTimeout: .seconds(180),
            connectTimeout: controlConnectTimeout,
            retryInterval: controlRetryInterval
        ) {
        case let .success(event):
            if let value = extract(event) {
                return .value(value)
            }
            if case let .commandFailed(failure) = event {
                return .failed(Self.commandFailure(failure))
            }
            return .unavailable(.dropped)
        case let .failure(error as ControlTransportError):
            switch error {
            case .agentUnavailable: return .unavailable(.agentNotRunning)
            case .timedOut, .protocolViolation: return .unavailable(.dropped)
            }
        case .failure:
            return .unavailable(.dropped)
        }
    }

    static func commandFailure(_ failure: ControlCommandFailure) -> CommandFailure {
        switch failure.category {
        case .busy: return .busy
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

    /// Opens the interactive channel with a bounded retry only for the
    /// connect-before-listen race. Protocol failures and dropped sessions are
    /// never replayed: auth requests may have reached the agent in those
    /// cases, so retrying them would be unsafe.
    private static func openAuthorizationChannel(
        socketURL: URL,
        timeout: Duration,
        retryInterval: Duration
    ) async -> LiveAuthorizationSession.ChannelOpen {
        let deadline = ContinuousClock.now + timeout
        while true {
            do {
                let channel = try await openAuthorizationChannel(socketURL: socketURL)
                return .opened(channel)
            } catch ControlTransportError.agentUnavailable where ContinuousClock.now < deadline {
                try? await Task.sleep(for: retryInterval)
            } catch ControlTransportError.agentUnavailable {
                return .unavailable(.agentNotRunning)
            } catch {
                return .unavailable(.dropped)
            }
        }
    }

    private static func openAuthorizationChannel(socketURL: URL) async throws
        -> ControlAuthChannel
    {
        try await withCheckedThrowingContinuation { continuation in
            DispatchQueue.global(qos: .utility).async {
                continuation.resume(
                    with: Result { try ControlAuthChannel.open(socketURL: socketURL) }
                )
            }
        }
    }

    /// One-shot commands use the same bounded connect-only retry. A command is
    /// retried only when `connect(2)` proved no listener existed, before any
    /// request bytes could have been accepted.
    private static func sendControlCommand(
        _ request: ControlRequest,
        socketURL: URL,
        commandTimeout: Duration,
        connectTimeout: Duration,
        retryInterval: Duration
    ) async -> Result<ControlEvent, Error> {
        let deadline = ContinuousClock.now + connectTimeout
        while true {
            do {
                let event: ControlEvent = try await withCheckedThrowingContinuation {
                    continuation in
                    DispatchQueue.global(qos: .utility).async {
                        continuation.resume(
                            with: Result {
                                try ControlClient.command(
                                    request, socketURL: socketURL, timeout: commandTimeout
                                )
                            }
                        )
                    }
                }
                return .success(event)
            } catch ControlTransportError.agentUnavailable where ContinuousClock.now < deadline {
                try? await Task.sleep(for: retryInterval)
            } catch {
                return .failure(error)
            }
        }
    }
}

extension LiveCompanionBackend: CompanionTerminationPreparing {
    /// Requests the agent's one bounded drain without starting an absent agent.
    /// Quitting must not resurrect a stopped helper merely to ask it to stop.
    public func prepareForTermination(
        reason: ControlTerminationRequest.Reason,
        targetBuild: String? = nil
    ) async -> CommandOutcome {
        guard case let .running(snapshot) = await fetchAgentHealthWithoutRelaunch(),
              let identity = snapshot.processIdentity,
              identity.isValidTerminationIdentity
        else {
            return .unavailable(.agentNotRunning)
        }
        return await prepareForTermination(
            ControlTerminationRequest(
                expectedAgentInstanceID: identity.instanceID,
                reason: reason,
                targetBuild: targetBuild
            )
        )
    }

    /// Sends the exact request identity used by the AppKit coordinator. This
    /// preserves response-loss reconciliation and the matching cancel path.
    public func prepareForTermination(_ request: ControlTerminationRequest) async -> CommandOutcome {
        await command(
            ControlRequest(
                operation: .prepareForTermination,
                termination: request
            ),
            ensureAgent: false,
            timeout: .seconds(20)
        )
    }
}
