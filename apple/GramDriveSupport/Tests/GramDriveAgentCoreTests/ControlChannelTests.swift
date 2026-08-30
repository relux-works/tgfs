import Darwin
import Foundation
import GramDriveCore
import GramDriveSupport
import Testing

@testable import GramDriveAgentCore

private final class LockedHistoryPriorities: @unchecked Sendable {
  private let lock = NSLock()
  private var requests: [HistoryPriorityRequest] = []

  func append(_ request: HistoryPriorityRequest) {
    lock.lock()
    requests.append(request)
    lock.unlock()
  }

  func snapshot() -> [HistoryPriorityRequest] {
    lock.lock()
    defer { lock.unlock() }
    return requests
  }
}

private final class LockedProviderFetchHealthReports: @unchecked Sendable {
  private let lock = NSLock()
  private var reports: [ProviderFetchHealthReport] = []

  func append(_ report: ProviderFetchHealthReport) {
    lock.lock()
    reports.append(report)
    lock.unlock()
  }

  func snapshot() -> [ProviderFetchHealthReport] {
    lock.lock()
    defer { lock.unlock() }
    return reports
  }
}

private final class LockedTerminationRequests: @unchecked Sendable {
  private let lock = NSLock()
  private var requests: [ControlTerminationRequest] = []

  func append(_ request: ControlTerminationRequest) {
    lock.lock()
    requests.append(request)
    lock.unlock()
  }

  var snapshot: [ControlTerminationRequest] {
    lock.lock()
    defer { lock.unlock() }
    return requests
  }

}

private final class LockedPreparedTermination: @unchecked Sendable {
  private let lock = NSLock()
  private var prepared = false
  private var requests: [ControlTerminationRequest] = []

  func recordPrepare(_ request: ControlTerminationRequest) {
    lock.lock()
    prepared = true
    requests.append(request)
    lock.unlock()
  }

  func acceptCommit(_ request: ControlTerminationRequest) -> Bool {
    lock.lock()
    defer { lock.unlock() }
    guard prepared else { return false }
    requests.append(request)
    return true
  }

  var snapshot: [ControlTerminationRequest] {
    lock.lock()
    defer { lock.unlock() }
    return requests
  }

  var recordedPrepareThenCommit: Bool {
    lock.lock()
    defer { lock.unlock() }
    return requests.map(\.action) == [.prepare, .commit]
  }
}

private final class LockedControlServerEvents: @unchecked Sendable {
  private let lock = NSLock()
  private var storage: [ControlServerStartupEvent] = []

  func append(_ event: ControlServerStartupEvent) {
    lock.lock()
    storage.append(event)
    lock.unlock()
  }

  var snapshot: [ControlServerStartupEvent] {
    lock.lock()
    defer { lock.unlock() }
    return storage
  }

  var diagnosticSummary: String {
    snapshot.map(\.diagnosticName).joined(separator: " -> ")
  }
}

private final class LockedFlag: @unchecked Sendable {
  private let lock = NSLock()
  private var storage = false

  func set() {
    lock.lock()
    storage = true
    lock.unlock()
  }

  var value: Bool {
    lock.lock()
    defer { lock.unlock() }
    return storage
  }
}

private final class LockedCount: @unchecked Sendable {
  private let lock = NSLock()
  private var storage = 0

  func increment() {
    lock.lock()
    storage += 1
    lock.unlock()
  }

  var value: Int {
    lock.lock()
    defer { lock.unlock() }
    return storage
  }
}

private struct RegistrationHoldRequestResult: Sendable {
  let event: ControlEvent
  let startupReturnedBeforeRelease: Bool
  let stagesBeforeRelease: [ControlServerStartupEvent]
}

/// A result bridge for a blocking job owned by a test scheduler.
private final class BlockingOperationResult<Value: Sendable>: @unchecked Sendable {
  private let lock = NSLock()
  private var result: Result<Value, any Error>?
  private var continuation: CheckedContinuation<Value, any Error>?

  func resolve(_ result: Result<Value, any Error>) {
    lock.lock()
    guard self.result == nil else {
      lock.unlock()
      return
    }
    self.result = result
    let continuation = self.continuation
    self.continuation = nil
    lock.unlock()
    continuation?.resume(with: result)
  }

  func value() async throws -> Value {
    try await withCheckedThrowingContinuation { continuation in
      lock.lock()
      if let result {
        lock.unlock()
        continuation.resume(with: result)
      } else {
        self.continuation = continuation
        lock.unlock()
      }
    }
  }
}

/// The control channel end to end: the real server and the real client
/// over a substitute socket, with scripted seams playing the engine
/// (BUG-260720-3i74u1).
// These tests each run a blocking local IPC client and a server backed by
// libdispatch. Serializing this suite avoids starving the server queue with
// mutually waiting client/server pairs from concurrently running suites.
@Suite(.serialized) struct ControlChannelTests {
  // MARK: - Fixtures

  /// Blocking UNIX-socket clients must not consume Swift Testing's cooperative
  /// executor. This dedicated libdispatch queue remains available while test
  /// tasks are suspended awaiting their responses.
  private static let blockingClientQueue = DispatchQueue(
    label: "com.reluxworks.gramdrive.tests.control-client",
    qos: .userInitiated,
    attributes: .concurrent)

  /// Schedules a real blocking operation directly on libdispatch. Awaiting the
  /// result does not need a Swift cooperative executor worker.
  private static func dispatchBlocking<Value: Sendable>(
    _ operation: @escaping @Sendable () throws -> Value
  ) -> BlockingOperationResult<Value> {
    let result = BlockingOperationResult<Value>()
    blockingClientQueue.async {
      result.resolve(Result { try operation() })
    }
    return result
  }

  /// A controlled two-worker scheduler for the historical coordination shape.
  /// It models the old parent-resumption dependency without pretending that a
  /// semaphore changes Swift's global cooperative executor policy.
  private static func schedulerBlocking<Value: Sendable>(
    _ scheduler: OperationQueue,
    operation: @escaping @Sendable () throws -> Value
  ) -> BlockingOperationResult<Value> {
    let result = BlockingOperationResult<Value>()
    scheduler.addOperation {
      result.resolve(Result { try operation() })
    }
    return result
  }

  /// A per-test socket home under the system temp dir.
  private static func tempRoot() throws -> URL {
    let url = FileManager.default.temporaryDirectory
      .appendingPathComponent("gramdrive-control-\(UUID().uuidString.prefix(8))")
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
  }

