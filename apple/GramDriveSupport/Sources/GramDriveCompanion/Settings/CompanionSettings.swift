import Foundation
import GramDriveAgentCore

/// Free space at the location Archive Mode would fill. A seam so the
/// preflight can be tested without a real volume.
public protocol DiskSpaceProbe: Sendable {
    /// Bytes available for important new content, or `nil` when the volume
    /// cannot report it.
    func availableCapacityBytes() -> UInt64?
}

/// The product disk probe over a container URL.
public struct VolumeDiskSpaceProbe: DiskSpaceProbe {
    private let url: URL

    public init(url: URL) {
        self.url = url
    }

    public func availableCapacityBytes() -> UInt64? {
        let values = try? url.resourceValues(
            forKeys: [.volumeAvailableCapacityForImportantUsageKey])
        return values?.volumeAvailableCapacityForImportantUsage.flatMap { capacity in
            capacity >= 0 ? UInt64(capacity) : nil
        }
    }
}

/// The result of the pre-enable check POL-2 requires before Archive Mode is
/// switched on: projected disk usage, and a low-disk warning when the scope
/// would not comfortably fit.
public enum ArchiveModePreflight: Equatable, Sendable {
    /// The scope fits: `projectedBytes` will be mirrored eagerly, with
    /// `availableBytes` free (when the volume reported it).
    case ok(projectedBytes: UInt64, availableBytes: UInt64?)
    /// The scope would not comfortably fit; enabling risks filling the disk.
    case lowDisk(projectedBytes: UInt64, availableBytes: UInt64)

    public var projectedBytes: UInt64 {
        switch self {
        case .ok(let projected, _), .lowDisk(let projected, _): return projected
        }
    }

    public var isLowDisk: Bool {
        if case .lowDisk = self { return true }
        return false
    }
}

/// Edits the durable host-owned settings: managed-cache quota and global
/// Archive Mode (POL-2), and launch-at-login (PLAT-MAC-005).
///
/// Loads and saves through the ``CompanionBackend`` seam (the settings
/// document the app writes and the agent reads); the launch-at-login
/// preference is additionally reconciled with launchd through
/// ``LaunchAtLoginPolicy`` — the app owns that registration, the agent only
/// reports it. Every derivation (GB projection, preflight) is a pure function
/// of the edited state, so the screen is fully testable.
@MainActor
@Observable
public final class CompanionSettingsViewModel {
    /// The managed-cache quota, in bytes (POL-2 default 10 GB).
    public var cacheQuotaBytes: UInt64 = AgentSettings.defaultCacheQuotaBytes
    /// Whether global Archive Mode is on.
    public var archiveModeEnabled: Bool = false
    /// Whether the agent should launch at login.
    public var launchAtLogin: Bool = false
    /// The last launch-at-login reconciliation result (e.g. awaiting the
    /// user's approval in System Settings), for the UI to surface.
    public private(set) var lastLaunchAction: LaunchAtLoginAction?
    /// Set when loading or saving settings failed; diagnostic, not contractual.
    public private(set) var lastError: String?

    private let backend: any CompanionBackend
    private let loginItemService: (any LoginItemService)?
    private let diskProbe: any DiskSpaceProbe
    private let lowDiskBufferBytes: UInt64

    public init(
        backend: any CompanionBackend,
        loginItemService: (any LoginItemService)? = nil,
        diskProbe: any DiskSpaceProbe,
        lowDiskBufferBytes: UInt64 = 2_000_000_000
    ) {
        self.backend = backend
        self.loginItemService = loginItemService
        self.diskProbe = diskProbe
        self.lowDiskBufferBytes = lowDiskBufferBytes
    }

    /// The cache quota expressed in (base-10) gigabytes, for a stepper/slider.
    public var cacheQuotaGigabytes: Double {
        get { Double(cacheQuotaBytes) / 1_000_000_000 }
        set { cacheQuotaBytes = UInt64((max(0, newValue) * 1_000_000_000).rounded()) }
    }

    /// Loads the persisted settings into the editable fields. A load failure
    /// leaves the defaults in place and records the error.
    public func load() {
        do {
            let settings = try backend.loadSettings()
            cacheQuotaBytes = settings.cacheQuotaBytes
            archiveModeEnabled = settings.archiveModeEnabled
            launchAtLogin = settings.launchAtLogin
            lastError = nil
        } catch {
            lastError = String(describing: error)
        }
    }

    /// The settings value the editable fields currently describe.
    public var edited: AgentSettings {
        AgentSettings(
            launchAtLogin: launchAtLogin,
            cacheQuotaBytes: cacheQuotaBytes,
            archiveModeEnabled: archiveModeEnabled)
    }

    /// Persists the edited settings. Returns whether it succeeded.
    @discardableResult
    public func save() -> Bool {
        do {
            try backend.saveSettings(edited)
            lastError = nil
            return true
        } catch {
            lastError = String(describing: error)
            return false
        }
    }

    /// The POL-2 pre-enable check for Archive Mode over a scope estimated at
    /// `estimatedArchiveBytes`. Pure; the UI shows the projection and blocks
    /// (or warns) on low disk before flipping ``archiveModeEnabled``.
    public func archiveModePreflight(estimatedArchiveBytes: UInt64) -> ArchiveModePreflight {
        let available = diskProbe.availableCapacityBytes()
        guard let available else {
            return .ok(projectedBytes: estimatedArchiveBytes, availableBytes: nil)
        }
        let needed = estimatedArchiveBytes.addingReportingOverflow(lowDiskBufferBytes)
        let wouldNotFit = needed.overflow || needed.partialValue > available
        return wouldNotFit
            ? .lowDisk(projectedBytes: estimatedArchiveBytes, availableBytes: available)
            : .ok(projectedBytes: estimatedArchiveBytes, availableBytes: available)
    }

    /// Reconciles the launch-at-login preference with launchd (idempotent),
    /// updating ``launchAtLogin`` and ``lastLaunchAction``. A no-op when no
    /// login-item service was supplied (e.g. running outside a signed bundle).
    @discardableResult
    public func applyLaunchAtLogin(_ enabled: Bool) -> LaunchAtLoginAction? {
        launchAtLogin = enabled
        guard let loginItemService else { return nil }
        do {
            let action = try LaunchAtLoginPolicy.reconcile(
                preference: enabled, service: loginItemService)
            lastLaunchAction = action
            return action
        } catch {
            lastError = String(describing: error)
            return nil
        }
    }
}
