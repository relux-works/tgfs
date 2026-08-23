import AppKit
import Darwin
import Foundation
import GramDriveCompanion
import Sparkle

private struct SmokeResult: Codable {
  let initialQualifyingWindowCount: Int
  let windowTitle: String
  let windowIsVisible: Bool
  let windowCanBecomeKey: Bool
  let activationPolicy: String
  let applicationIsActive: Bool
  let hostBuild: String
  let offeredBuild: String
}

@MainActor
private final class SmokeApplicationDelegate: NSObject, NSApplicationDelegate {
  private let deadline = ContinuousClock.now + .seconds(10)
  private var initialQualifyingWindowCount = 0
  private var readinessObservation: NSKeyValueObservation?
  private var updaterController: SPUStandardUpdaterController?

  func applicationDidFinishLaunching(_ notification: Notification) {
    initialQualifyingWindowCount = qualifyingWindows.count
    guard initialQualifyingWindowCount == 0 else {
      fail("host did not start from zero qualifying windows")
    }

    let controller = SPUStandardUpdaterController(
      startingUpdater: true,
      updaterDelegate: nil,
      userDriverDelegate: nil)
    updaterController = controller
    readinessObservation = controller.updater.observe(
      \.canCheckForUpdates,
      options: [.initial, .new]
    ) { [weak self] updater, _ in
      guard updater.canCheckForUpdates else { return }
      Task { @MainActor [weak self] in
        self?.beginManualCheck()
      }
    }
    pollForResult()
  }

  private func beginManualCheck() {
    guard let updaterController else {
      fail("Sparkle updater controller was released before the check")
    }
    readinessObservation = nil
    let availability = UpdateAvailability(canCheckForUpdates: true)
    ManualUpdateAction(
      availability: availability,
      activateApplication: { CompanionApplicationActivation.activate() },
      invokeUpdater: { updaterController.checkForUpdates(nil) }
    ).invoke()
  }

  private func pollForResult() {
    if let window = qualifyingWindows.first,
      NSApplication.shared.activationPolicy() == .regular
    {
      succeed(window: window)
      return
    }
    guard ContinuousClock.now < deadline else {
      fail("no visible key-capable standard Sparkle window appeared before the deadline")
    }
    DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(50)) { [weak self] in
      self?.pollForResult()
    }
  }

  private var qualifyingWindows: [NSWindow] {
    NSApplication.shared.windows.filter {
      $0.isVisible && $0.canBecomeKey && $0.level == .normal
    }
  }

  private func succeed(window: NSWindow) -> Never {
    let result = SmokeResult(
      initialQualifyingWindowCount: initialQualifyingWindowCount,
      windowTitle: window.title,
      windowIsVisible: window.isVisible,
      windowCanBecomeKey: window.canBecomeKey,
      activationPolicy: "regular",
      applicationIsActive: NSApplication.shared.isActive,
      hostBuild: Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "",
      offeredBuild: "2")
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys]
    guard let data = try? encoder.encode(result),
      let payload = String(data: data, encoding: .utf8)
    else {
      fail("could not encode smoke result")
    }
    print("SPARKLE_MANUAL_UPDATE_SMOKE \(payload)")
    fflush(stdout)
    Darwin.exit(EXIT_SUCCESS)
  }

  private func fail(_ message: String) -> Never {
    fputs("SPARKLE_MANUAL_UPDATE_SMOKE_ERROR \(message)\n", stderr)
    fflush(stderr)
    Darwin.exit(EXIT_FAILURE)
  }
}

@main
private enum SparkleManualUpdateSmokeHost {
  @MainActor
  static func main() {
    let application = NSApplication.shared
    let delegate = SmokeApplicationDelegate()
    application.delegate = delegate
    application.setActivationPolicy(.accessory)
    application.run()
  }
}
