import Darwin
import Foundation
import GramDriveAgentCore

/// AppKit exposes no completion token for `.terminateLater`; this seam owns
/// the sole reply to the original termination request. It keeps joined drains,
/// explicit cancellation, and late task results from replying twice.
@MainActor
public final class ApplicationTerminationReplyGate {
    private var pending = false

    public init() {}

    public var isPending: Bool {
        pending
    }

    public func begin() -> Bool {
        guard !pending else { return false }
        pending = true
        return true
    }

    public func takeReply(_ allowed: Bool) -> Bool? {
        guard pending else { return nil }
        pending = false
        return allowed
    }
}

/// The companion-side half of the File Provider-safe quit/update contract.
/// One coordinator coalesces every AppKit termination request into one bounded
/// control drain; callers receive `false` on any timeout or channel failure so
/// the current application version remains running.
@MainActor
public final class CompanionTerminationCoordinator {
    public enum Intent: Equatable, Sendable {
        case userQuit
        case update(targetBuild: String)

        public static func fromPendingUpdateBuild(_ pendingUpdateBuild: String?) -> Self {
            if let pendingUpdateBuild {
                return .update(targetBuild: pendingUpdateBuild)
            }
            return .userQuit
        }

        func controlRequest(expectedAgentInstanceID: UUID) -> ControlTerminationRequest {
            switch self {
            case .userQuit:
                return ControlTerminationRequest(
                    expectedAgentInstanceID: expectedAgentInstanceID,
                    reason: .userQuit
                )
            case let .update(targetBuild):
                return ControlTerminationRequest(
                    expectedAgentInstanceID: expectedAgentInstanceID,
                    reason: .update,
                    targetBuild: targetBuild
                )
            }
        }
    }

    private let prepare: @Sendable (ControlTerminationRequest) async -> CommandOutcome
    private let cancel: @Sendable (ControlTerminationRequest) async -> CommandOutcome
    private let commit: @Sendable (ControlTerminationRequest) async -> CommandOutcome
    private let health: @Sendable () async -> HealthReadout
    private let recoverCurrentBuild: @Sendable () async -> Bool
    private let pollInterval: Duration
    private let timeout: Duration
    private let cancellationTimeout: Duration
    private let explicitCancellationJoinObserver: @Sendable () -> Void
    private var drainTask: Task<DrainResult, Never>?
    private var activeRequest: ControlTerminationRequest?
    private var explicitCancellationTask: Task<CommandOutcome, Never>?
    /// Shown by the AppKit delegate after it replies `false`. It explicitly
    /// names the safe retry/Force Quit boundary rather than silently leaving an
    /// update pending after an unsuccessful drain.
    public private(set) var lastFailureMessage: String?

    private enum DrainResult {
        case allowed
        case cancelled(String)

        var isAllowed: Bool {
            if case .allowed = self { return true }
            return false
        }

        var failureMessage: String? {
            if case let .cancelled(message) = self { return message }
            return nil
        }
    }

    public convenience init(
        prepare: @escaping @Sendable (ControlTerminationRequest) async -> CommandOutcome,
        cancel: @escaping @Sendable (ControlTerminationRequest) async -> CommandOutcome = {
            _ in .unavailable(.dropped)
        },
        commit: @escaping @Sendable (ControlTerminationRequest) async -> CommandOutcome = {
            _ in .unavailable(.dropped)
        },
        health: @escaping @Sendable () async -> HealthReadout,
        recoverCurrentBuild: @escaping @Sendable () async -> Bool = { false },
        pollInterval: Duration = .milliseconds(100),
        timeout: Duration = .seconds(20),
        cancellationTimeout: Duration = .seconds(5)
    ) {
        self.init(
            prepare: prepare,
            cancel: cancel,
            commit: commit,
            health: health,
            recoverCurrentBuild: recoverCurrentBuild,
            pollInterval: pollInterval,
            timeout: timeout,
            cancellationTimeout: cancellationTimeout,
            explicitCancellationJoinObserver: {}
        )
    }