  private static func snapshot(accounts: [AccountHealthSummary]? = nil) -> AgentHealthSnapshot {
    AgentHealthSnapshot(
      payloadVersion: 1,
      agentVersion: AgentVersion.current,
      contractVersion: "0.6.0",
      pid: 42,
      state: .running,
      startedAtMs: 1_000,
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
      recentEvents: ["started"],
      accounts: accounts)
  }

  private static func handlers(
    authorizer: (any AgentAuthorizing)? = nil,
    authDiagnostics: (@Sendable (AuthDiagnosticCode) -> Void)? = nil,
    remover: (any AgentAccountRemoving)? = nil,
    repairer: (any AgentRepairing)? = nil,
    contentPolicy: (any AgentContentPolicyControlling)? = nil,
    historyPriority:
      (@Sendable (HistoryPriorityRequest) -> ControlCommandOutcome)? = nil,
    providerFetchHealth:
      (@Sendable (ProviderFetchHealthReport) -> ControlCommandOutcome)? = nil,
    accounts: [AccountHealthSummary]? = nil
  ) -> ControlServerHandlers {
    ControlServerHandlers(
      status: { snapshot(accounts: accounts) },
      reloadSettings: { AgentSettings(launchAtLogin: true, cacheQuotaBytes: 7) },
      authorizer: authorizer,
      authDiagnostics: authDiagnostics,
      remover: remover,
      repairer: repairer,
      contentPolicy: contentPolicy,
      historyPriority: historyPriority,
      providerFetchHealth: providerFetchHealth)
  }

  /// The real command client deliberately uses blocking socket I/O. Keep it
  /// off Swift Testing's cooperative executor so a concurrently running suite
  /// cannot starve the server queue it is waiting on.
  private static func command(
    _ request: ControlRequest,
    socketURL: URL,
    didWriteRequest: @escaping @Sendable () -> Void = {},
    timeout: Duration = .seconds(5)
  ) async throws -> ControlEvent {
    try await withCheckedThrowingContinuation { continuation in
      blockingClientQueue.async {
        do {
          let descriptor = try ControlClient.connect(socketURL: socketURL, receiveTimeout: timeout)
          defer { close(descriptor) }
          try ControlClient.writeLine(request, to: descriptor, path: socketURL.path)
          didWriteRequest()
          var buffer = Data()
          continuation.resume(
            returning: try ControlClient.readEvent(
              from: descriptor, path: socketURL.path, buffer: &buffer))
        } catch {
          continuation.resume(throwing: error)
        }
      }
    }
  }

  /// Sends prepare and commit in one dedicated blocking-client operation,
  /// deliberately writing both before reading either response. Holding source
  /// registration lets the test establish their real socket arrival order
  /// without waiting on the prepare acknowledgement.
  private static func pairedTerminationCommands(
    prepare: ControlRequest,
    commit: ControlRequest,
    socketURL: URL,
    didWritePrepare: @escaping @Sendable () -> Void = {},
    didWriteCommit: @escaping @Sendable () -> Void = {}
  ) async throws -> (ControlEvent, ControlEvent) {
    try await withCheckedThrowingContinuation { continuation in
      blockingClientQueue.async {
        do {
          let prepareDescriptor = try ControlClient.connect(
            socketURL: socketURL, receiveTimeout: .seconds(5))
          defer { close(prepareDescriptor) }
          try ControlClient.writeLine(prepare, to: prepareDescriptor, path: socketURL.path)
          didWritePrepare()

          let commitDescriptor = try ControlClient.connect(
            socketURL: socketURL, receiveTimeout: .seconds(5))
          defer { close(commitDescriptor) }
          try ControlClient.writeLine(commit, to: commitDescriptor, path: socketURL.path)
          didWriteCommit()

          var prepareBuffer = Data()
          let prepareEvent = try ControlClient.readEvent(
            from: prepareDescriptor, path: socketURL.path, buffer: &prepareBuffer)
          var commitBuffer = Data()
          let commitEvent = try ControlClient.readEvent(
            from: commitDescriptor, path: socketURL.path, buffer: &commitBuffer)
          continuation.resume(returning: (prepareEvent, commitEvent))
        } catch {
          continuation.resume(throwing: error)
        }
      }
    }
  }

  private static func waitUntil(
    _ description: String,
    within bound: Duration = .seconds(5),
    condition: @escaping @Sendable () -> Bool,
    sourceLocation: Testing.SourceLocation = #_sourceLocation
  ) async {
    let deadline = ContinuousClock.now + bound
    while ContinuousClock.now < deadline {
      if condition() { return }
      try? await Task.sleep(for: .milliseconds(10))
    }
    Issue.record("timed out waiting for \(description)", sourceLocation: sourceLocation)
  }

  // (bounded event consumption lives in `EventCollector` below)

  // MARK: - Commands

  @Test func statusAnswersTheLifecycleSnapshot() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let server = try ControlServer.start(socketURL: socket, handlers: Self.handlers())
    defer { server.stop() }

