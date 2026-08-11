import Darwin
import Dispatch
import Foundation
import GramDriveCore
import GramDriveSupport

/// One hosted sign-in session behind the control channel — the seam between
/// the server (wire concerns) and the engine's authorization flow (the FFI
/// `AuthSession` in production, a scripted session in tests).
///
/// `states` yields every flow state in order and finishes when the session
/// is over (complete, failed, closed, or the host tore it down). `submit`
/// answers every input — channel-level failures are reported as the
/// `session-ended` rejection, so the caller always gets a typed answer.
public protocol AgentAuthSessionHosting: Sendable {
  var states: AsyncStream<ControlAuthState> { get }
  func submit(_ input: ControlAuthInput) async -> AgentAuthSubmitAnswer
  func close()
}

/// The seam's answer to one input: the wire outcome minus the sequence
/// number, which the server stamps from the frame it is answering.
public struct AgentAuthSubmitAnswer: Equatable, Sendable {
  /// `accepted`, `rejected`, or `invalid-for-state`.
  public var outcome: String
  /// The classified rejection, present exactly for `rejected`.
  public var rejection: ControlAuthRejection?

  public init(outcome: String, rejection: ControlAuthRejection? = nil) {
    self.outcome = outcome
    self.rejection = rejection
  }

  public static let accepted = AgentAuthSubmitAnswer(outcome: "accepted")
  public static let invalidForState = AgentAuthSubmitAnswer(outcome: "invalid-for-state")
  public static func rejected(_ rejection: ControlAuthRejection) -> AgentAuthSubmitAnswer {
    AgentAuthSubmitAnswer(outcome: "rejected", rejection: rejection)
  }
}

/// Opens sign-in sessions for the control channel. Throwing refuses the
/// upgrade with a classified failure (`DriveError`s are mapped; anything
/// else is internal).
public protocol AgentAuthorizing: Sendable {
  func makeSession() throws -> any AgentAuthSessionHosting
}

/// Runs the engine half of the SEC-004 account removal.
public protocol AgentAccountRemoving: Sendable {
  func remove(_ request: ControlRemovalRequest) async -> ControlCommandOutcome
}

/// Runs the agent-side repair pass.
public protocol AgentRepairing: Sendable {
  func repair() async -> ControlCommandOutcome
}

/// Owns the engine's per-account retention and independent Archive policy.
public protocol AgentContentPolicyControlling: Sendable {
  func status(accountId: Int64) async throws -> ControlContentPolicyStatus
  func setRetention(
    accountId: Int64,
    target: ControlRetentionMode,
    typedConfirmation: String?
  ) async throws -> ControlRetentionTransition
  func setArchiveMode(
    accountId: Int64,
    enabled: Bool
  ) async throws -> ControlArchiveModeTransition
  func resumeRetentionPurge(accountId: Int64) async throws -> ControlRetentionPurgeResume
}

/// A command's outcome, as the seams report it.
public enum ControlCommandOutcome: Equatable, Sendable {
  case completed
  case failed(ControlCommandFailure)
}

/// What the control server serves with: the always-present lifecycle reads
/// (status, settings) and the engine-backed command seams. A `nil` seam
/// answers its command with a truthful `sourceUnavailable` — the shipped
/// agent wires all three (`AgentMain`), so that state is reachable only in
/// partial test assemblies.
public struct ControlServerHandlers: Sendable {
  public var status: @Sendable () -> AgentHealthSnapshot
  public var reloadSettings: @Sendable () throws -> AgentSettings
  public var authorizer: (any AgentAuthorizing)?
  /// Fixed auth outcome codes only. The control server owns the redaction
  /// boundary because it sees both raw user inputs and rich state payloads.
  public var authDiagnostics: (@Sendable (AuthDiagnosticCode) -> Void)?
  public var remover: (any AgentAccountRemoving)?
  public var repairer: (any AgentRepairing)?
  public var contentPolicy: (any AgentContentPolicyControlling)?
  public var historyPriority: (@Sendable (HistoryPriorityRequest) -> ControlCommandOutcome)?
  public var providerFetchHealth: (@Sendable (ProviderFetchHealthReport) -> ControlCommandOutcome)?
  /// Invoked only after the control server has acknowledged the request. The
  /// host owns the actual lifecycle drain and process exit.
  public var prepareForTermination: (@Sendable (ControlTerminationRequest) -> Void)?
  /// Performs the non-mutating half of prepare validation before the server
  /// writes its acknowledgement. The actual transition deliberately remains
  /// after the write, so a dropped acknowledgement cannot start a drain.
  public var canPrepareTermination: (@Sendable (ControlTerminationRequest) -> Bool)?
  /// Atomically claims a request-correlated prepared drain before the server
  /// emits its commit acceptance. A `false` result means cancellation or the
  /// lease won the race, so the companion must not permit termination.
  public var acceptTerminationCommit: (@Sendable (ControlTerminationRequest) -> Bool)?
  /// Starts the already accepted teardown only after the terminal commit
  /// result was written to the local control connection.
  public var finishAcceptedTerminationCommit: (@Sendable (ControlTerminationRequest) -> Void)?