    init(
        prepare: @escaping @Sendable (ControlTerminationRequest) async -> CommandOutcome,
        cancel: @escaping @Sendable (ControlTerminationRequest) async -> CommandOutcome,
        commit: @escaping @Sendable (ControlTerminationRequest) async -> CommandOutcome = {
            _ in .unavailable(.dropped)
        },
        health: @escaping @Sendable () async -> HealthReadout,
        recoverCurrentBuild: @escaping @Sendable () async -> Bool = { false },
        pollInterval: Duration = .milliseconds(100),
        timeout: Duration = .seconds(20),
        cancellationTimeout: Duration = .seconds(5),
        explicitCancellationJoinObserver: @escaping @Sendable () -> Void
    ) {
        self.prepare = prepare
        self.cancel = cancel
        self.commit = commit
        self.health = health
        self.recoverCurrentBuild = recoverCurrentBuild
        self.pollInterval = pollInterval
        self.timeout = timeout
        self.cancellationTimeout = cancellationTimeout
        self.explicitCancellationJoinObserver = explicitCancellationJoinObserver
    }

    /// Creates the production coordinator over the companion's live IPC
    /// boundary. Every phase reuses the same backend and passes the original
    /// request UUID through unchanged, including the irreversible commit.
    public static func live(
        layout: AgentRuntimeLayout,
        healthTimeout: Duration = .seconds(5),
        pollInterval: Duration = .milliseconds(100),
        timeout: Duration = .seconds(20),
        cancellationTimeout: Duration = .seconds(5)
    ) -> CompanionTerminationCoordinator {
        let backend = LiveCompanionBackend(layout: layout, healthTimeout: healthTimeout)
        return CompanionTerminationCoordinator(
            prepare: { request in await backend.prepareForTermination(request) },
            cancel: { request in await backend.prepareForTermination(request) },
            commit: { request in await backend.prepareForTermination(request) },
            health: { await backend.fetchAgentHealthWithoutRelaunch() },
            recoverCurrentBuild: { await backend.recoverCurrentBuildForTerminationRollback() },
            pollInterval: pollInterval,
            timeout: timeout,
            cancellationTimeout: cancellationTimeout
        )
    }

