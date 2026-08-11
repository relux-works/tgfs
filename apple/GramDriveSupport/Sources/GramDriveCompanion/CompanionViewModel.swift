import Foundation
import GramDriveAgentCore
import GramDriveSupport

/// The sections of the companion shell — the whole required surface for v1
/// (PLAT-MAC-005): authorization, account/provider status, cache/offline
/// settings, diagnostics, repair, removal.
public enum CompanionSection: String, CaseIterable, Identifiable, Sendable {
  case account
  case authorization
  case storage
  case diagnostics
  case repair
  case removal

  public var id: String { rawValue }

  public var title: String {
    switch self {
    case .account: return "Account"
    case .authorization: return "Sign In"
    case .storage: return "Storage & Offline"
    case .diagnostics: return "Diagnostics"
    case .repair: return "Repair"
    case .removal: return "Remove Account"
    }
  }

  public var systemImage: String {
    switch self {
    case .account: return "person.crop.circle"
    case .authorization: return "key.fill"
    case .storage: return "externaldrive.fill"
    case .diagnostics: return "stethoscope"
    case .repair: return "wrench.and.screwdriver.fill"
    case .removal: return "trash.fill"
    }
  }
}

/// The root view model: owns the per-section view models and the selection,
/// all over one ``CompanionBackend``.
@MainActor
@Observable
public final class CompanionViewModel {
  public var selectedSection: CompanionSection = .account

  public let status: CompanionStatusViewModel
  public let authorization: AuthorizationViewModel
  public let settings: CompanionSettingsViewModel
  public let contentPolicies: ContentPolicySettingsViewModel
  public let repair: RepairViewModel
  public let removal: AccountRemovalViewModel
  /// The first-launch onboarding flow, over the same live sub-view models so
  /// sign-in and defaults chosen during onboarding are the shell's own.
  public let onboarding: OnboardingViewModel

  public init(
    backend: any CompanionBackend,
    loginItemService: (any LoginItemService)? = nil,
    diskProbe: any DiskSpaceProbe,
    accountLabel: String,
    driveLocation: any DriveLocationProviding = CloudStorageDriveLocation(),
    domainSetup: any FileProviderDomainSettingUp,
    onboardingStore: any OnboardingCompletionStore = UserDefaultsOnboardingCompletionStore()
  ) {
    let status = CompanionStatusViewModel(backend: backend)
    let authorization = AuthorizationViewModel(backend: backend)
    let settings = CompanionSettingsViewModel(
      backend: backend, loginItemService: loginItemService, diskProbe: diskProbe)
    let contentPolicies = ContentPolicySettingsViewModel(
      backend: backend, diskProbe: diskProbe)
    self.status = status
    self.authorization = authorization
    self.settings = settings
    self.contentPolicies = contentPolicies
    self.repair = RepairViewModel(backend: backend)
    self.removal = AccountRemovalViewModel(backend: backend, accountLabel: accountLabel)
    self.onboarding = OnboardingViewModel(
      authorization: authorization,
      settings: settings,
      status: status,
      driveLocation: driveLocation,
      domainSetup: domainSetup,
      completionStore: onboardingStore)
  }

  /// Refreshes status and reloads settings — the shell's on-appear pass.
  public func refresh() async {
    await status.refresh()
    settings.load()
    await contentPolicies.refresh(from: status.readout)
  }

  /// Establishes the companion session's agent readiness before launch UI
  /// is presented. Live status refresh owns the cold-start barrier; scripted
  /// backends remain deterministic in previews and tests.
  public func startAgentSession() async {
    await status.refresh()
    guard Self.hasAuthorizedAccount(in: status.readout) else { return }
    await onboarding.prepareFileProviderDomain()
  }

  /// A privacy-preserving launch gate: only the authorization marker is
  /// inspected. No account identity or display name reaches UI or logs.
  public nonisolated static func hasAuthorizedAccount(in readout: HealthReadout) -> Bool {
    guard case .running(let snapshot) = readout else { return false }
    return snapshot.accounts?.contains { $0.authState == "authorized" } == true
  }

  /// The product wiring: a shell over the ``LiveCompanionBackend`` for one
  /// agent runtime layout, with the real login-item service and volume disk
  /// probe. The App Group container is resolved by the same rule every
  /// GramDrive process follows. `accountDomainCleanup` is the app half of
  /// an account removal (File Provider domain deregistration), injected by
  /// the executable because only the app embedding the extension may run
  /// it.
  public static func live(
    layout: AgentRuntimeLayout,
    accountLabel: String = "This account",
    domainSetup: any FileProviderDomainSettingUp,
    accountDomainCleanup: (@Sendable (Int64) async -> Void)? = nil,
    matchingAgentReady: (@Sendable () async -> Void)? = nil
  ) -> CompanionViewModel {
    CompanionViewModel(
      backend: LiveCompanionBackend(
        layout: layout,
        accountDomainCleanup: accountDomainCleanup,
        matchingAgentReady: matchingAgentReady),
      loginItemService: SMAppServiceAgentLoginItem(),
      diskProbe: VolumeDiskSpaceProbe(url: layout.dataRoot),
      accountLabel: accountLabel,
      domainSetup: domainSetup)
  }
}