  public init(
    status: @escaping @Sendable () -> AgentHealthSnapshot,
    reloadSettings: @escaping @Sendable () throws -> AgentSettings,
    authorizer: (any AgentAuthorizing)? = nil,
    authDiagnostics: (@Sendable (AuthDiagnosticCode) -> Void)? = nil,
    remover: (any AgentAccountRemoving)? = nil,
    repairer: (any AgentRepairing)? = nil,
    contentPolicy: (any AgentContentPolicyControlling)? = nil,
    historyPriority: (@Sendable (HistoryPriorityRequest) -> ControlCommandOutcome)? = nil,
    providerFetchHealth: (@Sendable (ProviderFetchHealthReport) -> ControlCommandOutcome)? = nil,
    prepareForTermination: (@Sendable (ControlTerminationRequest) -> Void)? = nil,
    canPrepareTermination: (@Sendable (ControlTerminationRequest) -> Bool)? = nil,
    acceptTerminationCommit: (@Sendable (ControlTerminationRequest) -> Bool)? = nil,
    finishAcceptedTerminationCommit: (@Sendable (ControlTerminationRequest) -> Void)? = nil
  ) {
    self.status = status
    self.reloadSettings = reloadSettings
    self.authorizer = authorizer
    self.authDiagnostics = authDiagnostics
    self.remover = remover
    self.repairer = repairer
    self.contentPolicy = contentPolicy
    self.historyPriority = historyPriority
    self.providerFetchHealth = providerFetchHealth
    self.prepareForTermination = prepareForTermination
    self.canPrepareTermination = canPrepareTermination
    self.acceptTerminationCommit = acceptTerminationCommit
    self.finishAcceptedTerminationCommit = finishAcceptedTerminationCommit
  }
}

/// Tunables of one control endpoint.
public struct ControlServerConfiguration: Sendable {
  /// Concurrent connection bound; beyond it new connections are refused
  /// rather than queued.
  public var maxConcurrentConnections: Int
  /// Cap on waiting for the request line of an accepted connection.
  public var requestTimeout: Duration
  /// Cap on a hosted agent answering one auth input. The client observes a
  /// closed channel on expiry rather than leaving a sign-in control wedged.
  public var authSubmissionTimeout: Duration

  public init(
    maxConcurrentConnections: Int = 8,
    requestTimeout: Duration = .seconds(5),
    authSubmissionTimeout: Duration = .seconds(90)
  ) {
    self.maxConcurrentConnections = maxConcurrentConnections
    self.requestTimeout = requestTimeout
    self.authSubmissionTimeout = authSubmissionTimeout
  }
}

/// A startup failure that occurs after the listener has bound but before its
/// dispatch source has become usable. `start` removes the listener on this
/// path, so callers never receive a socket whose source was cancelled before
/// registration.
enum ControlServerStartupError: Error, Equatable, Sendable {
  case listenerSourceCancelledBeforeRegistration
}

/// Test-only lifecycle markers. They deliberately live beside the real source
/// lifecycle instead of simulating libdispatch, so tests can hold the actual
/// registration callback while a real client is connected to the real socket.
enum ControlServerStartupEvent: Equatable, Sendable {
  case listenerRegistered
  case connectionAccepted
  case workStarted
  case statusResponseCompleted

  var diagnosticName: String {
    switch self {
    case .listenerRegistered:
      "registered"
    case .connectionAccepted:
      "accepted"
    case .workStarted:
      "work-started"
    case .statusResponseCompleted:
      "response-completed"
    }
  }
}

