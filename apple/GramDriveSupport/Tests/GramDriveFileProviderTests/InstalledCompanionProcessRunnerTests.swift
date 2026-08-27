import Dispatch
import Foundation
import Testing

@testable import GramDriveFileProvider

@Suite("Installed companion process runner")
struct InstalledCompanionProcessRunnerTests {
  @Test("Headless routing schedules the async command and services its callback")
  func headlessRoutingExecutesCommandThroughCallbackLoop() {
    let terminated = DispatchSemaphore(value: 0)
    let result = ProcessResultProbe()
    var applicationRuns = 0

    InstalledCompanionProcessRunner.run(
      commandRequested: true,
      scheduleCommand: { operation in
        Task.detached { await operation() }
      },
      runCommand: {
        await result.awaitCallbackService()
        return 0
      },
      terminate: { exitCode in
        result.record(exitCode)
        terminated.signal()
      },
      runApplication: {
        applicationRuns += 1
      },
      serviceCallbacks: {
        #expect(result.waitForCommandStart(timeout: .now() + .milliseconds(1_500)))
        result.serviceCallback()
        #expect(terminated.wait(timeout: .now() + .milliseconds(1_500)) == .success)
      })

    #expect(result.exitCode == 0)
    #expect(applicationRuns == 0)
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

private final class ProcessResultProbe: @unchecked Sendable {
  private let lock = NSLock()
  private let commandStarted = DispatchSemaphore(value: 0)
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
      lock.withLock { callbackContinuation = continuation }
      commandStarted.signal()
    }
  }

  func waitForCommandStart(timeout: DispatchTime) -> Bool {
    commandStarted.wait(timeout: timeout) == .success
  }

  func serviceCallback() {
    let continuation = lock.withLock {
      let continuation = callbackContinuation
      callbackContinuation = nil
      return continuation
    }
    continuation?.resume()
  }
}