    let event = try await Self.command(ControlRequest(operation: .status), socketURL: socket)
    #expect(event == .status(Self.snapshot()))
  }

  @Test func statusIsAvailableImmediatelyAcrossLoadedServerStarts() async throws {
    let endpointCount = 2
    let requestsPerEndpoint = 2
    try await withThrowingTaskGroup(of: Void.self) { group in
      for _ in 0..<endpointCount {
        group.addTask {
          let root = try Self.tempRoot()
          let socket = ControlContract.socketURL(dataRoot: root)
          try FileManager.default.createDirectory(
            at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
          let events = LockedControlServerEvents()
          let server = try ControlServer.start(
            socketURL: socket,
            handlers: Self.handlers(),
            startupObservation: ControlServerStartupObservation(record: events.append))
          defer { server.stop() }

          do {
            try await withThrowingTaskGroup(of: ControlEvent.self) { requests in
              for _ in 0..<requestsPerEndpoint {
                requests.addTask {
                  try await Self.command(
                    ControlRequest(operation: .status), socketURL: socket)
                }
              }
              for try await event in requests {
                #expect(event == .status(Self.snapshot()))
              }
            }
          } catch {
            Issue.record(
              "loaded status request failed after stages: \(events.diagnosticSummary)")
            throw error
          }
          let stages = events.snapshot
          #expect(stages.first == .listenerRegistered)
          #expect(stages.filter { $0 == .connectionAccepted }.count == requestsPerEndpoint)
          #expect(stages.filter { $0 == .workStarted }.count == requestsPerEndpoint)
          #expect(stages.filter { $0 == .statusResponseCompleted }.count == requestsPerEndpoint)
        }
      }
      try await group.waitForAll()
    }
  }

  @Test func coalescedAcceptsDrainInOrderBeforeStatusWorkStarts() async throws {
    let requestCount = 4
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let releaseRegistration = DispatchSemaphore(value: 0)
    let registrationEntered = LockedFlag()
    let writtenRequests = LockedCount()
    let events = LockedControlServerEvents()

    let startup = Self.dispatchBlocking {
      try ControlServer.start(
        socketURL: socket,
        handlers: Self.handlers(),
        startupObservation: ControlServerStartupObservation { event in
          events.append(event)
          guard event == .listenerRegistered else { return }
          registrationEntered.set()
          releaseRegistration.wait()
        })
    }
    await Self.waitUntil("listener registration", condition: { registrationEntered.value })

    let requests = Task {
      try await withThrowingTaskGroup(of: ControlEvent.self) { group in
        for _ in 0..<requestCount {
          group.addTask {
            try await Self.command(
              ControlRequest(operation: .status),
              socketURL: socket,
              didWriteRequest: { writtenRequests.increment() })
          }
        }
        var responses: [ControlEvent] = []
        for try await response in group {
          responses.append(response)
        }
        return responses
      }
    }
    await Self.waitUntil(
      "coalesced status writes",
      condition: {
        writtenRequests.value == requestCount
      })
    #expect(events.snapshot == [.listenerRegistered])

    releaseRegistration.signal()
    let server = try await startup.value()
    defer { server.stop() }
    do {
      for response in try await requests.value {
        #expect(response == .status(Self.snapshot()))
      }
    } catch {
      Issue.record(
        "coalesced status request failed after stages: \(events.diagnosticSummary)")
      throw error
    }
    await Self.waitUntil(
      "coalesced status responses",
      condition: {
        events.snapshot.filter { $0 == .statusResponseCompleted }.count == requestCount
      })

    let stages = events.snapshot
    let firstWork = try #require(stages.firstIndex(of: .workStarted))
    #expect(stages.first == .listenerRegistered)
    #expect(stages.filter { $0 == .connectionAccepted }.count == requestCount)
    #expect(stages.filter { $0 == .workStarted }.count == requestCount)
    #expect(stages.filter { $0 == .statusResponseCompleted }.count == requestCount)
    #expect(stages[1..<firstWork].allSatisfy { $0 == .connectionAccepted })
  }

  @Test func startWaitsForRealListenerRegistrationBeforePublishingTheSocket() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let releaseRegistration = DispatchSemaphore(value: 0)
    let registrationEntered = DispatchSemaphore(value: 0)
    let startupReturned = LockedFlag()
    let events = LockedControlServerEvents()

    let startup = Self.dispatchBlocking {
      defer { startupReturned.set() }
      return try ControlServer.start(
        socketURL: socket,
        handlers: Self.handlers(),
        startupObservation: ControlServerStartupObservation { event in
          events.append(event)
          guard event == .listenerRegistered else { return }
          registrationEntered.signal()
          releaseRegistration.wait()
        })
    }
    // The socket was bound before the source registration callback. Connect
    // and write while that callback is held. The client captures pre-release
    // state and releases registration itself, so its unchanged two-second read
    // deadline is independent of cooperative parent-task resumption.
    let request = Self.dispatchBlocking {
      registrationEntered.wait()
      let descriptor = try ControlClient.connect(socketURL: socket, receiveTimeout: .seconds(2))
      defer { close(descriptor) }
      try ControlClient.writeLine(
        ControlRequest(operation: .status), to: descriptor, path: socket.path)
      let startupReturnedBeforeRelease = startupReturned.value
      let stagesBeforeRelease = events.snapshot
      releaseRegistration.signal()
      var buffer = Data()
      return RegistrationHoldRequestResult(
        event: try ControlClient.readEvent(from: descriptor, path: socket.path, buffer: &buffer),
        startupReturnedBeforeRelease: startupReturnedBeforeRelease,
        stagesBeforeRelease: stagesBeforeRelease)
    }
    let requestResult = try await request.value()
    let server = try await startup.value()
    defer { server.stop() }
    #expect(requestResult.startupReturnedBeforeRelease == false)
    #expect(requestResult.stagesBeforeRelease == [.listenerRegistered])
    #expect(requestResult.event == .status(Self.snapshot()))
    #expect(
      events.snapshot == [
        .listenerRegistered,
        .connectionAccepted,
        .workStarted,
        .statusResponseCompleted,
      ])
  }

  /// The historical shape delegated startup, client I/O, and the parent
  /// release to one cooperative scheduler. With two workers, the real
  /// registration callback and the real response read occupy both workers, so
  /// the queued parent-resumption release cannot run. This is deliberately
  /// expected-red evidence. The normal test above uses the same socket,
  /// registration hold, and two-second client deadline, but releases from the
  /// dedicated Dispatch client before reading the response.
  @Test func controlledParentResumptionRegistrationHoldExpectedRedProof() async throws {
    guard
      ProcessInfo.processInfo.environment[
        "GRAMDRIVE_EXPECTED_RED_PARENT_RESUMPTION_REGISTRATION_HOLD"
      ] == "1"
    else { return }

    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let scheduler = OperationQueue()
    scheduler.name = "com.reluxworks.gramdrive.tests.cooperative-parent-resumption"
    scheduler.maxConcurrentOperationCount = 2
    let releaseRegistration = DispatchSemaphore(value: 0)
    let registrationEntered = LockedFlag()
    let requestWritten = LockedFlag()
    let parentReleaseStarted = LockedFlag()

    let startup = Self.schedulerBlocking(scheduler) {
      try ControlServer.start(
        socketURL: socket,
        handlers: Self.handlers(),
        startupObservation: ControlServerStartupObservation { event in
          guard event == .listenerRegistered else { return }
          registrationEntered.set()
          releaseRegistration.wait()
        })
    }
    await Self.waitUntil(
      "expected-red listener registration",
      condition: { registrationEntered.value })

    let request = Self.schedulerBlocking(scheduler) {
      let descriptor = try ControlClient.connect(
        socketURL: socket,
        receiveTimeout: .seconds(2))
      defer { close(descriptor) }
      try ControlClient.writeLine(
        ControlRequest(operation: .status), to: descriptor, path: socket.path)
      requestWritten.set()
      var buffer = Data()
      return try ControlClient.readEvent(from: descriptor, path: socket.path, buffer: &buffer)
    }
    await Self.waitUntil(
      "expected-red status request write",
      condition: { requestWritten.value })

    let parentRelease = Self.schedulerBlocking(scheduler) {
      parentReleaseStarted.set()
      releaseRegistration.signal()
    }

    // Expected red: under the controlled scheduler, the historical parent
    // release is queued behind the two blocking operations. This is a real
    // scheduler-capacity assertion, not a semaphore lease tautology.
    #expect(parentReleaseStarted.value)

    // Direct cleanup makes every real operation join after the expected-red
    // assertion; no listener or scheduled operation is left behind.
    releaseRegistration.signal()
    let server = try await startup.value()
    defer { server.stop() }
    #expect(try await request.value() == .status(Self.snapshot()))
    _ = try await parentRelease.value()
  }

  @Test func startRemovesTheSocketWhenListenerRegistrationIsCancelled() throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)

    #expect(throws: ControlServerStartupError.listenerSourceCancelledBeforeRegistration) {
      try ControlServer.start(
        socketURL: socket,
        handlers: Self.handlers(),
        startupObservation: ControlServerStartupObservation(
          record: { _ in },
          cancelBeforeResume: true))
    }
    #expect(FileManager.default.fileExists(atPath: socket.path) == false)
  }

  @Test func reloadSettingsAnswersTheAppliedDocument() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let server = try ControlServer.start(socketURL: socket, handlers: Self.handlers())
    defer { server.stop() }

    let event = try await Self.command(
      ControlRequest(operation: .reloadSettings), socketURL: socket)
    #expect(event == .settings(AgentSettings(launchAtLogin: true, cacheQuotaBytes: 7)))
  }

  @Test func terminationAcknowledgesBeforeStartingTheHostDrain() throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let recorded = LockedTerminationRequests()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers())
    // Use a second server with the host callback to pin the acknowledgement
    // contract through the real UNIX socket.
    server.stop()
    let liveServer = try ControlServer.start(
      socketURL: socket,
      handlers: ControlServerHandlers(
        status: { Self.snapshot() },
        reloadSettings: { AgentSettings() },
        prepareForTermination: { recorded.append($0) },
        acceptTerminationCommit: {
          recorded.append($0)
          return true
        }))
    defer { liveServer.stop() }

    let request = ControlTerminationRequest(
      expectedAgentInstanceID: UUID(), reason: .update, targetBuild: "137")
    let event = try ControlClient.command(
      ControlRequest(operation: .prepareForTermination, termination: request),
      socketURL: socket,
      timeout: .seconds(5))
    #expect(event == .commandDone)
    var commit = request
    commit.action = .commit
    let commitEvent = try ControlClient.command(
      ControlRequest(operation: .prepareForTermination, termination: commit),
      socketURL: socket,
      timeout: .seconds(5))
    #expect(commitEvent == .terminationCommitAccepted)
    let deadline = ContinuousClock.now + .seconds(1)
    while recorded.snapshot.count < 2, ContinuousClock.now < deadline {
      Thread.sleep(forTimeInterval: 0.01)
    }
    #expect(recorded.snapshot == [request, commit])
  }

  @Test func coalescedPrepareExecutesBeforeCommitStateCheck() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let releaseRegistration = DispatchSemaphore(value: 0)
    let registrationEntered = LockedFlag()
    let prepareWritten = LockedFlag()
    let commitWritten = LockedFlag()
    let prepared = LockedPreparedTermination()

    let startup = Self.dispatchBlocking {
      try ControlServer.start(
        socketURL: socket,
        handlers: ControlServerHandlers(
          status: { Self.snapshot() },
          reloadSettings: { AgentSettings() },
          prepareForTermination: { prepared.recordPrepare($0) },
          acceptTerminationCommit: { prepared.acceptCommit($0) }),
        startupObservation: ControlServerStartupObservation { event in
          guard event == .listenerRegistered else { return }
          registrationEntered.set()
          releaseRegistration.wait()
        })
    }
    await Self.waitUntil("listener registration", condition: { registrationEntered.value })

    let request = ControlTerminationRequest(
      expectedAgentInstanceID: UUID(), reason: .update, targetBuild: "137")
    var commit = request
    commit.action = .commit
    let responses = Task {
      try await Self.pairedTerminationCommands(
        prepare: ControlRequest(operation: .prepareForTermination, termination: request),
        commit: ControlRequest(operation: .prepareForTermination, termination: commit),
        socketURL: socket,
        didWritePrepare: { prepareWritten.set() },
        didWriteCommit: { commitWritten.set() })
    }
    await Self.waitUntil("coalesced prepare write", condition: { prepareWritten.value })
    await Self.waitUntil("coalesced commit write", condition: { commitWritten.value })

    releaseRegistration.signal()
    let server = try await startup.value()
    defer { server.stop() }
    let (prepareEvent, commitEvent) = try await responses.value
    #expect(prepareEvent == .commandDone)
    #expect(commitEvent == .terminationCommitAccepted)
    #expect(prepared.recordedPrepareThenCommit)
  }

  @Test func terminationPrepareRefusesBeforeAcknowledgingAnInvalidInstance() throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let recorded = LockedTerminationRequests()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: ControlServerHandlers(
        status: { Self.snapshot() },
        reloadSettings: { AgentSettings() },
        prepareForTermination: { recorded.append($0) },
        canPrepareTermination: { _ in false }))
    defer { server.stop() }

    let request = ControlTerminationRequest(
      expectedAgentInstanceID: UUID(), reason: .userQuit)
    let event = try ControlClient.command(
      ControlRequest(operation: .prepareForTermination, termination: request),
      socketURL: socket,
      timeout: .seconds(5))

    #expect(
      event
        == .commandFailed(
          ControlCommandFailure(
            category: .invalidArgument,
            detail: "termination prepare was not accepted")))
    #expect(recorded.snapshot.isEmpty)
  }

  @Test func contentPolicyCommandsStayTypedAndAccountScoped() throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let policy = ScriptedContentPolicyController()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(contentPolicy: policy))
    defer { server.stop() }

    let statusEvent = try ControlClient.command(
      ControlRequest(
        operation: .contentPolicyStatus,
        contentPolicy: ControlContentPolicyRequest(accountId: 777)),
      socketURL: socket,
      timeout: .seconds(5))
    #expect(statusEvent == .contentPolicyStatus(policy.auditStatus))

    let phrase = "PURGE ACCOUNT 777 AUDIT HISTORY"
    let retentionEvent = try ControlClient.command(
      ControlRequest(
        operation: .setRetention,
        contentPolicy: ControlContentPolicyRequest(
          accountId: 777,
          retention: .mirror,
          typedConfirmation: phrase)),
      socketURL: socket,
      timeout: .seconds(5))
    #expect(retentionEvent == .retentionChanged(policy.retentionTransition))
    #expect(
      policy.retentionRequests
        == [
          ScriptedContentPolicyController.RetentionRequest(
            accountId: 777,
            target: .mirror,
            typedConfirmation: phrase)
        ])

    let archiveEvent = try ControlClient.command(
      ControlRequest(
        operation: .setArchiveMode,
        contentPolicy: ControlContentPolicyRequest(
          accountId: 777,
          archiveModeEnabled: true)),
      socketURL: socket,
      timeout: .seconds(5))
    #expect(archiveEvent == .archiveModeChanged(policy.archiveTransition))
    #expect(policy.archiveRequests == [.init(accountId: 777, enabled: true)])

    let purgeEvent = try ControlClient.command(
      ControlRequest(
        operation: .resumeRetentionPurge,
        contentPolicy: ControlContentPolicyRequest(accountId: 777)),
      socketURL: socket,
      timeout: .seconds(5))
    #expect(purgeEvent == .retentionPurgeResumed(policy.purgeResume))
    #expect(policy.purgeAccounts == [777])
  }

  @Test func olderContentPolicyPayloadDefaultsToMirrorWithoutInventingProgress() throws {
    let data = Data(
      #"{"accountId":777,"futureAgentField":{"value":1}}"#.utf8)
    let decoded = try JSONDecoder().decode(ControlContentPolicyStatus.self, from: data)

    #expect(decoded.accountId == 777)
    #expect(decoded.retention == .mirror)
    #expect(decoded.archiveModeEnabled == false)
    #expect(decoded.pendingFilePurges == 0)
    #expect(decoded.auditToMirrorConfirmationPhrase.isEmpty)
    #expect(decoded.archiveBackfill == ControlArchiveBackfillProgress())
  }

  @Test func historyPriorityRunsTheOwnedSessionSeam() throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let received = LockedHistoryPriorities()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(historyPriority: { request in
        received.append(request)
        return .completed
      }))
    defer { server.stop() }

    let request = HistoryPriorityRequest(
      accountId: 42, chatId: 900, priority: .visible)
    let event = try ControlClient.command(
      ControlRequest(
        operation: .historyPriority,
        historyPriority: request),
      socketURL: socket,
      timeout: .seconds(5))
    #expect(event == .commandDone)
    #expect(received.snapshot() == [request])
  }

  @Test func providerPriorityClientDeliversTransitionsOffTheCaller() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let received = LockedHistoryPriorities()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(historyPriority: { request in
        received.append(request)
        return .completed
      }))
    defer { server.stop() }
    let client = AgentHistoryPriorityClient(socketURL: { socket })
    let transitions: [HistoryPriorityHint] = [.requested, .visible, .background]
    for priority in transitions {
      client.signal(
        HistoryPriorityRequest(
          accountId: 42, chatId: 900, priority: priority))
    }

    let deadline = ContinuousClock.now + .seconds(5)
    while received.snapshot().count < transitions.count,
      ContinuousClock.now < deadline
    {
      try await Task.sleep(for: .milliseconds(10))
    }
    #expect(received.snapshot().map(\.priority) == transitions)
  }

  @Test func providerFetchHealthClientDeliversIdentityFreeCountsOffTheCaller() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let received = LockedProviderFetchHealthReports()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(providerFetchHealth: { report in
        received.append(report)
        return .completed
      }))
    defer { server.stop() }

    let report = ProviderFetchHealthReport(
      succeeded: false,
      engineFailure: true,
      providerMapping: true,
      noSuchItem: true,
      retryable: false,
      observedAtMs: 1_000)
    let client = AgentProviderFetchHealthClient(socketURL: { socket })
    client.signal(report)

    let deadline = ContinuousClock.now + .seconds(5)
    while received.snapshot().isEmpty, ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(10))
    }
    #expect(received.snapshot() == [report])
  }

  @Test func repairRunsTheSeamAndReportsItsOutcome() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let repairer = ScriptedRepairer(
      outcome: .failed(
        ControlCommandFailure(category: .authRequired, detail: "sign in first")))
    let server = try ControlServer.start(
      socketURL: socket, handlers: Self.handlers(repairer: repairer))
    defer { server.stop() }

    let event = try await Self.command(
      ControlRequest(operation: .repair), socketURL: socket, timeout: .seconds(5))
    #expect(
      event
        == .commandFailed(
          ControlCommandFailure(category: .authRequired, detail: "sign in first")))
    #expect(repairer.runCount == 1)
  }

  @Test func removalRunsTheSeamWithItsParameters() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let remover = ScriptedRemover(outcome: .completed)
    let server = try ControlServer.start(
      socketURL: socket, handlers: Self.handlers(remover: remover))
    defer { server.stop() }

    let event = try await Self.command(
      ControlRequest(
        operation: .removeAccount,
        removal: ControlRemovalRequest(accountId: 777, revokeSession: true)),
      socketURL: socket,
      timeout: .seconds(5))
    #expect(event == .commandDone)
    #expect(remover.requests == [ControlRemovalRequest(accountId: 777, revokeSession: true)])
  }

  @Test func removalWithoutParametersIsRefusedTyped() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let server = try ControlServer.start(
      socketURL: socket, handlers: Self.handlers(remover: ScriptedRemover(outcome: .completed)))
    defer { server.stop() }

    let event = try await Self.command(ControlRequest(operation: .removeAccount), socketURL: socket)
    guard case .commandFailed(let failure) = event else {
      Issue.record("expected a typed refusal, got \(event)")
      return
    }
    #expect(failure.category == .invalidArgument)
  }

  @Test func aMissingSeamAnswersSourceUnavailable() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let server = try ControlServer.start(socketURL: socket, handlers: Self.handlers())
    defer { server.stop() }

    let event = try await Self.command(ControlRequest(operation: .repair), socketURL: socket)
    guard case .commandFailed(let failure) = event else {
      Issue.record("expected a typed refusal, got \(event)")
      return
    }
    #expect(failure.category == .sourceUnavailable)
  }

  @Test func aVersionMismatchIsRefusedTyped() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let server = try ControlServer.start(socketURL: socket, handlers: Self.handlers())
    defer { server.stop() }

    let event = try await Self.command(
      ControlRequest(protocolVersion: 99, operation: .status),
      socketURL: socket,
      timeout: .seconds(5))
    guard case .commandFailed(let failure) = event else {
      Issue.record("expected a typed refusal, got \(event)")
      return
    }
    #expect(failure.category == .invalidArgument)
    #expect(failure.detail.contains("protocol version"))
  }

  @Test func noAgentIsATypedTransportError() throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    #expect(throws: ControlTransportError.agentUnavailable(path: socket.path)) {
      _ = try ControlClient.command(
        ControlRequest(operation: .status), socketURL: socket, timeout: .seconds(1))
    }
  }

  // MARK: - The auth session

  @Test func authSessionStreamsStatesAndCorrelatesSubmits() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let session = ScriptedHostedSession()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(authorizer: ScriptedAuthorizer(session: session)))
    defer { server.stop() }

    let channel = try ControlAuthChannel.open(socketURL: socket)
    defer { channel.close() }
    let events = EventCollector(channel.events)
    await Self.waitUntil("the server attaches to the auth-state stream") {
      session.hasStateConsumer
    }
    session.emit(ControlAuthState(kind: "starting"))
    #expect(await events.next() == .authState(ControlAuthState(kind: "starting")))

    session.emit(ControlAuthState(kind: "wait-phone-number"))
    #expect(
      await events.next()
        == .authState(ControlAuthState(kind: "wait-phone-number")))

    try channel.send(
      ControlAuthInputFrame(seq: 7, input: .submitPhoneNumber("+9996612222")))
    #expect(
      await events.next()
        == .authSubmitResult(ControlAuthSubmitResult(seq: 7, outcome: "accepted")))
    #expect(session.submitted == [.submitPhoneNumber("+9996612222")])

    // A rejection answer keeps its classification and the caller's seq.
    session.answer = .rejected(
      ControlAuthRejection(kind: "invalid-code"))
    try channel.send(ControlAuthInputFrame(seq: 9, input: .submitCode("00000")))
    #expect(
      await events.next()
        == .authSubmitResult(
          ControlAuthSubmitResult(
            seq: 9,
            outcome: "rejected",
            rejection: ControlAuthRejection(kind: "invalid-code"))))
  }

  @Test func authDiagnosticsUseFixedCodesForSessionRefusalAndFinalization() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let session = ScriptedHostedSession()
    let diagnostics = AuthDiagnosticCollector()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(
        authorizer: ScriptedAuthorizer(session: session),
        authDiagnostics: { diagnostics.record($0) }))
    defer { server.stop() }

    let channel = try ControlAuthChannel.open(socketURL: socket)
    defer { channel.close() }
    let events = EventCollector(channel.events)
    await Self.waitUntil("the server attaches to the auth-state stream") {
      session.hasStateConsumer
    }
    session.answer = .rejected(
      ControlAuthRejection(kind: "other", code: 987_654_321, detail: "private-password"))
    try channel.send(ControlAuthInputFrame(seq: 1, input: .submitCode("843921")))
    _ = await events.next()
    session.emit(
      ControlAuthState(
        kind: "ready",
        account: ControlAccountIdentity(accountId: 987_654_321, displayName: "Ada Lovelace")))
    _ = await events.next()
    session.emit(ControlAuthState(kind: "failed", failureDetail: "tg://login?token=private"))
    _ = await events.next()

    await Self.waitUntil("auth diagnostic codes") {
      let codes = diagnostics.codes
      return codes.contains(.sessionStarted)
        && codes.contains(.refusedOther)
        && codes.contains(.finalizeSucceeded)
        && codes.contains(.finalizeFailed)
    }
  }

  @Test func statusCompletesWhileAnAuthChannelRemainsOpen() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let session = ScriptedHostedSession()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(authorizer: ScriptedAuthorizer(session: session)))
    defer { server.stop() }

    let channel = try ControlAuthChannel.open(socketURL: socket)
    defer { channel.close() }
    let events = EventCollector(channel.events)
    await Self.waitUntil("the server attaches to the auth-state stream") {
      session.hasStateConsumer
    }

    let status = try await Self.command(
      ControlRequest(operation: .status), socketURL: socket, timeout: .seconds(2))
    #expect(status == .status(Self.snapshot()))
    #expect(session.isClosed == false)

    session.emit(ControlAuthState(kind: "wait-phone-number"))
    #expect(
      await events.next(within: .seconds(2))
        == .authState(ControlAuthState(kind: "wait-phone-number")))
    try channel.send(
      ControlAuthInputFrame(seq: 17, input: .submitPhoneNumber("+9996612222")))
    #expect(
      await events.next(within: .seconds(2))
        == .authSubmitResult(ControlAuthSubmitResult(seq: 17, outcome: "accepted")))
    #expect(session.submitted == [.submitPhoneNumber("+9996612222")])
  }

  @Test func authSessionEndingFinishesTheChannel() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let session = ScriptedHostedSession()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(authorizer: ScriptedAuthorizer(session: session)))
    defer { server.stop() }

    let channel = try ControlAuthChannel.open(socketURL: socket)
    defer { channel.close() }
    let events = EventCollector(channel.events)
    await Self.waitUntil("the server attaches to the auth-state stream") {
      session.hasStateConsumer
    }
    session.emit(ControlAuthState(kind: "starting"))
    #expect(await events.next() == .authState(ControlAuthState(kind: "starting")))

    session.emit(ControlAuthState(kind: "closed"))
    session.finishStates()
    #expect(await events.next() == .authState(ControlAuthState(kind: "closed")))
    #expect(await events.next() == nil, "the stream ends with the session")
  }

  @Test func clientDisconnectClosesTheHostedSession() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let session = ScriptedHostedSession()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(authorizer: ScriptedAuthorizer(session: session)))
    defer { server.stop() }

    let channel = try ControlAuthChannel.open(socketURL: socket)
    let events = EventCollector(channel.events)
    await Self.waitUntil("the server attaches to the auth-state stream") {
      session.hasStateConsumer
    }
    session.emit(ControlAuthState(kind: "starting"))
    #expect(await events.next() == .authState(ControlAuthState(kind: "starting")))

    channel.close()
    let deadline = ContinuousClock.now + .seconds(5)
    while !session.isClosed, ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(20))
    }
    #expect(session.isClosed, "EOF must close the hosted session")
  }

  @Test func stalledHostedAuthSubmissionClosesTheChannelAtItsDeadline() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let session = StalledHostedSession()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(authorizer: ScriptedAuthorizer(session: session)),
      configuration: ControlServerConfiguration(authSubmissionTimeout: .milliseconds(20)))
    defer { server.stop() }

    let channel = try ControlAuthChannel.open(socketURL: socket)
    defer { channel.close() }
    let events = EventCollector(channel.events)
    try channel.send(ControlAuthInputFrame(seq: 1, input: .requestQrCode))

    #expect(await events.next(within: .seconds(1)) == nil)
    #expect(session.isClosed, "the timed-out host must be closed with its connection")
  }

  @Test func withoutAnAuthorizerTheUpgradeIsRefused() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let server = try ControlServer.start(socketURL: socket, handlers: Self.handlers())
    defer { server.stop() }

    let channel = try ControlAuthChannel.open(socketURL: socket)
    defer { channel.close() }
    let events = EventCollector(channel.events)
    guard case .commandFailed(let failure) = await events.next() else {
      Issue.record("expected a refusal event")
      return
    }
    #expect(failure.category == .sourceUnavailable)
  }

  @Test func aDuplicateSignInUpgradeIsRefusedAsBusy() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(authorizer: BusyAuthorizer()))
    defer { server.stop() }

    let channel = try ControlAuthChannel.open(socketURL: socket)
    defer { channel.close() }
    let events = EventCollector(channel.events)
    #expect(
      await events.next()
        == .commandFailed(
          ControlCommandFailure(
            category: .busy,
            detail: "a sign-in is already in progress")))
  }

  @Test func newerControlFailureCategoriesDecodeWithoutBreakingOlderPeers() throws {
    let busyEvent = ControlEvent.commandFailed(
      ControlCommandFailure(category: .busy, detail: "a sign-in is already in progress"))
    let encoded = try JSONEncoder().encode(busyEvent)
    #expect(try JSONDecoder().decode(ControlEvent.self, from: encoded) == busyEvent)

    // A peer that predates `busy` has the documented lenient decode posture:
    // it can still read the event and falls back to its safe internal bucket.
    #expect(
      try JSONDecoder().decode(LegacyFailureCategory.self, from: Data("\"busy\"".utf8))
        == .internalError)
  }

  @Test func stopClosesActiveSessions() async throws {
    let root = try Self.tempRoot()
    let socket = ControlContract.socketURL(dataRoot: root)
    try FileManager.default.createDirectory(
      at: socket.deletingLastPathComponent(), withIntermediateDirectories: true)
    let session = ScriptedHostedSession()
    let server = try ControlServer.start(
      socketURL: socket,
      handlers: Self.handlers(authorizer: ScriptedAuthorizer(session: session)))

    let channel = try ControlAuthChannel.open(socketURL: socket)
    defer { channel.close() }
    let events = EventCollector(channel.events)
    await Self.waitUntil("the server attaches to the auth-state stream") {
      session.hasStateConsumer
    }
    session.emit(ControlAuthState(kind: "starting"))
    #expect(await events.next() == .authState(ControlAuthState(kind: "starting")))

    server.stop()
    let deadline = ContinuousClock.now + .seconds(5)
    while !session.isClosed, ContinuousClock.now < deadline {
      try await Task.sleep(for: .milliseconds(20))
    }
    #expect(session.isClosed, "stop() must close hosted sessions")
  }
}