/// An internal observation seam used only by the control-channel regression
/// tests. Production callers use `ControlServer.start`, which installs no
/// observer and has no test-specific scheduling behavior.
struct ControlServerStartupObservation: Sendable {
  let record: @Sendable (ControlServerStartupEvent) -> Void
  let cancelBeforeResume: Bool

  init(
    record: @escaping @Sendable (ControlServerStartupEvent) -> Void,
    cancelBeforeResume: Bool = false
  ) {
    self.record = record
    self.cancelBeforeResume = cancelBeforeResume
  }
}

/// Resolves exactly once, so source cancellation cannot strand the synchronous
/// startup caller waiting for registration.
private final class ControlServerReadiness: @unchecked Sendable {
  private enum Result {
    case pending
    case registered
    case cancelled
  }

  private let lock = NSLock()
  private let signal = DispatchSemaphore(value: 0)
  private var result: Result = .pending

  func registered() {
    resolve(.registered)
  }

  func cancelled() {
    resolve(.cancelled)
  }

  func wait() throws {
    signal.wait()
    lock.lock()
    let result = self.result
    lock.unlock()
    guard case .registered = result else {
      throw ControlServerStartupError.listenerSourceCancelledBeforeRegistration
    }
  }

  private func resolve(_ result: Result) {
    lock.lock()
    guard case .pending = self.result else {
      lock.unlock()
      return
    }
    self.result = result
    lock.unlock()
    signal.signal()
  }
}

/// The agent's control endpoint: the serving side of ``ControlContract``
/// (BUG-260720-3i74u1).
///
/// Commands answer with one terminal event and close. An auth connection
/// upgrades: the request-line timeout is lifted (a sign-in legitimately
/// idles while the user types), a writer pumps the session's states out,
/// and a reader loop turns input frames into seam submissions with
/// sequence-correlated answers. Either side closing the connection ends
/// the session.
public final class ControlServer: @unchecked Sendable {
  private let lock = NSLock()
  private let acceptQueue: DispatchQueue
  private let workQueue: DispatchQueue
  private let authInputQueue: DispatchQueue
  private let socketPath: String
  private let handlers: ControlServerHandlers
  private let configuration: ControlServerConfiguration
  private let startupObservation: ControlServerStartupObservation?

  private var listener: Int32?
  private var acceptSource: (any DispatchSourceRead)?
  private var connections: [ObjectIdentifier: ControlConnection] = [:]

  /// Binds, listens, and starts serving before returning. A stale socket file
  /// (from a killed predecessor) is removed first — safe under the agent's
  /// single-instance lock.
  public static func start(
    socketURL: URL,
    handlers: ControlServerHandlers,
    configuration: ControlServerConfiguration = ControlServerConfiguration()
  ) throws -> ControlServer {
    try startImpl(
      socketURL: socketURL,
      handlers: handlers,
      configuration: configuration,
      startupObservation: nil)
  }

  /// Starts a real control server with a test-only lifecycle observer. The
  /// observer runs from the source registration callback, after kevent
  /// registration and before `start` is allowed to return.
  static func start(
    socketURL: URL,
    handlers: ControlServerHandlers,
    configuration: ControlServerConfiguration = ControlServerConfiguration(),
    startupObservation: ControlServerStartupObservation
  ) throws -> ControlServer {
    try startImpl(
      socketURL: socketURL,
      handlers: handlers,
      configuration: configuration,
      startupObservation: startupObservation)
  }

  private static func startImpl(
    socketURL: URL,
    handlers: ControlServerHandlers,
    configuration: ControlServerConfiguration,
    startupObservation: ControlServerStartupObservation?
  ) throws -> ControlServer {
    let path = socketURL.path
    let fd = socket(AF_UNIX, SOCK_STREAM, 0)
    guard fd >= 0 else {
      throw UnixSocketError.failed(operation: "socket", code: errno)
    }
    _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
    unlink(path)
    do {
      try UnixSocketAddress.bind(descriptor: fd, path: path)
    } catch {
      close(fd)
      throw error
    }
    guard listen(fd, 16) == 0 else {
      let code = errno
      close(fd)
      unlink(path)
      throw UnixSocketError.failed(operation: "listen", code: code)
    }
    let listenerFlags = fcntl(fd, F_GETFL)
    guard listenerFlags >= 0, fcntl(fd, F_SETFL, listenerFlags | O_NONBLOCK) == 0 else {
      let code = errno
      close(fd)
      unlink(path)
      throw UnixSocketError.failed(operation: "fcntl", code: code)
    }
    let server = ControlServer(
      listener: fd,
      socketPath: path,
      handlers: handlers,
      configuration: configuration,
      startupObservation: startupObservation)
    do {
      try server.startAccepting()
    } catch {
      server.stop()
      throw error
    }
    return server
  }