    /// Starts or joins the current drain. One joined request never sends a
    /// second control command, which preserves the agent's single drain.
    public func requestTermination(_ intent: Intent) async -> Bool {
        if let drainTask {
            let result = await drainTask.value
            lastFailureMessage = result.failureMessage
            return result.isAllowed
        }
        let prepare = self.prepare
        let health = self.health
        let pollInterval = self.pollInterval
        let timeout = self.timeout
        let cancellationTimeout = self.cancellationTimeout
        let commit = self.commit
        let recoverCurrentBuild = self.recoverCurrentBuild
        let joinObserver = explicitCancellationJoinObserver
        let task = Task<DrainResult, Never> { @MainActor [self] in
            // Capture the process identity after the task has been installed
            // as the join point. This preserves one drain for simultaneous
            // AppKit callbacks while preventing a replacement at the same
            // socket path from accepting delayed control bytes.
            let initialReadout = await health()
            if case .notRunning = initialReadout {
                // There is no old process to mutate or observe.
                return .allowed
            }
            guard case let .running(snapshot) = initialReadout,
                  let identity = snapshot.processIdentity,
                  identity.isValidTerminationIdentity
            else {
                return .cancelled(
                    "GramDrive could not capture the running agent identity. The app remains open; try again after the agent is healthy, or use Force Quit if you need to stop immediately."
                )
            }
            let request = intent.controlRequest(expectedAgentInstanceID: identity.instanceID)
            activeRequest = request
            let exitWitness = ProcessExitWitness(identity: identity)
            switch await prepare(request) {
            case .completed:
                break
            case .unavailable(.agentNotRunning):
                guard exitWitness.didObserveExit || identity.observe().provesCapturedProcessExited else {
                    return .cancelled(
                        "GramDrive could not prove that the running agent accepted the quit request. The app remains open; try again, or use Force Quit if you need to stop immediately."
                    )
                }
                return .allowed
            case .unavailable(.dropped):
                // The server records `.draining` before it closes its successful
                // acknowledgement. A lost response is therefore not a safe reason
                // to reply false; reconcile the request-correlated health instead.
                break
            case .unavailable, .failed:
                return .cancelled(
                    "GramDrive could not ask its agent to prepare for quitting. The app remains open; try again, or use Force Quit if you need to stop immediately."
                )
            }
            let deadline = ContinuousClock.now + timeout
            let acceptanceDeadline = ContinuousClock.now + .seconds(1)
            while ContinuousClock.now < deadline {
                if let explicitCancellationTask {
                    joinObserver()
                    return await Self.cancelAndReconcile(
                        request: request,
                        cancel: cancel,
                        health: health,
                        pollInterval: pollInterval,
                        timeout: cancellationTimeout,
                        identity: identity,
                        recoverCurrentBuild: recoverCurrentBuild,
                        initialCancellation: explicitCancellationTask
                    )
                }
                switch await health() {
                case .notRunning:
                    guard exitWitness.didObserveExit || identity.observe().provesCapturedProcessExited else {
                        return await Self.cancelAndReconcile(
                            request: request,
                            cancel: cancel,
                            health: health,
                            pollInterval: pollInterval,
                            timeout: cancellationTimeout,
                            identity: identity,
                            recoverCurrentBuild: recoverCurrentBuild
                        )
                    }
                    return .allowed
                case let .running(snapshot)
                    where snapshot.terminationRequestID == request.requestID
                    && Self.rollbackIsUsable(snapshot, for: request, identity: identity):
                    return .cancelled(
                        "GramDrive could not safely stop all File Provider transfers. The current version remains open; try quitting again after transfers settle, or use Force Quit if you need to stop immediately."
                    )
                case let .running(snapshot)
                    where snapshot.terminationRequestID == request.requestID
                    && snapshot.state == .terminationReady:
                    guard snapshot.processIdentity != nil else {
                        // Do not cross the irreversible boundary for an
                        // older helper whose process identity cannot be
                        // witnessed safely. Its ready lease/cancel path
                        // keeps the current version usable instead.
                        return await Self.cancelAndReconcile(
                            request: request,
                            cancel: cancel,
                            health: health,
                            pollInterval: pollInterval,
                            timeout: cancellationTimeout,
                            identity: identity,
                            recoverCurrentBuild: recoverCurrentBuild
                        )
                    }
                    return await Self.commitPreparedTermination(
                        request: request,
                        identity: snapshot.processIdentity!,
                        commit: commit,
                        cancel: cancel,
                        health: health,
                        pollInterval: pollInterval,
                        cancellationTimeout: cancellationTimeout,
                        recoverCurrentBuild: recoverCurrentBuild
                    )
                case let .running(snapshot)
                    where snapshot.terminationRequestID == request.requestID
                    && snapshot.state == .stopped:
                    // `.stopped` is valid only after this coordinator sends a
                    // matching commit. Seeing it before that boundary must not
                    // turn a stale or foreign teardown into an AppKit `true`.
                    return await Self.cancelAndReconcile(
                        request: request,
                        cancel: cancel,
                        health: health,
                        pollInterval: pollInterval,
                        timeout: cancellationTimeout,
                        identity: identity,
                        recoverCurrentBuild: recoverCurrentBuild
                    )
                case let .running(snapshot)
                    where snapshot.terminationRequestID == request.requestID
                    && snapshot.state == .draining:
                    try? await Task.sleep(for: pollInterval)
                case .running where ContinuousClock.now < acceptanceDeadline:
                    // A response can arrive from the kernel just before the server
                    // synchronously records the drain. Do not race that transition.
                    try? await Task.sleep(for: pollInterval)
                case .running:
                    return await Self.cancelAndReconcile(
                        request: request,
                        cancel: cancel,
                        health: health,
                        pollInterval: pollInterval,
                        timeout: cancellationTimeout,
                        identity: identity,
                        recoverCurrentBuild: recoverCurrentBuild
                    )
                case .timedOut, .error:
                    try? await Task.sleep(for: pollInterval)
                }
            }
            return await Self.cancelAndReconcile(
                request: request,
                cancel: cancel,
                health: health,
                pollInterval: pollInterval,
                timeout: cancellationTimeout,
                identity: identity,
                recoverCurrentBuild: recoverCurrentBuild
            )
        }
        drainTask = task
        let result = await task.value
        drainTask = nil
        activeRequest = nil
        explicitCancellationTask = nil
        lastFailureMessage = result.failureMessage
        return result.isAllowed
    }

    /// Explicitly keeps the current version open while a drain is in flight.
    /// It joins the same bounded task; it never emits a second termination
    /// decision or starts a second agent shutdown.
    public func cancelTermination() async -> Bool {
        guard let activeRequest, let drainTask else { return false }
        let cancellationTask: Task<CommandOutcome, Never>
        if let explicitCancellationTask {
            cancellationTask = explicitCancellationTask
        } else {
            var request = activeRequest
            request.action = .cancel
            let cancel = self.cancel
            let task = Task { await cancel(request) }
            explicitCancellationTask = task
            cancellationTask = task
        }
        _ = await cancellationTask.value
        let result = await drainTask.value
        lastFailureMessage = result.failureMessage
        return result.isAllowed
    }

