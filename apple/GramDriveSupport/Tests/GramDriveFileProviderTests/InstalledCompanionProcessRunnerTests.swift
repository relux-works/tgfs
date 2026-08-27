import Foundation
import Testing

@testable import GramDriveFileProvider

@Suite("Installed companion process runner")
struct InstalledCompanionProcessRunnerTests {
  @Test("Headless routing schedules the async command and services its callback")
  func headlessRoutingExecutesCommandThroughCallbackLoop() async throws {
    let scheduler = ScheduledOperationProbe()
    let result = ProcessResultProbe()
    var applicationRuns = 0
    var callbackLoops = 0

    InstalledCompanionProcessRunner.run(
      commandRequested: true,
      scheduleCommand: { operation in
        scheduler.schedule(operation)
      },
      runCommand: {
        await result.awaitCallbackService()
        return 73
      },
      terminate: { exitCode in
        result.record(exitCode)
      },
      runApplication: {
        applicationRuns += 1
      },
      serviceCallbacks: {
        #expect(scheduler.scheduleCount == 1)
        #expect(result.exitCode == nil)
        callbackLoops += 1
        result.serviceCallback()
      })

    #expect(callbackLoops == 1)
    #expect(applicationRuns == 0)
    try await scheduler.runScheduledOperation()
    #expect(result.exitCode == 73)
  }

  @Test("Ordinary routing invokes only the SwiftUI application")
  func ordinaryRoutingInvokesOnlyApplication() {
    var scheduledCommands = 0
    var applicationRuns = 0
    var callbackLoops = 0
    let result = ProcessResultProbe()

    InstalledCompanionProcessRunner.run(
      commandRequested: false,
      scheduleCommand: { _ in scheduledCommands += 1 },
      runCommand: { 0 },
      terminate: { result.record($0) },
      runApplication: { applicationRuns += 1 },
      serviceCallbacks: { callbackLoops += 1 })

    #expect(applicationRuns == 1)
    #expect(scheduledCommands == 0)
    #expect(callbackLoops == 0)
    #expect(result.exitCode == nil)
  }
}

private final class ScheduledOperationProbe: @unchecked Sendable {
  typealias Operation = @Sendable () async -> Void

  private let lock = NSLock()
  private var operation: Operation?
  private var count = 0

  var scheduleCount: Int {
    lock.withLock { count }
  }

  func schedule(_ operation: @escaping Operation) {
    lock.withLock {
      self.operation = operation
      count += 1
    }
  }

  func runScheduledOperation() async throws {
    let scheduledOperation: Operation? = lock.withLock { self.operation }
    let executedOperation = try #require(scheduledOperation)
    await executedOperation()
  }
}

private final class ProcessResultProbe: @unchecked Sendable {
  private let lock = NSLock()
  private var callbackWasServiced = false
  private var callbackContinuation: CheckedContinuation<Void, Never>?
  private var value: Int32?

  var exitCode: Int32? {
    lock.withLock { value }
  }

  func record(_ exitCode: Int32) {
    lock.withLock { value = exitCode }
  }

  func awaitCallbackService() async {
    await withCheckedContinuation { continuation in
      let resumeImmediately = lock.withLock {
        guard !callbackWasServiced else { return true }
        callbackContinuation = continuation
        return false
      }
      if resumeImmediately { continuation.resume() }
    }
  }

  func serviceCallback() {
    let continuation = lock.withLock {
      callbackWasServiced = true
      let continuation = callbackContinuation
      callbackContinuation = nil
      return continuation
    }
    continuation?.resume()
  }
}