  private init(
    listener: Int32,
    socketPath: String,
    handlers: ControlServerHandlers,
    configuration: ControlServerConfiguration,
    startupObservation: ControlServerStartupObservation?
  ) {
    self.listener = listener
    self.socketPath = socketPath
    self.handlers = handlers
    self.configuration = configuration
    self.startupObservation = startupObservation
    self.acceptQueue = DispatchQueue(label: "com.reluxworks.gramdrive.agent.control")
    self.workQueue = DispatchQueue(
      label: "com.reluxworks.gramdrive.agent.control.work",
      // Requests accepted in one listener delivery execute in submission
      // order. This is protocol-significant for prepare/commit termination:
      // commit must observe the state recorded by the preceding prepare.
      qos: .userInitiated)
    self.authInputQueue = DispatchQueue(
      label: "com.reluxworks.gramdrive.agent.control.auth-input",
      qos: .userInitiated,
      attributes: .concurrent)
  }

  /// Connections currently being served.
  public var activeConnectionCount: Int {
    lock.lock()
    defer { lock.unlock() }
    return connections.count
  }

  /// Stops accepting, tears down the socket file, and ends every active
  /// connection (closing any hosted sign-in session). Idempotent; also
  /// runs on deallocation.
  public func stop() {
    lock.lock()
    acceptSource?.cancel()
    acceptSource = nil
    let listener = self.listener
    self.listener = nil
    let active = Array(connections.values)
    lock.unlock()
    if let listener {
      close(listener)
      unlink(socketPath)
    }
    for connection in active {
      connection.teardown()
    }
  }

  deinit {
    stop()
  }

  // MARK: - Accepting

  private func startAccepting() throws {
    lock.lock()
    guard let fd = listener else {
      lock.unlock()
      throw ControlServerStartupError.listenerSourceCancelledBeforeRegistration
    }
    let source = DispatchSource.makeReadSource(fileDescriptor: fd, queue: acceptQueue)
    let readiness = ControlServerReadiness()
    let observation = startupObservation
    source.setRegistrationHandler {
      // Dispatch guarantees this callback is submitted only after the read
      // source's kevent has registered with the system. That makes it the
      // lifecycle barrier; an empty queue hop cannot provide this guarantee.
      observation?.record(.listenerRegistered)
      readiness.registered()
    }
    source.setEventHandler { [weak self] in
      self?.acceptPendingConnections()
    }
    source.setCancelHandler {
      readiness.cancelled()
    }
    acceptSource = source
    lock.unlock()
    if observation?.cancelBeforeResume == true {
      source.cancel()
    }
    source.resume()
    try readiness.wait()
  }

