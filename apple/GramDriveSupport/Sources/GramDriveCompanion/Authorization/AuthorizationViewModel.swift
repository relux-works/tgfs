import Foundation
import GramDriveAgentCore

/// The authorization conclusion the Sign In screen can safely derive from
/// agent health when no live authorization session owns the screen.
public enum AuthorizationHealthState: Equatable, Sendable {
    /// No health reconciliation has run in this companion process yet.
    case unknown
    /// At least one live account namespace is definitively authorized.
    case authorized
    /// The agent definitively observed that authorization is required.
    case authorizationRequired
    /// Health cannot currently establish either terminal authorization result.
    case unavailable
}

/// Drives one authorization flow and renders its state.
///
/// The state stream from the ``AuthorizationSession`` is the single source of
/// truth for what screen shows — inputs never move the rendered state on
/// their own (the same rule the core `AuthMachine` follows: TDLib's reported
/// state advances the flow, an input only provokes the next report). The view
/// model forwards user actions, classifies their immediate result (a
/// rejection with advice, an invalid-for-state input, a dropped channel), and
/// leaves the resulting state to arrive on the stream.
@MainActor
@Observable
public final class AuthorizationViewModel {
    /// The current authorization screen.
    public private(set) var state: CompanionAuthState = .idle
    /// Every state entered, in order — the flow's observable trail (drivers
    /// and tests assert progression against it).
    public private(set) var stateHistory: [CompanionAuthState] = []
    /// The most recent rejection TDLib returned for a submitted request, with
    /// its ``CompanionRetryAdvice``. Cleared when the flow advances.
    public private(set) var lastRejection: CompanionAuthRejection?
    /// The most recent input that was structurally invalid for the current
    /// state (a caller-side condition; the flow position did not change).
    public private(set) var lastInvalidInput: CompanionAuthInput?
    /// Set when no control channel can drive authorization — an honest
    /// terminal for the screen, distinct from any auth state.
    public private(set) var unavailable: ControlChannelUnavailable?
    /// True while a session transition or control-channel round-trip is in flight.
    public private(set) var isSubmitting: Bool = false
    /// True only while the user explicitly cancels a live authorization flow.
    /// Kept separate from ordinary input submission so the view can show
    /// accurate progress without mislabeling phone/code/password requests.
    public private(set) var isCancelling: Bool = false
    /// The latest health-derived authorization conclusion. This supplements
    /// the rendered auth state; it never overrides an active session stream.
    public private(set) var healthState: AuthorizationHealthState = .unknown

    private let backend: any CompanionBackend
    private let teardownTimeout: Duration
    private var session: (any AuthorizationSession)?
    private var consumeTask: Task<Void, Never>?
    private var activeSessionID: UUID?
    private var activeFlowID: UUID?
    private var activeOperations: Set<UUID> = []

    public init(backend: any CompanionBackend, teardownTimeout: Duration = .seconds(15)) {
        self.backend = backend
        self.teardownTimeout = teardownTimeout
    }

    /// The advice for the current rejection, if any.
    public var advice: CompanionRetryAdvice? { lastRejection?.advice }

    /// Whether the flow has finished successfully.
    public var isAuthorized: Bool { state == .ready }

    /// Reconciles the screen from agent health without opening an auth control
    /// channel. A live session remains the authority until its stream ends.
    public func reconcile(with readout: HealthReadout) {
        guard activeFlowID == nil, !isCancelling else { return }

        let nextHealthState = Self.healthState(from: readout)
        healthState = nextHealthState
        unavailable = nil
        lastRejection = nil
        lastInvalidInput = nil

        switch nextHealthState {
        case .authorized:
            applyIfChanged(.ready)
        case .authorizationRequired:
            applyIfChanged(.idle)
        case .unavailable:
            // A transient inability to observe health is weaker evidence than
            // an already established ready state. Keep signed-in UI stable,
            // but expose the unavailable observation alongside it.
            if state != .ready { applyIfChanged(.idle) }
        case .unknown:
            break
        }
    }

