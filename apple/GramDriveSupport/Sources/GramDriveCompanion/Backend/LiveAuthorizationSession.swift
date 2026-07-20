import Foundation
import GramDriveAgentCore

/// The live ``AuthorizationSession``: one control-channel auth connection,
/// its wire states mapped onto the shell's vocabulary (BUG-260720-3i74u1).
///
/// `start()` opens the channel (after the backend ensured the agent runs)
/// and consumes the first event to distinguish a live session from a
/// refusal; from then on a pump routes wire events — states to the state
/// stream, sequence-correlated submit results to their waiting callers.
/// The channel closing for any reason finishes the state stream and fails
/// outstanding submits as a dropped channel.
public final class LiveAuthorizationSession: AuthorizationSession, @unchecked Sendable {
    /// The opener's answer: a live channel, or the typed reason there is
    /// none (not `Result` — ``ControlChannelUnavailable`` is a state, not
    /// an `Error`).
    public enum ChannelOpen: Sendable {
        case opened(ControlAuthChannel)
        case unavailable(ControlChannelUnavailable)
    }

    /// How the backend produces the underlying connection: agent ensure +
    /// channel open, or a typed reason it cannot.
    public typealias ChannelOpener = @Sendable () async -> ChannelOpen

    private let openChannel: ChannelOpener
    private let submitTimeout: Duration

    private let lock = NSLock()
    private var channel: ControlAuthChannel?
    private var nextSeq: UInt64 = 1
    private var waiters: [UInt64: CheckedContinuation<AuthSubmitResult, Never>] = [:]
    private var finished = false

    private let stateStream: AsyncStream<CompanionAuthState>
    private let stateContinuation: AsyncStream<CompanionAuthState>.Continuation

    public init(
        openChannel: @escaping ChannelOpener,
        submitTimeout: Duration = .seconds(90)
    ) {
        self.openChannel = openChannel
        self.submitTimeout = submitTimeout
        (stateStream, stateContinuation) = AsyncStream.makeStream(of: CompanionAuthState.self)
    }

    public var states: AsyncStream<CompanionAuthState> {
        stateStream
    }

    public func start() async -> AuthStartResult {
        let channel: ControlAuthChannel
        switch await openChannel() {
        case .unavailable(let reason):
            finish()
            return .unavailable(reason)
        case .opened(let opened):
            channel = opened
        }
        adopt(channel)

        // The server's first line decides: a state (the session is live) or
        // a refusal.
        var iterator = channel.events.makeAsyncIterator()
        switch await iterator.next() {
        case .authState(let state):
            stateContinuation.yield(Self.companionState(state))
            // The iterator consumed the first event; the pump continues on
            // the same iterator (AsyncStream is single-consumer).
            Task { [weak self] in
                while let event = await iterator.next() {
                    self?.handle(event)
                }
                self?.finish()
            }
            return .started
        case .commandFailed, .none:
            channel.close()
            finish()
            return .unavailable(.dropped)
        case .some:
            channel.close()
            finish()
            return .unavailable(.dropped)
        }
    }

    public func submit(_ input: CompanionAuthInput) async -> AuthSubmitResult {
        guard let (channel, seq) = reserveSubmission() else {
            return .unavailable(.dropped)
        }
        let frame = ControlAuthInputFrame(seq: seq, input: Self.wireInput(input))
        return await withCheckedContinuation { continuation in
            register(continuation, for: seq)
            do {
                try channel.send(frame)
            } catch {
                resolve(seq: seq, with: .unavailable(.dropped))
                return
            }
            let timeout = submitTimeout
            Task { [weak self] in
                try? await Task.sleep(for: timeout)
                self?.resolve(seq: seq, with: .unavailable(.dropped))
            }
        }
    }

    // MARK: - Event routing

    // NSLock is not async-safe to hold across suspension, so every locked
    // section lives in one of these synchronous helpers.

    private func adopt(_ channel: ControlAuthChannel) {
        lock.lock()
        self.channel = channel
        lock.unlock()
    }

    private func reserveSubmission() -> (ControlAuthChannel, UInt64)? {
        lock.lock()
        defer { lock.unlock() }
        guard let channel, !finished else { return nil }
        let seq = nextSeq
        nextSeq += 1
        return (channel, seq)
    }

    private func register(
        _ continuation: CheckedContinuation<AuthSubmitResult, Never>, for seq: UInt64
    ) {
        lock.lock()
        waiters[seq] = continuation
        lock.unlock()
    }