  private func acceptPendingConnections() {
    lock.lock()
    let fd = listener
    lock.unlock()
    guard let fd else { return }
    var acceptedConnections: [ControlConnection] = []
    while true {
      let conn = accept(fd, nil, nil)
      if conn < 0 {
        if errno == EINTR { continue }
        // The listener is non-blocking so this is the normal end of one
        // read-source delivery. Draining it prevents queued clients from
        // waiting for an unrelated future readability notification.
        break
      }
      _ = fcntl(conn, F_SETFD, FD_CLOEXEC)
      let connectionFlags = fcntl(conn, F_GETFL)
      guard connectionFlags >= 0,
        fcntl(conn, F_SETFL, connectionFlags & ~O_NONBLOCK) == 0
      else {
        close(conn)
        continue
      }
      var noSigpipe: Int32 = 1
      _ = setsockopt(
        conn, SOL_SOCKET, SO_NOSIGPIPE,
        &noSigpipe, socklen_t(MemoryLayout<Int32>.size))
      var sendTimeout = timeval(tv_sec: 5, tv_usec: 0)
      _ = setsockopt(
        conn, SOL_SOCKET, SO_SNDTIMEO,
        &sendTimeout, socklen_t(MemoryLayout<timeval>.size))
      let receiveSeconds = max(1, Int(configuration.requestTimeout.components.seconds))
      var receiveTimeout = timeval(tv_sec: receiveSeconds, tv_usec: 0)
      _ = setsockopt(
        conn, SOL_SOCKET, SO_RCVTIMEO,
        &receiveTimeout, socklen_t(MemoryLayout<timeval>.size))

      let connection = ControlConnection(descriptor: conn)
      startupObservation?.record(.connectionAccepted)
      acceptedConnections.append(connection)
    }

    // Complete this accept batch before starting any request work. Besides
    // keeping the source queue non-blocking, this preserves kernel accept
    // order when several clients became readable in one coalesced delivery.
    for connection in acceptedConnections {
      // The accept queue must never block. The serial work queue preserves
      // request execution order for this accepted batch.
      workQueue.async { [weak self] in
        self?.serve(connection)
      }
    }
  }

  // MARK: - Per-connection lifecycle

  private func serve(_ connection: ControlConnection) {
    startupObservation?.record(.workStarted)
    guard admit(connection) else {
      connection.refuse(
        ControlCommandFailure(
          category: .sourceUnavailable,
          detail: "concurrent control connection bound reached"))
      return
    }

    let request: ControlRequest
    do {
      request = try connection.readLine(
        ControlRequest.self, cap: ControlContract.maxRequestLineBytes)
    } catch {
      connection.refuse(
        ControlCommandFailure(category: .invalidArgument, detail: "unreadable request"))
      remove(connection)
      return
    }

    guard request.protocolVersion == ControlContract.protocolVersion else {
      connection.refuse(
        ControlCommandFailure(
          category: .invalidArgument,
          detail: "protocol version mismatch: agent speaks "
            + "\(ControlContract.protocolVersion)"))
      remove(connection)
      return
    }

    switch request.operation {
    case .status:
      connection.writeEvent(.status(handlers.status()))
      connection.finish()
      startupObservation?.record(.statusResponseCompleted)
      remove(connection)
    case .reloadSettings:
      do {
        connection.writeEvent(.settings(try handlers.reloadSettings()))
        connection.finish()
      } catch {
        connection.refuse(
          ControlCommandFailure(
            category: .storage, detail: "settings could not be reloaded"))
      }
      remove(connection)
    case .repair:
      runCommand(on: connection, seam: handlers.repairer, name: "repair") { repairer in
        await repairer.repair()
      }
    case .removeAccount:
      guard let removal = request.removal else {
        connection.refuse(
          ControlCommandFailure(
            category: .invalidArgument, detail: "removal parameters missing"))
        remove(connection)
        return
      }
      runCommand(on: connection, seam: handlers.remover, name: "account removal") {
        remover in
        await remover.remove(removal)
      }
    case .authStart:
      serveAuth(on: connection)
    case .historyPriority:
      guard let priority = request.historyPriority else {
        connection.refuse(
          ControlCommandFailure(
            category: .invalidArgument,
            detail: "history priority parameters missing"))
        remove(connection)
        return
      }
      runCommand(
        on: connection, seam: handlers.historyPriority,
        name: "history priority"
      ) { handler in
        handler(priority)
      }
    case .providerFetchHealth:
      guard let report = request.providerFetchHealth else {
        connection.refuse(
          ControlCommandFailure(
            category: .invalidArgument, detail: "provider fetch health parameters missing"))
        remove(connection)
        return
      }
      runCommand(
        on: connection, seam: handlers.providerFetchHealth,
        name: "provider fetch health"
      ) { handler in
        handler(report)
      }
    case .contentPolicyStatus:
      guard let policy = request.contentPolicy else {
        refuseMissingPolicyParameters(on: connection)
        return
      }
      runPolicy(on: connection) { controller in
        .contentPolicyStatus(try await controller.status(accountId: policy.accountId))
      }
    case .setRetention:
      guard let policy = request.contentPolicy, let target = policy.retention else {
        refuseMissingPolicyParameters(on: connection)
        return
      }
      runPolicy(on: connection) { controller in
        .retentionChanged(
          try await controller.setRetention(
            accountId: policy.accountId,
            target: target,
            typedConfirmation: policy.typedConfirmation))
      }
    case .setArchiveMode:
      guard let policy = request.contentPolicy,
        let enabled = policy.archiveModeEnabled
      else {
        refuseMissingPolicyParameters(on: connection)
        return
      }
      runPolicy(on: connection) { controller in
        .archiveModeChanged(
          try await controller.setArchiveMode(
            accountId: policy.accountId,
            enabled: enabled))
      }
    case .resumeRetentionPurge:
      guard let policy = request.contentPolicy else {
        refuseMissingPolicyParameters(on: connection)
        return
      }
      runPolicy(on: connection) { controller in
        .retentionPurgeResumed(
          try await controller.resumeRetentionPurge(accountId: policy.accountId))
      }
    case .prepareForTermination:
      guard let termination = request.termination else {
        connection.refuse(
          ControlCommandFailure(
            category: .invalidArgument, detail: "termination parameters missing"))
        remove(connection)
        return
      }
      if termination.action == .commit {
        guard let accept = handlers.acceptTerminationCommit else {
          connection.refuse(
            ControlCommandFailure(
              category: .sourceUnavailable,
              detail: "termination commit is not hosted in this build"))
          remove(connection)
          return
        }
        guard accept(termination) else {
          connection.refuse(
            ControlCommandFailure(
              category: .invalidArgument,
              detail: "termination commit was not accepted"))
          remove(connection)
          return
        }
        // Once accepted, the lifecycle is irreversible. Emit that distinct
        // result before ending resources, then start teardown even if the
        // peer loses the result; the companion reconciles disappearance.
        _ = connection.writeEvent(.terminationCommitAccepted)
        connection.finish()
        remove(connection)
        handlers.finishAcceptedTerminationCommit?(termination)
        return
      }
      guard let handler = handlers.prepareForTermination else {
        connection.refuse(
          ControlCommandFailure(
            category: .sourceUnavailable,
            detail: "termination preparation is not hosted in this build"))
        remove(connection)
        return
      }
      guard handlers.canPrepareTermination?(termination) ?? true else {
        connection.refuse(
          ControlCommandFailure(
            category: .invalidArgument,
            detail: "termination prepare was not accepted"))
        remove(connection)
        return
      }
      // Never mutate lifecycle state unless the acknowledgement reached the
      // local socket. The handler records the request before this connection
      // closes, so a client that loses the response can reconcile health.
      guard connection.writeEvent(.commandDone) else {
        connection.finish()
        remove(connection)
        return
      }
      handler(termination)
      connection.finish()
      remove(connection)
    }
  }

