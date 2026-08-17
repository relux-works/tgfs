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
  private let firstEventTimeout: Duration
  private let submitTimeout: Duration
  private let completionTimeout: Duration

  private let lock = NSLock()
  private var channel: ControlAuthChannel?
  private var nextSeq: UInt64 = 1
  private var waiters: [UInt64: CheckedContinuation<AuthSubmitResult, Never>] = [:]
  private var finishWaiters: [CheckedContinuation<Void, Never>] = []
  private var finished = false

  private let stateStream: AsyncStream<CompanionAuthState>
  private let stateContinuation: AsyncStream<CompanionAuthState>.Continuation

  public init(
    openChannel: @escaping ChannelOpener,
    firstEventTimeout: Duration = .seconds(15),
    submitTimeout: Duration = .seconds(90),
    completionTimeout: Duration = .seconds(15)
  ) {
    self.openChannel = openChannel
    self.firstEventTimeout = firstEventTimeout
    self.submitTimeout = submitTimeout
    self.completionTimeout = completionTimeout
    (stateStream, stateContinuation) = AsyncStream.makeStream(of: CompanionAuthState.self)
  }

  public var states: AsyncStream<CompanionAuthState> {
    stateStream
  }

  public func start() async -> AuthStartResult {
    let channel: ControlAuthChannel
    switch await Self.race(
      timeout: firstEventTimeout,
      operation: openChannel,
      onLateValue: { result in
        if case .opened(let channel) = result { channel.close() }
      }
    ) {
    case .timedOut:
      finish()
      return .unavailable(.timedOut)
    case .value(.unavailable(let reason)):
      finish()
      return .unavailable(reason)
    case .value(.opened(let opened)):
      channel = opened
    }
    guard adopt(channel) else {
      channel.close()
      return .unavailable(.dropped)
    }

    // The server's first line decides: a state (the session is live) or
    // a refusal. Keep EOF separate from the deadline: an already-closed
    // channel is actionable as a dropped connection, not a timeout.
    let firstEvent = await Self.race(
      timeout: firstEventTimeout,
      operation: {
        var iterator = channel.events.makeAsyncIterator()
        return await iterator.next()
      }
    )
    switch firstEvent {
    case .timedOut:
      channel.close()
      finish()
      return .unavailable(.timedOut)
    case .value(nil):
      channel.close()
      finish()
      return .unavailable(.dropped)
    case .value(.some(.authState(let state))):
      stateContinuation.yield(Self.companionState(state))
      // The first iterator is exhausted before this pump starts; the stream
      // remains single-consumer for all subsequent events.
      Task { [weak self] in
        var iterator = channel.events.makeAsyncIterator()
        while let event = await iterator.next() {
          self?.handle(event)
        }
        self?.finish()
      }
      return .started
    case .value(.some(.commandFailed(let failure))):
      channel.close()
      finish()
      return .unavailable(Self.authUpgradeUnavailable(for: failure))
    default:
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
        failSubmission(seq: seq, reason: .dropped)
        return
      }
      let timeout = submitTimeout
      Task { [weak self] in
        try? await Task.sleep(for: timeout)
        self?.failSubmission(seq: seq, reason: .timedOut)
      }
    }
  }

  public func cancel() async -> ControlChannelUnavailable? {
    let result = await Self.race(
      timeout: completionTimeout,
      operation: { [weak self] in
        guard let self else { return AuthSubmitResult.unavailable(.dropped) }
        let submission = await self.submit(.cancel)
        await self.waitUntilFinished()
        return submission
      }
    )
    switch result {
    case .timedOut:
      finish()
      return .timedOut
    case .value(.unavailable(let reason)):
      finish()
      return reason
    case .value:
      return nil
    }
  }

  // MARK: - Event routing

  /// Maps an auth-upgrade refusal onto the user-visible channel state. The
  /// v1 agent used `invalid-argument` for the process-wide sign-in slot, so
  /// retain that narrow fallback while newer agents send `busy` explicitly.
  static func authUpgradeUnavailable(
    for failure: ControlCommandFailure
  ) -> ControlChannelUnavailable {
    switch failure.category {
    case .busy, .invalidArgument:
      return .busy
    case .authRequired, .rateLimited, .sourceUnavailable, .storage, .integrity,
      .cancelled, .internalError, .notFound:
      return .dropped
    }
  }

  // NSLock is not async-safe to hold across suspension, so every locked
  // section lives in one of these synchronous helpers.

  private func adopt(_ channel: ControlAuthChannel) -> Bool {
    lock.lock()
    guard !finished else {
      lock.unlock()
      return false
    }
    self.channel = channel
    lock.unlock()
    return true
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

  private func waitUntilFinished() async {
    await withCheckedContinuation { continuation in
      lock.lock()
      if finished {
        lock.unlock()
        continuation.resume()
      } else {
        finishWaiters.append(continuation)
        lock.unlock()
      }
    }
  }

  private func handle(_ event: ControlEvent) {
    switch event {
    case .authState(let state):
      stateContinuation.yield(Self.companionState(state))
    case .authSubmitResult(let result):
      resolve(seq: result.seq, with: Self.submitResult(result))
    case .commandDone, .terminationCommitAccepted, .commandFailed, .status, .settings,
      .contentPolicyStatus, .retentionChanged, .archiveModeChanged,
      .retentionPurgeResumed:
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

  private func failSubmission(seq: UInt64, reason: ControlChannelUnavailable) {
    resolve(seq: seq, with: .unavailable(reason))
    finish()
  }

  /// Ends the session: the state stream finishes and every outstanding
  /// submit resolves as dropped. Idempotent.
  private func finish() {
    lock.lock()
    let wasFinished = finished
    finished = true
    let pending = waiters
    waiters = [:]
    let finishing = finishWaiters
    finishWaiters = []
    let channel = self.channel
    self.channel = nil
    lock.unlock()
    guard !wasFinished else { return }
    for (_, continuation) in pending {
      continuation.resume(returning: .unavailable(.dropped))
    }
    for continuation in finishing {
      continuation.resume()
    }
    stateContinuation.finish()
    channel?.close()
  }

  deinit {
    finish()
  }

  /// Returns a value only if the operation wins its deadline. The operation
  /// is deliberately detached: a non-cooperative agent must not keep the UI
  /// task suspended after the control path has failed closed.
  private static func race<Value: Sendable>(
    timeout: Duration,
    operation: @escaping @Sendable () async -> Value,
    onLateValue: @escaping @Sendable (Value) -> Void = { _ in }
  ) async -> DeadlineResult<Value> {
    let gate = DeadlineGate<Value>(onLateValue: onLateValue)
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

private enum DeadlineResult<Value: Sendable>: Sendable {
  case value(Value)
  case timedOut
}

private final class DeadlineGate<Value: Sendable>: @unchecked Sendable {
  private let lock = NSLock()
  private let onLateValue: @Sendable (Value) -> Void
  private var continuation: CheckedContinuation<DeadlineResult<Value>, Never>?
  private var resolved = false

  init(onLateValue: @escaping @Sendable (Value) -> Void) {
    self.onLateValue = onLateValue
  }

  func install(_ continuation: CheckedContinuation<DeadlineResult<Value>, Never>) {
    lock.lock()
    self.continuation = continuation
    lock.unlock()
  }

  func resolve(_ value: Value) {
    lock.lock()
    guard !resolved else {
      lock.unlock()
      onLateValue(value)
      return
    }
    resolved = true
    let continuation = self.continuation
    self.continuation = nil
    lock.unlock()
    continuation?.resume(returning: .value(value))
  }

  func timeout() {
    lock.lock()
    guard !resolved else {
      lock.unlock()
      return
    }
    resolved = true
    let continuation = self.continuation
    self.continuation = nil
    lock.unlock()
    continuation?.resume(returning: .timedOut)
  }
}
