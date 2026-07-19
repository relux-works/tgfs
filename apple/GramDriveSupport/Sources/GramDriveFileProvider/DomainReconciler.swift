import Foundation
import GramDriveCore

/// What one reconcile pass decided, before anything is applied. A pure
/// diff of desired against registered — computing it mutates nothing.
public struct DomainReconcilePlan: Equatable, Sendable {
    /// Desired domains the system does not know yet.
    public var adds: [DesiredDomain]
    /// Desired domains registered under the right identifier but showing
    /// a stale display name (re-registered to rename).
    public var renames: [DesiredDomain]
    /// Desired domains already registered exactly as desired — untouched,
    /// which is what makes a repeated pass a no-op.
    public var keeps: [DesiredDomain]
    /// Registered domains no configured account explains. Reported, never
    /// touched: removal and repair are owned by TASK-260715-gnat2x, and
    /// registration destroying Finder state is the failure mode this
    /// split exists to prevent.
    public var strays: [RegisteredDomain]

    /// Whether the pass has nothing to apply.
    public var isSettled: Bool { adds.isEmpty && renames.isEmpty }

    public init(
        adds: [DesiredDomain] = [],
        renames: [DesiredDomain] = [],
        keeps: [DesiredDomain] = [],
        strays: [RegisteredDomain] = []
    ) {
        self.adds = adds
        self.renames = renames
        self.keeps = keeps
        self.strays = strays
    }
}

/// One applied reconcile pass: the desired set it worked from and the
/// plan it applied.
public struct DomainReconcileOutcome: Equatable, Sendable {
    public let desired: [DesiredDomain]
    public let plan: DomainReconcilePlan

    public init(desired: [DesiredDomain], plan: DomainReconcilePlan) {
        self.desired = desired
        self.plan = plan
    }
}

/// Idempotent File Provider domain reconciliation (TASK-260715-3s44pc).
///
/// The contract behind every acceptance path — first run, restart,
/// duplicate install, reauthorization, multiple accounts — is one rule:
/// *converge the registered set toward the desired set, touching only
/// what differs.* Identifiers are stable (``DomainIdentity``), `register`
/// upserts, and nothing here removes, so running the pass any number of
/// times, from any number of concurrently installed copies, lands in the
/// same state: each account's domain appears exactly once.
public enum DomainReconciler {
    /// Diffs desired domains against the system's registered set. Pure.
    public static func plan(
        desired: [DesiredDomain],
        registered: [RegisteredDomain]
    ) -> DomainReconcilePlan {
        let byIdentifier = Dictionary(
            registered.map { ($0.identifier, $0) },
            uniquingKeysWith: { first, _ in first }
        )
        var plan = DomainReconcilePlan()
        for domain in desired {
            guard let existing = byIdentifier[domain.identifier] else {
                plan.adds.append(domain)
                continue
            }
            if existing.displayName == domain.displayName {
                plan.keeps.append(domain)
            } else {
                plan.renames.append(domain)
            }
        }
        let desiredIdentifiers = Set(desired.map(\.identifier))
        plan.strays = registered.filter { !desiredIdentifiers.contains($0.identifier) }
        return plan
    }

    /// Reconciles the registered domains toward the accounts' desired
    /// set: reads the registered domains, applies adds and renames
    /// through the registrar, and reports what happened. Keeps and strays
    /// are never touched.
    public static func reconcile(
        accounts: [AccountInfo],
        using registrar: some DomainRegistrar
    ) async throws -> DomainReconcileOutcome {
        let desired = DomainIdentity.desiredDomains(for: accounts)
        let registered = try await registrar.registeredDomains()
        let plan = Self.plan(desired: desired, registered: registered)
        for domain in plan.adds + plan.renames {
            try await registrar.register(domain)
        }
        return DomainReconcileOutcome(desired: desired, plan: plan)
    }

    /// The shared-state entry point: the desired set comes from the
    /// durable account rows every GramDrive process agrees on
    /// (PLAT-MAC-003). The store read is synchronous and touches disk —
    /// call from off the main thread, as with every shared-state read.
    public static func reconcile(
        store: SharedStateStore,
        using registrar: some DomainRegistrar
    ) async throws -> DomainReconcileOutcome {
        try await reconcile(accounts: store.accounts(), using: registrar)
    }
}