  private func refuseMissingPolicyParameters(on connection: ControlConnection) {
    connection.refuse(
      ControlCommandFailure(
        category: .invalidArgument,
        detail: "content-policy parameters missing"))
    remove(connection)
  }

  /// Runs one typed policy operation. These responses carry the committed
  /// engine state rather than flattening a transition into a generic
  /// `done`, so a reconnect can reconcile an interrupted UI truthfully.
  private func runPolicy(
    on connection: ControlConnection,
    run: @escaping @Sendable (any AgentContentPolicyControlling) async throws -> ControlEvent
  ) {
    guard let controller = handlers.contentPolicy else {
      connection.refuse(
        ControlCommandFailure(
          category: .sourceUnavailable,
          detail: "content policy is not hosted in this build"))
      remove(connection)
      return
    }
    Task { [weak self] in
      do {
        connection.writeEvent(try await run(controller))
      } catch {
        connection.writeEvent(.commandFailed(Self.failure(from: error)))
      }
      connection.finish()
      self?.remove(connection)
    }
  }

  /// Runs one engine-backed command through its seam, answering with the
  /// terminal event. A missing seam is a truthful `sourceUnavailable`.
  private func runCommand<Seam: Sendable>(
    on connection: ControlConnection,
    seam: Seam?,
    name: String,
    run: @escaping @Sendable (Seam) async -> ControlCommandOutcome
  ) {
    guard let seam else {
      connection.refuse(
        ControlCommandFailure(
          category: .sourceUnavailable,
          detail: "\(name) is not hosted in this build"))
      remove(connection)
      return
    }
    Task { [weak self] in
      switch await run(seam) {
      case .completed:
        connection.writeEvent(.commandDone)
      case .failed(let failure):
        connection.writeEvent(.commandFailed(failure))
      }
      connection.finish()
      self?.remove(connection)
    }
  }

