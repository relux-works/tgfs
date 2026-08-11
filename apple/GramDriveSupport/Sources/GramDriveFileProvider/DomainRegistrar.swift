import FileProvider
import Foundation

/// One domain as the system reports it — the facts the reconciler
/// compares against, nothing more.
public struct RegisteredDomain: Equatable, Hashable, Sendable {
    /// The system's domain identifier (raw value).
    public let identifier: String
    /// The user-visible name the system currently shows.
    public let displayName: String

    public init(identifier: String, displayName: String) {
        self.identifier = identifier
        self.displayName = displayName
    }
}

/// The narrow seam between the reconciler and the platform's domain
/// registry, so reconciliation logic is testable without entitlements or
/// an installed extension.
///
/// Deliberately has no removal operation: domain removal and stale-domain
/// repair are owned by the removal task (TASK-260715-gnat2x), and a seam
/// without `remove` makes "registration never destroys Finder state"
/// structural rather than disciplined.
public protocol DomainRegistrar: Sendable {
    /// Every currently registered domain of this app's provider.
    func registeredDomains() async throws -> [RegisteredDomain]
    /// Registers the domain, or updates its display name when a domain
    /// with the same identifier already exists (the platform's `add` has
    /// upsert semantics — re-adding is how a rename lands).
    func register(_ domain: DesiredDomain) async throws
}

/// The live registrar over `NSFileProviderManager`.
///
/// Platform constraint (PLAT-MAC-001): domain management resolves the
/// File Provider extension from the *calling app's* bundle, so this must
/// run inside the app that embeds the extension — the companion shell —
/// and is proven live by the signing/packaging task (TASK-260715-1dk9ik),
/// not by unit tests. Everything above this seam is tested against fakes.
public struct SystemDomainRegistrar: DomainRegistrar {
    public init() {}

    private func platformDomain(_ domain: DesiredDomain) -> NSFileProviderDomain {
        NSFileProviderDomain(
            identifier: NSFileProviderDomainIdentifier(rawValue: domain.identifier),
            displayName: domain.displayName
        )
    }

    public func registeredDomains() async throws -> [RegisteredDomain] {
        try await NSFileProviderManager.domains().map { domain in
            RegisteredDomain(
                identifier: domain.identifier.rawValue,
                displayName: domain.displayName
            )
        }
    }

    public func register(_ domain: DesiredDomain) async throws {
        try await NSFileProviderManager.add(platformDomain(domain))
    }

    /// Resolves the system's user-visible root for one registered domain.
    /// The containing app uses this after reconciliation so onboarding only
    /// reports success when its Finder action targets the actual drive root.
    public func userVisibleRootURL(for domain: DesiredDomain) async throws -> URL {
        guard let manager = NSFileProviderManager(for: platformDomain(domain)) else {
            throw SystemDomainRegistrarError.managerUnavailable
        }
        return try await manager.getUserVisibleURL(for: .rootContainer)
    }

    /// Manager used by the containing app's durable-state change relay.
    public func changeSignaler(for domain: DesiredDomain) -> (any ProviderChangeSignaling)? {
        NSFileProviderManager(for: platformDomain(domain))
    }
}

public enum SystemDomainRegistrarError: Error, Equatable, Sendable {
    case managerUnavailable
}
