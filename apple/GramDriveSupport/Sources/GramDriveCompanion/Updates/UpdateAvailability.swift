import Observation

/// Observable projection of Sparkle's KVO-backed manual-update capability.
/// Views observe this value directly, so a change in `SPUUpdater` invalidates
/// the menu, toolbar, and command surface instead of leaving stale closures.
@Observable
@MainActor
public final class UpdateAvailability {
  public static let unavailable = UpdateAvailability(canCheckForUpdates: false)

  public private(set) var canCheckForUpdates: Bool

  public init(canCheckForUpdates: Bool = false) {
    self.canCheckForUpdates = canCheckForUpdates
  }

  public func setCanCheckForUpdates(_ enabled: Bool) {
    canCheckForUpdates = enabled
  }
}

/// The small testable action surface shared by the menu, settings toolbar,
/// and app command. A disabled Sparkle updater never receives a manual check.
@MainActor
public struct ManualUpdateAction {
  private let availability: UpdateAvailability
  private let invokeUpdater: () -> Void

  public init(availability: UpdateAvailability, invokeUpdater: @escaping () -> Void) {
    self.availability = availability
    self.invokeUpdater = invokeUpdater
  }

  public var isEnabled: Bool { availability.canCheckForUpdates }

  public func invoke() {
    guard isEnabled else { return }
    invokeUpdater()
  }
}