  /// Upgrades the connection to a sign-in session: unbounded reads (the
  /// user types at human speed), a state-pumping writer, and the input
  /// reader loop.
  private func serveAuth(on connection: ControlConnection) {
    guard let authorizer = handlers.authorizer else {
      connection.refuse(
        ControlCommandFailure(
          category: .sourceUnavailable,
          detail: "sign-in is not hosted in this build"))
      remove(connection)
      return
    }
    let session: any AgentAuthSessionHosting
    do {
      session = try authorizer.makeSession()
    } catch {
      connection.refuse(Self.failure(from: error))
      remove(connection)
      return
    }
    handlers.authDiagnostics?(.sessionStarted)
    connection.adopt(session: session)

    // A sign-in idles while the user reads and types; only the
    // handshake had a deadline.
    var unbounded = timeval(tv_sec: 0, tv_usec: 0)
    _ = setsockopt(
      connection.descriptor, SOL_SOCKET, SO_RCVTIMEO,
      &unbounded, socklen_t(MemoryLayout<timeval>.size))

    // Writer: the session's states, in order; the stream finishing is
    // the session being over, which ends the connection.
    Task { [weak self] in
      for await state in session.states {
        if let code = AuthDiagnosticCode.finalization(for: state) {
          self?.handlers.authDiagnostics?(code)
        }
        connection.writeEvent(.authState(state))
      }
      connection.teardown()
      self?.remove(connection)
    }

    // Reader: one input frame per line until EOF or protocol breach.
    authInputQueue.async { [weak self] in
      while true {
        let frame: ControlAuthInputFrame
        do {
          frame = try connection.readLine(
            ControlAuthInputFrame.self,
            cap: ControlContract.maxRequestLineBytes)
        } catch {
          connection.teardown()
          self?.remove(connection)
          return
        }
        Task.detached { [weak self, session, frame, connection] in
          guard let answer = await Self.answerAuthInput(
            session: session,
            input: frame.input,
            timeout: self?.configuration.authSubmissionTimeout ?? .seconds(90))
          else {
            connection.teardown()
            self?.remove(connection)
            return
          }
          if let rejection = answer.rejection {
            self?.handlers.authDiagnostics?(AuthDiagnosticCode.refusal(for: rejection))
          }
          connection.writeEvent(
            .authSubmitResult(
              ControlAuthSubmitResult(
                seq: frame.seq,
                outcome: answer.outcome,
                rejection: answer.rejection)))
        }
      }
    }
  }

  /// A hosted engine call is allowed to outlive this task, but never the
  /// control connection. This keeps a wedged namespace/auth pump from
  /// pinning a submit control or the server's active-connection slot.
  private static func answerAuthInput(
    session: any AgentAuthSessionHosting,
    input: ControlAuthInput,
    timeout: Duration
  ) async -> AgentAuthSubmitAnswer? {
    let gate = AuthSubmissionDeadlineGate<AgentAuthSubmitAnswer?>(timedOut: { nil })
    return await withCheckedContinuation { continuation in
      gate.install(continuation)
      Task.detached { [gate, session, input] in
        gate.resolve(await session.submit(input))
      }
      Task.detached { [gate, timeout] in
        try? await Task.sleep(for: timeout)
        gate.timeout()
      }
    }
  }

  /// Admits the connection into the bounded active set.
  private func admit(_ connection: ControlConnection) -> Bool {
    lock.lock()
    defer { lock.unlock() }
    guard connections.count < configuration.maxConcurrentConnections else {
      return false
    }
    connections[ObjectIdentifier(connection)] = connection
    return true
  }

  private func remove(_ connection: ControlConnection) {
    lock.lock()
    connections.removeValue(forKey: ObjectIdentifier(connection))
    lock.unlock()
  }

