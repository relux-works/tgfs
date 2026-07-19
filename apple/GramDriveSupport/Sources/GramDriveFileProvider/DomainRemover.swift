import FileProvider
import Foundation

/// What happens to a domain's local (materialized) data when its domain is
/// removed — the "preserve or delete per explicit user choice" decision
/// (PLAT-MAC-004; SEC-004), narrowed to the two dispositions a read-only V1
/// (PLAT-MAC-004 read scope) can actually offer.
///
/// The third platform mode, `preserveDirtyUserData`, is deliberately absent:
/// with no local edits to upload, there is never dirty user data to keep, so
/// modelling it would be a state that cannot occur.
public enum DomainDataDisposition: Equatable, Sendable {
    /// Delete the domain and everything the system materialized for it — the
    /// trace-free wipe a full account removal wants (SEC-004). Maps to the
    /// platform's default `removeAll`.
    case deleteLocalData
    /// Keep the user's downloaded files: the system moves them out of the
    /// provider and hands back the location they now live at, so removing the
    /// account never destroys files the user already has on disk. Maps to
    /// `preserveDownloadedUserData`.
    case preserveDownloads
}

/// The outcome of removing one domain.
public struct DomainRemovalOutcome: Equatable, Sendable {
    /// The domain identifier the removal targeted.
    public let identifier: String
    /// Whether the domain was actually registered when the removal ran.
    /// `false` is the idempotent no-op case — re-running a completed
    /// removal, or removing an account whose domain never existed.
    public let wasRegistered: Bool
    /// The disposition applied. Meaningful only when ``wasRegistered``.
    public let disposition: DomainDataDisposition
    /// Where the system moved preserved downloads to, when the disposition
    /// was ``DomainDataDisposition/preserveDownloads`` and something was
    /// preserved. `nil` for a delete, an idempotent no-op, or when the
    /// system kept nothing. The URL the UI surfaces so the user can find
    /// their retained files.
    public let preservedDataLocation: URL?

    public init(
        identifier: String,
        wasRegistered: Bool,
        disposition: DomainDataDisposition,
        preservedDataLocation: URL?
    ) {
        self.identifier = identifier
        self.wasRegistered = wasRegistered
        self.disposition = disposition
        self.preservedDataLocation = preservedDataLocation
    }
}

/// The narrow seam that *removes* a File Provider domain — deliberately
/// separate from ``DomainRegistrar``.
///
/// The registrar seam has no remove operation, which makes "registration
/// never destroys Finder state" structural rather than disciplined
/// (``DomainReconciler`` and ``DomainStartupReconcile`` can only add and
/// rename). Removal is a distinct capability, handed only to the explicit
/// removal (``DomainRemoval``) and repair (``DomainRepair``) flows — the
/// places a user or operator has asked for a domain to go away.
public protocol DomainRemover: Sendable {
    /// Removes a registered domain, disposing of its local data per the
    /// caller's explicit choice. Returns the location the system moved
    /// preserved downloads to, or `nil` when nothing was preserved.
    ///
    /// The `domain` carries the identifier the system matches on plus the
    /// display name needed to reconstruct the platform object; callers pass
    /// the ``RegisteredDomain`` they read back, so the value always matches
    /// what the system holds.
    func remove(
        _ domain: RegisteredDomain,
        disposition: DomainDataDisposition
    ) async throws -> URL?
}

/// The live remover over `NSFileProviderManager`.
///
/// Same platform constraint as ``SystemDomainRegistrar`` (PLAT-MAC-001):
/// domain management resolves the extension from the *calling app's* bundle,
/// so this runs inside the companion shell and is proven live by the
/// signing/packaging task (TASK-260715-1dk9ik), not by unit tests.
/// Everything above the seam is tested against fakes.
public struct SystemDomainRemover: DomainRemover {
    public init() {}

    public func remove(
        _ domain: RegisteredDomain,
        disposition: DomainDataDisposition
    ) async throws -> URL? {
        try await NSFileProviderManager.remove(
            NSFileProviderDomain(
                identifier: NSFileProviderDomainIdentifier(rawValue: domain.identifier),
                displayName: domain.displayName
            ),
            mode: disposition.removalMode
        )
    }
}

extension DomainDataDisposition {
    /// The platform removal mode this disposition maps to.
    var removalMode: NSFileProviderManager.DomainRemovalMode {
        switch self {
        case .deleteLocalData: return .removeAll
        case .preserveDownloads: return .preserveDownloadedUserData
        }
    }
}