    /// A timeout is not itself a safe `false` reply: an accepted drain might
    /// otherwise exit after AppKit gives up. This uses the prepared-drain
    /// lease: because this path never sends commit, expiry restores the same
    /// current agent even if both cancellation and health responses are lost.
    /// The helper is intentionally bounded by `timeout`.
    private static func cancelAndReconcile(
        request: ControlTerminationRequest,
        cancel: @escaping @Sendable (ControlTerminationRequest) async -> CommandOutcome,
        health: @escaping @Sendable () async -> HealthReadout,
        pollInterval: Duration,
        timeout: Duration,
        identity: AgentProcessIdentity,
        recoverCurrentBuild: @escaping @Sendable () async -> Bool,
        initialCancellation: Task<CommandOutcome, Never>? = nil
    ) async -> DrainResult {
        var cancellation = request
        cancellation.action = .cancel
        if let initialCancellation {
            // The explicit AppKit path installs this task before awaiting the
            // command. Joining it here prevents the reconciliation loop from
            // retrying while the first cancellation is merely still in flight.
            _ = await initialCancellation.value
        } else {
            _ = await cancel(cancellation)
        }

        let deadline = ContinuousClock.now + timeout
        var nextCancellationAttempt = ContinuousClock.now + pollInterval
        while ContinuousClock.now < deadline {
            switch await health() {
            case .notRunning:
                if identity.observe().provesCapturedProcessExited { return .allowed }
                try? await Task.sleep(for: pollInterval)
            case let .running(snapshot)
                where snapshot.terminationRequestID == request.requestID
                && rollbackIsUsable(snapshot, for: request, identity: identity):
                return .cancelled(
                    "GramDrive could not safely stop all File Provider transfers. The current version remains open; try quitting again after transfers settle, or use Force Quit if you need to stop immediately."
                )
            case let .running(snapshot)
                where snapshot.terminationRequestID == request.requestID
                && (snapshot.state == .draining || snapshot.state == .terminationReady):
                try? await Task.sleep(for: pollInterval)
            case .running, .timedOut, .error:
                try? await Task.sleep(for: pollInterval)
            }

            // Retrying the exact cancellation is idempotent. It remains safe
            // to stop at the deadline because no commit has been sent, so the
            // agent's finite lease must restore admission and namespace owners.
            if ContinuousClock.now >= nextCancellationAttempt {
                _ = await cancel(cancellation)
                nextCancellationAttempt = ContinuousClock.now + pollInterval
            }
        }
        // The ready lease is recovery machinery, not proof. If its proof
        // channel remains unavailable, replace the exact old process with
        // the same current build; a failed replacement leaves old-process
        // death as the only safe terminal result.
        while !(await waitForExactProcessExit(identity, pollInterval: pollInterval)) {}
        if await recoverCurrentBuild() {
            return .cancelled(
                "GramDrive restored the current agent after the quit drain could not be confirmed. The app remains open; try quitting again, or use Force Quit if you need to stop immediately."
            )
        }
        return .allowed
    }

    /// Sends the sole irreversible action only after health has confirmed the
    /// request-correlated prepared state. A delivered commit is acknowledged
    /// before agent teardown begins. If its response is lost, an ambiguous
    /// state must not become a false reply: the app is already committing to
    /// termination, while an undelivered commit remains protected by the
    /// agent's rollback lease.
    private static func commitPreparedTermination(
        request: ControlTerminationRequest,
        identity: AgentProcessIdentity,
        commit: @escaping @Sendable (ControlTerminationRequest) async -> CommandOutcome,
        cancel: @escaping @Sendable (ControlTerminationRequest) async -> CommandOutcome,
        health: @escaping @Sendable () async -> HealthReadout,
        pollInterval: Duration,
        cancellationTimeout: Duration,
        recoverCurrentBuild: @escaping @Sendable () async -> Bool
    ) async -> DrainResult {
        // Register before the irreversible command crosses the socket. A
        // replacement can immediately reuse the endpoint path, so endpoint
        // disappearance is never the witness; this exact PID/start pair is.
        let exitWitness = ProcessExitWitness(identity: identity)
        var commitment = request
        commitment.action = .commit
        switch await commit(commitment) {
        case .unavailable(.agentNotRunning):
            return await awaitCommittedTermination(
                request: request,
                identity: identity,
                exitWitness: exitWitness,
                health: health,
                pollInterval: pollInterval,
                timeout: cancellationTimeout
            )
        case .completed, .unavailable(.dropped):
            // A commit response is not evidence that the old helper has
            // stopped. The control server reports acceptance only after it
            // atomically claims the request-correlated lease; then its host
            // tears down endpoints. A dropped response follows the same
            // reconciliation: either the endpoint disappears (allow) or the
            // prepared lease publishes cancellation (keep the app open).
            return await awaitCommittedTermination(
                request: request,
                identity: identity,
                exitWitness: exitWitness,
                health: health,
                pollInterval: pollInterval,
                timeout: cancellationTimeout
            )
        case .unavailable, .failed:
            return await cancelAndReconcile(
                request: request,
                cancel: cancel,
                health: health,
                pollInterval: pollInterval,
                timeout: cancellationTimeout,
                identity: identity,
                recoverCurrentBuild: recoverCurrentBuild
            )
        }
    }