  /// Maps a seam error onto the wire categories (NFR-030 alignment);
  /// anything that is not a classified `DriveError` is internal.
  static func failure(from error: Error) -> ControlCommandFailure {
    guard let driveError = error as? DriveError else {
      return ControlCommandFailure(category: .internalError, detail: "internal failure")
    }
    switch driveError {
    case .InvalidArgument:
      return ControlCommandFailure(category: .invalidArgument, detail: "invalid argument")
    case .NotFound:
      return ControlCommandFailure(category: .notFound, detail: "not found")
    case .AuthRequired:
      return ControlCommandFailure(
        category: .authRequired, detail: "authorization required")
    case .RateLimited(_, let retryAfterMs):
      return ControlCommandFailure(
        category: .rateLimited, detail: "rate limited", retryAfterMs: retryAfterMs)
    case .SourceUnavailable:
      return ControlCommandFailure(
        category: .sourceUnavailable, detail: "source unavailable")
    case .Storage:
      return ControlCommandFailure(category: .storage, detail: "storage failure")
    case .Integrity:
      return ControlCommandFailure(category: .integrity, detail: "integrity failure")
    case .Restricted, .VersionConflict:
      return ControlCommandFailure(category: .internalError, detail: "internal failure")
    case .Cancelled:
      return ControlCommandFailure(category: .cancelled, detail: "cancelled")
    case .Internal:
      return ControlCommandFailure(category: .internalError, detail: "internal failure")
    }
  }
}

private final class AuthSubmissionDeadlineGate<Value: Sendable>: @unchecked Sendable {
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

/// One accepted control connection: the descriptor, the serialized writer,
/// the buffered line reader, and (for auth) the hosted session.
private final class ControlConnection: @unchecked Sendable {
  let descriptor: Int32

  private let lock = NSLock()
  private var closed = false
  private var readBuffer = Data()
  private var session: (any AgentAuthSessionHosting)?

  init(descriptor: Int32) {
    self.descriptor = descriptor
  }

  func adopt(session: any AgentAuthSessionHosting) {
    lock.lock()
    defer { lock.unlock() }
    self.session = session
  }

  /// Reads one `\n`-terminated line (leftovers persist for the next
  /// call), under `cap` and the socket's receive timeout.
  func readLine<T: Decodable>(_ type: T.Type, cap: Int) throws -> T {
    var chunk = [UInt8](repeating: 0, count: 4 * 1024)
    while true {
      if let lineEnd = readBuffer.firstIndex(of: 0x0A) {
        let line = readBuffer.subdata(in: readBuffer.startIndex..<lineEnd)
        readBuffer.removeSubrange(readBuffer.startIndex...lineEnd)
        return try HydrationWire.decodeLine(type, from: line)
      }
      guard readBuffer.count <= cap else {
        throw UnixSocketError.failed(operation: "read", code: EMSGSIZE)
      }
      let count = read(descriptor, &chunk, chunk.count)
      guard count > 0 else {
        throw UnixSocketError.failed(operation: "read", code: count == 0 ? 0 : errno)
      }
      readBuffer.append(contentsOf: chunk[0..<count])
    }
  }

  /// Writes one event line. The Boolean is significant for lifecycle
  /// commands: a failed acknowledgement must not start an irreversible drain.
  @discardableResult
  func writeEvent(_ event: ControlEvent) -> Bool {
    guard let data = try? HydrationWire.encodeLine(event) else { return false }
    lock.lock()
    defer { lock.unlock() }
    guard !closed else { return false }
    return data.withUnsafeBytes { (bytes: UnsafeRawBufferPointer) -> Bool in
      var offset = 0
      while offset < bytes.count {
        let written = write(descriptor, bytes.baseAddress! + offset, bytes.count - offset)
        guard written > 0 else { return false }
        offset += written
      }
      return true
    }
  }

  /// Terminal refusal for connections that never reached a seam.
  func refuse(_ failure: ControlCommandFailure) {
    writeEvent(.commandFailed(failure))
    finish()
  }

  /// Ends the connection and any hosted session; the entry point both
  /// the reader (EOF) and the writer (session over) converge on.
  func teardown() {
    lock.lock()
    let hosted = session
    session = nil
    lock.unlock()
    hosted?.close()
    finish()
  }

  /// Closes exactly once.
  func finish() {
    lock.lock()
    let wasClosed = closed
    closed = true
    lock.unlock()
    if !wasClosed {
      close(descriptor)
    }
  }

  deinit {
    teardown()
  }
}
