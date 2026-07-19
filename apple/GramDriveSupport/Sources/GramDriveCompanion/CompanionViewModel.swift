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
    public let repair: RepairViewModel
    public let removal: AccountRemovalViewModel

    public init(
        backend: any CompanionBackend,
        loginItemService: (any LoginItemService)? = nil,
        diskProbe: any DiskSpaceProbe,
        accountLabel: String
    ) {
        self.status = CompanionStatusViewModel(backend: backend)
        self.authorization = AuthorizationViewModel(backend: backend)
        self.settings = CompanionSettingsViewModel(
            backend: backend, loginItemService: loginItemService, diskProbe: diskProbe)
        self.repair = RepairViewModel(backend: backend)
        self.removal = AccountRemovalViewModel(backend: backend, accountLabel: accountLabel)
    }

    /// Refreshes status and reloads settings — the shell's on-appear pass.
    public func refresh() async {
        await status.refresh()
        settings.load()
    }

    /// The product wiring: a shell over the ``LiveCompanionBackend`` for one
    /// agent runtime layout, with the real login-item service and volume disk
    /// probe. The App Group container is resolved by the same rule every
    /// GramDrive process follows.
    public static func live(
        layout: AgentRuntimeLayout,
        accountLabel: String = "This account"
    ) -> CompanionViewModel {
        CompanionViewModel(
            backend: LiveCompanionBackend(layout: layout),
            loginItemService: SMAppServiceAgentLoginItem(),
            diskProbe: VolumeDiskSpaceProbe(url: layout.dataRoot),
            accountLabel: accountLabel)
    }
}
