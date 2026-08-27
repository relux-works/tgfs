/// Dependency-injected routing for the companion executable's two process modes.
///
/// The headless command must schedule its asynchronous File Provider work before
/// entering the callback loop. Ordinary launches must enter only the SwiftUI
/// application lifecycle.
public enum InstalledCompanionProcessRunner {
  public static func run(
    commandRequested: Bool,
    scheduleCommand: (@escaping @Sendable () async -> Void) -> Void,
    runCommand: @escaping @Sendable () async -> Int32,
    terminate: @escaping @Sendable (Int32) -> Void,
    runApplication: () -> Void,
    serviceCallbacks: () -> Void
  ) {
    guard commandRequested else {
      runApplication()
      return
    }

    scheduleCommand {
      terminate(await runCommand())
    }
    serviceCallbacks()
  }
}