    private func handle(_ event: ControlEvent) {
        switch event {
        case .authState(let state):
            stateContinuation.yield(Self.companionState(state))
        case .authSubmitResult(let result):
            resolve(seq: result.seq, with: Self.submitResult(result))
        case .commandDone, .commandFailed, .status, .settings:
            // Nothing legal arrives here mid-session; ignore rather than
            // wedge (fail-safe decode posture).
            break
        }
    }

    /// Resolves one waiting submit exactly once.
    private func resolve(seq: UInt64, with result: AuthSubmitResult) {
        lock.lock()
        let continuation = waiters.removeValue(forKey: seq)
        lock.unlock()
        continuation?.resume(returning: result)
    }

    /// Ends the session: the state stream finishes and every outstanding
    /// submit resolves as dropped. Idempotent.
    private func finish() {
        lock.lock()
        let wasFinished = finished
        finished = true
        let pending = waiters
        waiters = [:]
        let channel = self.channel
        self.channel = nil
        lock.unlock()
        guard !wasFinished else { return }
        for (_, continuation) in pending {
            continuation.resume(returning: .unavailable(.dropped))
        }
        stateContinuation.finish()
        channel?.close()
    }

    deinit {
        finish()
    }

    // MARK: - Vocabulary mapping

    static func wireInput(_ input: CompanionAuthInput) -> ControlAuthInput {
        switch input {
        case .submitPhoneNumber(let value): return .submitPhoneNumber(value)
        case .requestQrCode: return .requestQrCode
        case .submitCode(let value): return .submitCode(value)
        case .resendCode: return .resendCode
        case .submitPassword(let value): return .submitPassword(value)
        case .cancel: return .cancel
        }
    }

    static func companionState(_ state: ControlAuthState) -> CompanionAuthState {
        switch state.kind {
        case "starting":
            return .starting
        // Finalizing is the agent persisting the signed-in account — like
        // configuring, machinery the user waits out, not a user-facing step.
        case "configuring", "finalizing":
            return .configuring
        case "wait-phone-number":
            return .waitPhoneNumber
        case "wait-code":
            guard let info = state.codeInfo else {
                return .unsupported(kind: state.kind)
            }
            return .waitCode(
                CompanionCodeInfo(
                    phoneNumber: info.phoneNumber,
                    codeLength: info.codeLength,
                    resendTimeoutSeconds: info.resendTimeoutSeconds))
        case "wait-qr-confirmation":
            guard let link = state.qrLink else {
                return .unsupported(kind: state.kind)
            }
            return .waitQrConfirmation(link: link)
        case "wait-password":
            guard let info = state.passwordInfo else {
                return .unsupported(kind: state.kind)
            }
            return .waitPassword(
                CompanionPasswordInfo(
                    hint: info.hint, hasRecoveryEmail: info.hasRecoveryEmail))
        case "ready":
            return .ready
        case "logging-out":
            return .loggingOut
        case "closing":
            return .closing
        case "closed":
            return .closed
        case "unsupported":
            return .unsupported(kind: state.unsupportedKind ?? "unknown")
        case "failed":
            return .failed(detail: state.failureDetail ?? "unknown")
        default:
            // A newer agent's state this shell does not know: render the
            // honest fail-safe, exactly like the core's unknown-state rule.
            return .unsupported(kind: state.kind)
        }
    }

    static func submitResult(_ result: ControlAuthSubmitResult) -> AuthSubmitResult {
        switch result.outcome {
        case "accepted":
            return .accepted
        case "invalid-for-state":
            return .invalidForState
        case "rejected":
            guard let rejection = result.rejection else {
                return .rejected(.other(code: 0, message: "unclassified rejection"))
            }
            return .rejected(Self.companionRejection(rejection))
        default:
            return .rejected(.other(code: 0, message: "unknown outcome \(result.outcome)"))
        }
    }

    static func companionRejection(_ rejection: ControlAuthRejection) -> CompanionAuthRejection {
        switch rejection.kind {
        case "invalid-phone-number": return .invalidPhoneNumber
        case "phone-number-banned": return .phoneNumberBanned
        case "invalid-code": return .invalidCode
        case "expired-code": return .expiredCode
        case "invalid-password": return .invalidPassword
        case "rate-limited": return .rateLimited(retryAfterSeconds: rejection.retryAfterSeconds)
        case "network": return .network
        case "session-ended": return .sessionEnded
        case "other":
            return .other(code: rejection.code ?? 0, message: rejection.detail ?? "")
        default:
            return .other(code: rejection.code ?? 0, message: rejection.kind)
        }
    }
}