    /// Privacy-safe projection: account identity and display names never leave
    /// the health snapshot. Definitive observations outrank indeterminate ones.
    public nonisolated static func healthState(
        from readout: HealthReadout
    ) -> AuthorizationHealthState {
        guard case .running(let snapshot) = readout,
              let accounts = snapshot.accounts
        else { return .unavailable }
        if accounts.contains(where: { $0.observedAuthorization == .authorized }) {
            return .authorized
        }
        if accounts.contains(where: { $0.observedAuthorization == .authorizationRequired }) {
            return .authorizationRequired
        }
        if accounts.isEmpty
            || accounts.contains(where: {
                $0.observedAuthorization == nil && $0.authState != "authorized"
            })
        {
            return .authorizationRequired
        }
        return .unavailable
    }

    /// Starts a fresh authorization session and begins rendering its states.
    public func begin() async {
        guard !isSubmitting else { return }
        let operationID = beginOperation()
        defer { endOperation(operationID) }

        reset()
        // A tap must be acknowledged before a stale namespace's teardown can
        // finish. The agent remains authoritative for every later state.
        apply(.starting)
        let flowID = UUID()
        activeFlowID = flowID
        let teardownFailure = await endExistingSession()
        guard teardownFailure == nil, activeFlowID == flowID else {
            if activeFlowID == flowID {
                unavailable = teardownFailure ?? .dropped
                state = .idle
                activeFlowID = nil
            }
            return
        }
        let session = backend.makeAuthorizationSession()
        let sessionID = UUID()
        self.session = session
        activeSessionID = sessionID
        let result = await session.start()
        guard activeFlowID == flowID, activeSessionID == sessionID else { return }
        switch result {
        case .unavailable(let reason):
            unavailable = reason
            state = .idle
            clearSession(sessionID)
            activeFlowID = nil
        case .started:
            let states = session.states
            consumeTask = Task { [weak self] in
                for await next in states {
                    guard !Task.isCancelled else { return }
                    self?.apply(next, for: sessionID)
                }
                self?.finishFlow(flowID, sessionID: sessionID)
            }
        }
    }

    /// Submits one user action, unless it is structurally invalid for the
    /// current state (which is recorded, not sent). The resulting state, when
    /// the agent accepts the action, arrives on the state stream.
    public func submit(_ input: CompanionAuthInput) async {
        guard !isSubmitting else { return }
        guard input.isValid(in: state) else {
            lastInvalidInput = input
            return
        }
        guard let session, let sessionID = activeSessionID else {
            unavailable = .agentNotRunning
            return
        }
        lastInvalidInput = nil
        let operationID = beginOperation()
        defer { endOperation(operationID) }
        let result = await session.submit(input)
        guard activeSessionID == sessionID else { return }
        switch result {
        case .accepted:
            lastRejection = nil
        case .rejected(let rejection):
            lastRejection = rejection
        case .invalidForState:
            lastInvalidInput = input
        case .unavailable(let reason):
            unavailable = reason
        }
    }

    /// Abandons the flow locally (`cancel`) and stops rendering.
    public func cancel() async {
        // Repeated activation is deliberately coalesced. In particular, the
        // second task must not clear the submission flag while the first is
        // still bounded on namespace teardown.
        guard !isCancelling else { return }
        activeOperations.removeAll()
        let operationID = beginOperation()
        isCancelling = true
        activeFlowID = nil
        defer {
            isCancelling = false
            endOperation(operationID)
        }

        reset()
        state = .starting
        let teardownFailure = await endExistingSession()
        guard isCancelling else { return }
        if let teardownFailure { unavailable = teardownFailure }
        state = .idle
    }

    /// Awaits the state stream ending (it ends on `closed`, `cancel`, or a
    /// dropped channel). Used by drivers and tests to observe the flow to
    /// quiescence.
    public func waitForCompletion() async {
        guard let completion = consumeTask, let sessionID = activeSessionID else { return }
        let completed = await waitForTask(completion)
        guard activeSessionID == sessionID else { return }
        if !completed {
            unavailable = .timedOut
            _ = await endExistingSession()
            activeFlowID = nil
            state = .idle
        }
    }

