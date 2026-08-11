import Foundation

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

    private let backend: any CompanionBackend
    private var session: (any AuthorizationSession)?
    private var consumeTask: Task<Void, Never>?

    public init(backend: any CompanionBackend) {
        self.backend = backend
    }

    /// The advice for the current rejection, if any.
    public var advice: CompanionRetryAdvice? { lastRejection?.advice }

    /// Whether the flow has finished successfully.
    public var isAuthorized: Bool { state == .ready }

    /// Starts a fresh authorization session and begins rendering its states.
    public func begin() async {
        guard !isSubmitting else { return }
        isSubmitting = true
        defer { isSubmitting = false }

        await endExistingSession()
        reset()
        let session = backend.makeAuthorizationSession()
        self.session = session
        let result = await session.start()
        switch result {
        case .unavailable(let reason):
            unavailable = reason
        case .started:
            let states = session.states
            consumeTask = Task { [weak self] in
                for await next in states {
                    self?.apply(next)
                }
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
        guard let session else {
            unavailable = .agentNotRunning
            return
        }
        lastInvalidInput = nil
        isSubmitting = true
        defer { isSubmitting = false }
        let result = await session.submit(input)
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
        guard !isSubmitting else { return }
        isSubmitting = true
        defer { isSubmitting = false }

        await endExistingSession()
    }

    /// Awaits the state stream ending (it ends on `closed`, `cancel`, or a
    /// dropped channel). Used by drivers and tests to observe the flow to
    /// quiescence.
    public func waitForCompletion() async {
        await consumeTask?.value
    }

    // MARK: - Internals

    /// Applies one reported state. The single place the rendered state moves.
    private func apply(_ next: CompanionAuthState) {
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

    /// Ends an existing control-channel session before opening another one.
    /// This is also the QR-to-phone fallback: TDLib rotates QR links itself,
    /// but changing auth methods requires a fresh phone-capable session.
    private func endExistingSession() async {
        let existingSession = session
        let existingConsumption = consumeTask
        await existingSession?.cancel()
        // Keep the sole state consumer alive through the terminal event and
        // stream completion. In production that completion is delayed until
        // the FFI auth pump has released its single-sign-in ScopeGuard.
        await existingConsumption?.value
        consumeTask = nil
        session = nil
    }

    private func reset() {
        state = .idle
        stateHistory = []
        lastRejection = nil
        lastInvalidInput = nil
        unavailable = nil
    }
}