    /// An irreversible commit is successful only after the old control/health
    /// endpoint disappears. A matching live `.stopped` payload means teardown
    /// is in progress, not that AppKit may yet be told to quit.
    private static func awaitCommittedTermination(
        request: ControlTerminationRequest,
        identity: AgentProcessIdentity,
        exitWitness: ProcessExitWitness,
        health: @escaping @Sendable () async -> HealthReadout,
        pollInterval: Duration,
        timeout: Duration
    ) async -> DrainResult {
        let deadline = ContinuousClock.now + timeout
        while ContinuousClock.now < deadline {
            switch await health() {
            case .notRunning:
                if exitWitness.didObserveExit || identity.observe().provesCapturedProcessExited {
                    return .allowed
                }
                try? await Task.sleep(for: pollInterval)
            case let .running(snapshot)
                where snapshot.terminationRequestID == request.requestID
                && snapshot.state == .stopped:
                try? await Task.sleep(for: pollInterval)
            case .running, .timedOut, .error:
                try? await Task.sleep(for: pollInterval)
            }
        }
        // A control acknowledgement or timeout never proves teardown. The
        // agent owns a two-second committed-exit watchdog; if its endpoint is
        // still not observable after the normal observation budget, use the
        // captured pid + kernel-start identity for TERM/KILL escalation and
        // require that exact identity to disappear before allowing AppKit.
        while !(await waitForExactProcessExit(identity, pollInterval: pollInterval)) {
            // An armed commit is irreversible. Do not convert an unobserved
            // teardown into `false`; repeat identity-checked escalation until
            // the exact old process is proven gone.
        }
        return .allowed
    }

    /// The process fallback is deliberately identity-checked before every
    /// signal. A PID that has been reused is proof that the captured old
    /// process exited, but it is never signalled as if it were the old agent.
    private static func waitForExactProcessExit(
        _ identity: AgentProcessIdentity,
        pollInterval: Duration
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

    private static func rollbackIsUsable(
        _ snapshot: AgentHealthSnapshot,
        for request: ControlTerminationRequest,
        identity: AgentProcessIdentity
    ) -> Bool {
        snapshot.terminationRequestID == request.requestID
            && snapshot.state == .terminationCancelled
            && snapshot.processIdentity == identity
            && snapshot.servingGeneration != nil
            && snapshot.transferAdmissionOpen == true
            && snapshot.namespaceOwnersRestored == true
            && snapshot.finderContentState == .ready
    }

    /// A process source is registered before commit, rather than after a
    /// socket error. This observes the captured process even when launchd
    /// publishes a replacement at the same pathname without a visible gap.
    private final class ProcessExitWitness: @unchecked Sendable {
        private let lock = NSLock()
        private var exited = false
        private let source: DispatchSourceProcess

        init(identity: AgentProcessIdentity) {
            source = DispatchSource.makeProcessSource(
                identifier: pid_t(identity.pid), eventMask: .exit, queue: .global(qos: .userInitiated)
            )
            source.setEventHandler { [weak self] in
                self?.lock.lock()
                self?.exited = true
                self?.lock.unlock()
            }
            source.resume()
        }

        deinit { source.cancel() }

        var didObserveExit: Bool {
            lock.lock()
            defer { lock.unlock() }
            return exited
        }
    }
}