// MARK: - Bounded event consumption

/// Pumps a channel's events into a buffer so tests can await the next one
/// under a deadline — a wedged stream is a failure, never a hang.
final class EventCollector: @unchecked Sendable {
  private let lock = NSLock()
  private var items: [ControlEvent] = []
  private var finished = false
  private var cursor = 0

  init(_ stream: AsyncStream<ControlEvent>) {
    Task {
      for await event in stream {
        self.append(event)
      }
      self.markFinished()
    }
  }

  /// The next unseen event, `nil` once the stream finished. Fails the
  /// test on timeout.
  func next(
    within bound: Duration = .seconds(5),
    sourceLocation: Testing.SourceLocation = #_sourceLocation
  ) async -> ControlEvent? {
    let deadline = ContinuousClock.now + bound
    while ContinuousClock.now < deadline {
      if let (event, done) = poll() {
        if done { return nil }
        return event
      }
      try? await Task.sleep(for: .milliseconds(10))
    }
    Issue.record("no event arrived within the bound", sourceLocation: sourceLocation)
    return nil
  }

  private func poll() -> (ControlEvent?, Bool)? {
    lock.lock()
    defer { lock.unlock() }
    if cursor < items.count {
      let event = items[cursor]
      cursor += 1
      return (event, false)
    }
    if finished {
      return (nil, true)
    }
    return nil
  }