    // MARK: - Internals

    /// Applies one reported state. The single place the rendered state moves.
    private func apply(_ next: CompanionAuthState, for sessionID: UUID? = nil) {
        if let sessionID, activeSessionID != sessionID { return }
        state = next
        stateHistory.append(next)
        // A genuine transition clears a stale rejection; a re-entry of the
        // same variant (a fresh QR link, re-sent code info) keeps it, since
        // the user's last action was not resolved by staying put.
        if next.kind != stateHistory.dropLast().last?.kind {
            lastRejection = nil
            lastInvalidInput = nil
        }
    }

    private func applyIfChanged(_ next: CompanionAuthState) {
        guard state != next else { return }
        apply(next)
    }

    private func finishFlow(_ flowID: UUID, sessionID: UUID) {
        guard activeFlowID == flowID, activeSessionID == sessionID else { return }
        activeFlowID = nil
    }

    /// Ends an existing control-channel session before opening another one.
    /// This is also the QR-to-phone fallback: TDLib rotates QR links itself,
    /// but changing auth methods requires a fresh phone-capable session.
    private func endExistingSession() async -> ControlChannelUnavailable? {
        let existingSession = session
        let existingConsumption = consumeTask
        session = nil
        consumeTask = nil
        activeSessionID = nil
        existingConsumption?.cancel()

        guard let existingSession else { return nil }
        let cancellationFailure = await waitForSessionCancellation(existingSession)
        let consumed = await waitForTask(existingConsumption)
        return cancellationFailure ?? (consumed ? nil : .timedOut)
    }

    private func clearSession(_ sessionID: UUID) {
        guard activeSessionID == sessionID else { return }
        session = nil
        consumeTask = nil
        activeSessionID = nil
    }

    private func beginOperation() -> UUID {
        let operationID = UUID()
        activeOperations.insert(operationID)
        isSubmitting = true
        return operationID
    }

    private func endOperation(_ operationID: UUID) {
        activeOperations.remove(operationID)
        isSubmitting = !activeOperations.isEmpty
    }

    private func waitForSessionCancellation(
        _ session: any AuthorizationSession
    ) async -> ControlChannelUnavailable? {
        await withDeadline(
            timeout: teardownTimeout,
            operation: {
                await session.cancel()
            },
            timedOut: .timedOut)
    }

    private func waitForTask(_ task: Task<Void, Never>?) async -> Bool {
        guard let task else { return true }
        return await withDeadline(
            timeout: teardownTimeout,
            operation: {
                await task.value
                return true
            },
            timedOut: false)
    }

    private func withDeadline<Value: Sendable>(
        timeout: Duration,
        operation: @escaping @Sendable () async -> Value,
        timedOut: @autoclosure @escaping @Sendable () -> Value
    ) async -> Value {
        let gate = AuthorizationDeadlineGate<Value>(timedOut: timedOut)
        return await withCheckedContinuation { continuation in
            gate.install(continuation)
            Task.detached { [gate] in
                gate.resolve(await operation())
            }
            Task.detached { [gate, timeout] in
                try? await Task.sleep(for: timeout)
                gate.timeout()
            }
        }
    }

    private func reset() {
        state = .idle
        stateHistory = []
        lastRejection = nil
        lastInvalidInput = nil
        unavailable = nil
        healthState = .unknown
    }
}

private final class AuthorizationDeadlineGate<Value: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private let timedOut: @Sendable () -> Value
    private var continuation: CheckedContinuation<Value, Never>?
    private var resolved = false

    init(timedOut: @escaping @Sendable () -> Value) {
        self.timedOut = timedOut
    }

    func install(_ continuation: CheckedContinuation<Value, Never>) {
        lock.lock()
        self.continuation = continuation
        lock.unlock()
    }

    func resolve(_ value: Value) {
        lock.lock()
        guard !resolved else {
            lock.unlock()
            return
        }
        resolved = true
        let continuation = self.continuation
        self.continuation = nil
        lock.unlock()
        continuation?.resume(returning: value)
    }

    func timeout() {
        resolve(timedOut())
    }
}
