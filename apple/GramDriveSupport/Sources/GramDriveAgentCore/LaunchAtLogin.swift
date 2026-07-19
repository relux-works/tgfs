import Foundation
import ServiceManagement

/// Registration state of the agent's login item, in the vocabulary the
/// policy needs (a projection of `SMAppService.Status`).
public enum LoginItemStatus: String, Sendable, Equatable {
    /// Not registered; the agent will not start at login.
    case notRegistered
    /// Registered and approved; launchd starts the agent at login.
    case enabled
    /// Registered but awaiting the user's approval in System Settings —
    /// the platform, not the product, owns that consent step.
    case requiresApproval
    /// The system cannot find the item (e.g. the plist is missing from the
    /// caller's bundle) — a packaging bug, not a user state.
    case notFound
}

/// The registration surface ``LaunchAtLoginPolicy`` drives.
///
/// The product implementation is ``SMAppServiceAgentLoginItem``; tests
/// substitute a fake. Split as a protocol because `SMAppService` can only
/// operate from inside the signed app bundle that carries the agent's
/// launchd plist — a platform constraint no test host satisfies.
public protocol LoginItemService {
    /// Current registration status.
    var status: LoginItemStatus { get }
    /// Registers the item with launchd.
    func register() throws
    /// Unregisters the item from launchd.
    func unregister() throws
}

/// What ``LaunchAtLoginPolicy/reconcile(preference:service:)`` did.
public enum LaunchAtLoginAction: Equatable, Sendable {
    /// The item was registered (and is now enabled).
    case registered
    /// The item was unregistered.
    case unregistered
    /// Registration exists (or was just requested) but the user has not
    /// approved it in System Settings yet. Surfaced, never retried in a
    /// loop: approval belongs to the user.
    case awaitingApproval
    /// Registration already matched the preference; nothing was touched.
    case noChange
}

/// Reconciles the user's launch-at-login preference with the actual
/// launchd registration — the "startup policy honoring user preference"
/// rule (PLAT-MAC-005) in one idempotent operation.
///
/// Called by the app shell whenever the preference changes and at app
/// startup. The agent itself never calls `register`/`unregister`: the
/// login item's plist lives in the *app* bundle
/// (`Contents/Library/LaunchAgents`), so only the app can operate it —
/// the agent honors the preference by reporting it (health) and never
/// overriding it.
public enum LaunchAtLoginPolicy {
    /// Applies `preference` to `service`. Idempotent: reapplying the same
    /// preference is ``LaunchAtLoginAction/noChange``.
    public static func reconcile(
        preference: Bool,
        service: any LoginItemService
    ) throws -> LaunchAtLoginAction {
        switch (preference, service.status) {
        case (true, .enabled):
            return .noChange
        case (true, .requiresApproval):
            // Registration is already on file; re-registering cannot grant
            // what only the user's approval can.
            return .awaitingApproval
        case (true, .notRegistered), (true, .notFound):
            try service.register()
            return service.status == .requiresApproval ? .awaitingApproval : .registered
        case (false, .enabled), (false, .requiresApproval):
            try service.unregister()
            return .unregistered
        case (false, .notRegistered), (false, .notFound):
            return .noChange
        }
    }
}

/// The product `SMAppService` adapter (macOS 13+; v1 targets 14+).
///
/// Constructible anywhere, but only meaningful when called from the signed
/// GramDrive app bundle whose `Contents/Library/LaunchAgents/<plistName>`
/// declares the agent — the platform resolves the plist against the
/// caller's bundle. Exercised by the app shell (TASK-260715-13pxnu), not
/// by unit tests.
public struct SMAppServiceAgentLoginItem: LoginItemService {
    /// The launchd property list name v1 ships for the agent, derived from
    /// the product namespace (DEC-019 / POL-7).
    public static let defaultPlistName = "com.reluxworks.gramdrive.agent.plist"

    private let service: SMAppService

    public init(plistName: String = SMAppServiceAgentLoginItem.defaultPlistName) {
        self.service = SMAppService.agent(plistName: plistName)
    }

    public var status: LoginItemStatus {
        switch service.status {
        case .enabled: return .enabled
        case .requiresApproval: return .requiresApproval
        case .notFound: return .notFound
        case .notRegistered: return .notRegistered
        @unknown default: return .notRegistered
        }
    }

    public func register() throws {
        try service.register()
    }

    public func unregister() throws {
        try service.unregister()
    }
}