  private func append(_ event: ControlEvent) {
    lock.lock()
    items.append(event)
    lock.unlock()
  }

  private func markFinished() {
    lock.lock()
    finished = true
    lock.unlock()
  }
}

// MARK: - Scripted seams

private final class ScriptedRepairer: AgentRepairing, @unchecked Sendable {
  private let lock = NSLock()
  private let outcome: ControlCommandOutcome
  private var runs = 0

  init(outcome: ControlCommandOutcome) {
    self.outcome = outcome
  }

  var runCount: Int {
    lock.lock()
    defer { lock.unlock() }
    return runs
  }

  func repair() async -> ControlCommandOutcome {
    recordRun()
    return outcome
  }

  private func recordRun() {
    lock.lock()
    runs += 1
    lock.unlock()
  }
}

private final class ScriptedRemover: AgentAccountRemoving, @unchecked Sendable {
  private let lock = NSLock()
  private let outcome: ControlCommandOutcome
  private var received: [ControlRemovalRequest] = []

  init(outcome: ControlCommandOutcome) {
    self.outcome = outcome
  }

  var requests: [ControlRemovalRequest] {
    lock.lock()
    defer { lock.unlock() }
    return received
  }

  func remove(_ request: ControlRemovalRequest) async -> ControlCommandOutcome {
    record(request)
    return outcome
  }

  private func record(_ request: ControlRemovalRequest) {
    lock.lock()
    received.append(request)
    lock.unlock()
  }
}

private final class ScriptedContentPolicyController:
  AgentContentPolicyControlling, @unchecked Sendable
{
  struct RetentionRequest: Equatable {
    var accountId: Int64
    var target: ControlRetentionMode
    var typedConfirmation: String?
  }

  struct ArchiveRequest: Equatable {
    var accountId: Int64
    var enabled: Bool
  }

  private let lock = NSLock()
  private var recordedRetention: [RetentionRequest] = []
  private var recordedArchive: [ArchiveRequest] = []
  private var recordedPurge: [Int64] = []

  let auditStatus = ControlContentPolicyStatus(
    accountId: 777,
    retention: .audit,
    archiveModeEnabled: false,
    pendingFilePurges: 3,
    auditToMirrorConfirmationPhrase: "PURGE ACCOUNT 777 AUDIT HISTORY",
    archiveBackfill: ControlArchiveBackfillProgress(
      pendingAllowedItems: 9,
      failedAllowedItems: 1))

  var mirrorStatus: ControlContentPolicyStatus {
    ControlContentPolicyStatus(
      accountId: 777,
      retention: .mirror,
      archiveModeEnabled: false,
      pendingFilePurges: 3,
      auditToMirrorConfirmationPhrase: "PURGE ACCOUNT 777 AUDIT HISTORY")
  }

  var retentionTransition: ControlRetentionTransition {
    ControlRetentionTransition(
      previous: .audit,
      current: .mirror,
      purgedRevisions: 4,
      purgedDeletedMetadata: 5,
      purgedRetainedBytes: 6,
      invalidatedItems: 7,
      invalidatedDocuments: 8,
      acknowledgedFilePurges: 2,
      status: mirrorStatus)
  }

  var archiveTransition: ControlArchiveModeTransition {
    var current = mirrorStatus
    current.archiveModeEnabled = true
    return ControlArchiveModeTransition(
      previous: false,
      current: true,
      pinnedAllowedItems: 12,
      releasedItems: 0,
      status: current)
  }

  var purgeResume: ControlRetentionPurgeResume {
    var current = mirrorStatus
    current.pendingFilePurges = 0
    return ControlRetentionPurgeResume(
      acknowledgedFilePurges: 3,
      status: current)
  }

  var retentionRequests: [RetentionRequest] {
    lock.lock()
    defer { lock.unlock() }
    return recordedRetention
  }

  var archiveRequests: [ArchiveRequest] {
    lock.lock()
    defer { lock.unlock() }
    return recordedArchive
  }

  var purgeAccounts: [Int64] {
    lock.lock()
    defer { lock.unlock() }
    return recordedPurge
  }

  func status(accountId: Int64) async throws -> ControlContentPolicyStatus {
    #expect(accountId == 777)
    return auditStatus
  }

  func setRetention(
    accountId: Int64,
    target: ControlRetentionMode,
    typedConfirmation: String?
  ) async throws -> ControlRetentionTransition {
    recordRetention(
      RetentionRequest(
        accountId: accountId,
        target: target,
        typedConfirmation: typedConfirmation))
    return retentionTransition
  }

  private func recordRetention(_ request: RetentionRequest) {
    lock.lock()
    recordedRetention.append(request)
    lock.unlock()
  }

  func setArchiveMode(
    accountId: Int64,
    enabled: Bool
  ) async throws -> ControlArchiveModeTransition {
    recordArchive(ArchiveRequest(accountId: accountId, enabled: enabled))
    return archiveTransition
  }

  private func recordArchive(_ request: ArchiveRequest) {
    lock.lock()
    recordedArchive.append(request)
    lock.unlock()
  }

  func resumeRetentionPurge(
    accountId: Int64
  ) async throws -> ControlRetentionPurgeResume {
    recordPurge(accountId)
    return purgeResume
  }

  private func recordPurge(_ accountId: Int64) {
    lock.lock()
    recordedPurge.append(accountId)
    lock.unlock()
  }
}

private struct ScriptedAuthorizer: AgentAuthorizing {
  let session: any AgentAuthSessionHosting

  func makeSession() throws -> any AgentAuthSessionHosting {
    session
  }
}

private struct BusyAuthorizer: AgentAuthorizing {
  func makeSession() throws -> any AgentAuthSessionHosting {
    throw DriveError.InvalidArgument(detail: "another sign-in is already running")
  }
}

private enum LegacyFailureCategory: String, Decodable, Equatable {
  case invalidArgument
  case notFound
  case authRequired
  case rateLimited
  case sourceUnavailable
  case storage
  case integrity
  case cancelled
  case internalError = "internal"

  init(from decoder: Decoder) throws {
    let raw = try decoder.singleValueContainer().decode(String.self)
    self = LegacyFailureCategory(rawValue: raw) ?? .internalError
  }
}

private final class AuthDiagnosticCollector: @unchecked Sendable {
  private let lock = NSLock()
  private var recorded: [AuthDiagnosticCode] = []

  func record(_ code: AuthDiagnosticCode) {
    lock.lock()
    recorded.append(code)
    lock.unlock()
  }

  var codes: [AuthDiagnosticCode] {
    lock.lock()
    defer { lock.unlock() }
    return recorded
  }
}

/// A hand-scripted hosted session: tests emit states and pick the answer
/// each submit receives.
final class ScriptedHostedSession: AgentAuthSessionHosting, @unchecked Sendable {
  private let lock = NSLock()
  private let stream: AsyncStream<ControlAuthState>
  private let continuation: AsyncStream<ControlAuthState>.Continuation
  private var inputs: [ControlAuthInput] = []
  private var closed = false
  private var stateConsumerReady = false

  /// The answer the next submit receives; tests mutate between inputs.
  var answer: AgentAuthSubmitAnswer = .accepted

  init() {
    (stream, continuation) = AsyncStream.makeStream(of: ControlAuthState.self)
  }

  var states: AsyncStream<ControlAuthState> {
    lock.lock()
    stateConsumerReady = true
    lock.unlock()
    return stream
  }

  var hasStateConsumer: Bool {
    lock.lock()
    defer { lock.unlock() }
    return stateConsumerReady
  }

  var submitted: [ControlAuthInput] {
    lock.lock()
    defer { lock.unlock() }
    return inputs
  }

  var isClosed: Bool {
    lock.lock()
    defer { lock.unlock() }
    return closed
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

  private func record(_ input: ControlAuthInput) -> AgentAuthSubmitAnswer {
    lock.lock()
    defer { lock.unlock() }
    inputs.append(input)
    return answer
  }

  func close() {
    lock.lock()
    closed = true
    lock.unlock()
    continuation.finish()
  }
}

private final class StalledHostedSession: AgentAuthSessionHosting, @unchecked Sendable {
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
