import Foundation
import GramDriveCore
import GramDriveSupport

/// One stray a repair pass removed: the orphan domain identifier and where
/// the system moved its preserved downloads to (when the disposition kept
/// them).
public struct DomainStrayRemoval: Equatable, Sendable {
    public let identifier: String
    public let displayName: String
    public let preservedDataLocation: URL?

    public init(identifier: String, displayName: String, preservedDataLocation: URL?) {
        self.identifier = identifier
        self.displayName = displayName
        self.preservedDataLocation = preservedDataLocation
    }
}

/// What one applied repair pass amounted to: the desired set it worked from,
/// the plan it converged, and the strays it removed.
public struct DomainRepairOutcome: Equatable, Sendable {
    /// The desired domains derived from the canonical account rows.
    public let desired: [DesiredDomain]
    /// The reconcile plan (adds / renames / keeps / strays) this pass applied.
    public let plan: DomainReconcilePlan
    /// The strays this pass removed, in the order they were removed.
    public let removedStrays: [DomainStrayRemoval]
    /// Strays the total-teardown guard withheld: non-empty only when the
    /// desired set was empty while domains were still registered and the
    /// pass refused to remove them (see ``TotalTeardownPolicy``). Empty on
    /// every ordinary pass.
    public let withheldStrays: [RegisteredDomain]

    public init(
        desired: [DesiredDomain],
        plan: DomainReconcilePlan,
        removedStrays: [DomainStrayRemoval],
        withheldStrays: [RegisteredDomain] = []
    ) {
        self.desired = desired
        self.plan = plan
        self.removedStrays = removedStrays
        self.withheldStrays = withheldStrays
    }

    /// Whether the repair found nothing to do — the registered set already
    /// equalled the desired set, with no strays.
    public var wasSettled: Bool { plan.isSettled && plan.strays.isEmpty }

    /// Whether this pass refused a total teardown — an empty desired set
    /// against a non-empty registered set — and left every domain in place.
    public var withheldTotalTeardown: Bool { !withheldStrays.isEmpty }
}

/// How a repair pass treats the *total-teardown* case: an empty desired set
/// (no configured accounts) against a non-empty registered set, where every
/// registered domain is therefore a stray.
///
/// An empty account list is a normal, non-throwing answer from the canonical
/// store (`shared_state.rs`: `accounts()`), so it is indistinguishable from a
/// *spurious-empty* read — a genuinely-empty-but-present database while domains
/// are still registered (an App Group id change across an upgrade, an
/// externally reset state dir). Removing *every* domain on that signal would
/// destroy Finder state the account rows never actually disowned, and
/// fail-closed-on-throw does not catch it because empty does not throw. So the
/// default refuses.
public enum TotalTeardownPolicy: Equatable, Sendable {
    /// Refuse the teardown: withhold every stray removal and leave the
    /// registered set intact (the outcome flags it via
    /// ``DomainRepairOutcome/withheldTotalTeardown``). The genuine
    /// last-account logout removes its domain through the targeted
    /// ``DomainRemoval`` flow, driven by the actual logout — not by repair
    /// inferring teardown from emptiness.
    case refuse
    /// Proceed even when it means removing every registered domain — only for
    /// a teardown the user has explicitly confirmed.
    case allow
}

/// User-triggered File Provider domain repair (TASK-260715-gnat2x;
/// SYNC-070/SYNC-071).
///
/// Repair is ``DomainReconciler`` plus the one thing the reconciler refuses
/// to do: resolve strays. It rebuilds the registered set from the canonical
/// account rows —
///
/// - **Re-registers** every account's domain the system has lost (a crash,
///   an interrupted install, a system that dropped the registration). Because
///   the identifier is stable, re-adding it *recovers* the account's existing
///   Finder state (materialized files, pins) rather than creating a fresh
///   empty domain — that is the "rebuilds provider state without data loss"
///   guarantee (SYNC-071). Repair never removes-and-re-adds a live account's
///   domain, which is the operation that *would* lose data.
/// - **Removes strays** — domains no account row explains: the leftover of an
///   interrupted removal, a corrupt registration, or a foreign domain. With
///   the default ``DomainDataDisposition/preserveDownloads`` disposition, the
///   orphan's downloaded files are moved aside rather than deleted, so even
///   stray cleanup loses no data.
///
/// This is why repair is user-triggered and the startup reconcile
/// (``DomainStartupReconcile``) is add-only: automatically destroying Finder
/// state on every launch is the failure mode the split guards against, but an
/// explicit repair the user asked for is allowed to clean orphans. The launch
/// path runs the add-only ``DomainStartupReconcile``; this repair runs only
/// from the explicit "repair File Provider domains" user action (SYNC-071).
///
/// Even under that explicit trigger, the *total-teardown* case is guarded: an
/// empty desired set makes every registered domain a stray, which a
/// spurious-empty canonical read produces just as a genuine last-account
/// logout would. So repair refuses to remove them all by default
/// (``TotalTeardownPolicy``); only an explicitly-confirmed teardown proceeds.
///
/// Both properties survive interruption. Adds/renames run before stray
/// removal, so a crash mid-pass leaves a registered set that a re-run
/// converges from either side — a partially-applied repair re-runs into a
/// completed one, and a completed one re-runs into a settled no-op.
public enum DomainRepair {
    /// Reconciles the registered domains toward the accounts' desired set and
    /// removes strays. Pure inputs, explicit seams — the testable core.
    ///
    /// Applies adds and renames through the registrar (upserts; no data
    /// loss), then removes each stray through the remover with
    /// `strayDisposition`. Keeps are never touched. When the desired set is
    /// empty and domains are still registered, `totalTeardown` decides whether
    /// to refuse the mass removal (the default) or proceed.
    public static func repair(
        accounts: [AccountInfo],
        registrar: some DomainRegistrar,
        remover: some DomainRemover,
        strayDisposition: DomainDataDisposition = .preserveDownloads,
        totalTeardown: TotalTeardownPolicy = .refuse
    ) async throws -> DomainRepairOutcome {
        let desired = DomainIdentity.desiredDomains(for: accounts)
        let registered = try await registrar.registeredDomains()
        let plan = DomainReconciler.plan(desired: desired, registered: registered)

        // Recover the accounts' own domains first, so an interruption never
        // leaves a real account without its domain while orphans are cleaned.
        // (When the desired set is empty there is nothing to add here.)
        for domain in plan.adds + plan.renames {
            try await registrar.register(domain)
        }

        // Total-teardown guard (see TotalTeardownPolicy): an empty desired set
        // makes every registered domain a stray, which a spurious-empty
        // canonical read produces just like a genuine last-account logout.
        // Unless the teardown is explicitly allowed, withhold the removals and
        // leave every domain in place — the outcome flags it so the caller can
        // surface it.
        if desired.isEmpty, !plan.strays.isEmpty, totalTeardown == .refuse {
            return DomainRepairOutcome(
                desired: desired,
                plan: plan,
                removedStrays: [],
                withheldStrays: plan.strays
            )
        }

        var removedStrays: [DomainStrayRemoval] = []
        for stray in plan.strays {
            let preserved = try await remover.remove(stray, disposition: strayDisposition)
            removedStrays.append(
                DomainStrayRemoval(
                    identifier: stray.identifier,
                    displayName: stray.displayName,
                    preservedDataLocation: preserved
                )
            )
        }

        return DomainRepairOutcome(desired: desired, plan: plan, removedStrays: removedStrays)
    }

    /// The shared-state entry point: the desired set comes from the durable
    /// account rows every GramDrive process agrees on (PLAT-MAC-003). The
    /// store read is synchronous and touches disk — call off the main thread.
    public static func repair(
        store: SharedStateStore,
        registrar: some DomainRegistrar,
        remover: some DomainRemover,
        strayDisposition: DomainDataDisposition = .preserveDownloads,
        totalTeardown: TotalTeardownPolicy = .refuse
    ) async throws -> DomainRepairOutcome {
        try await repair(
            accounts: store.accounts(),
            registrar: registrar,
            remover: remover,
            strayDisposition: strayDisposition,
            totalTeardown: totalTeardown
        )
    }
}

extension DomainRepair {
    /// What a repair pass amounted to, in the never-throwing shape the app's
    /// maintenance action logs and renders — the same rule as
    /// ``DomainStartupReconcile/Outcome``: repair is a recovery action, so a
    /// failure to run is reported, never a crash.
    public enum Outcome {
        /// No shared container to repair from (unsigned dev run without the
        /// App Group entitlement).
        case skipped(reason: String)
        /// The pass ran; the outcome says what it applied and removed.
        case repaired(DomainRepairOutcome)
        /// The pass could not complete (shared state unavailable, or the
        /// platform refused the registration/removal).
        case failed(reason: String)
    }

    /// Runs one repair pass against an explicit data root — the testable
    /// form, and the building block of ``run(strayDisposition:registrar:remover:)``.
    public static func run(
        dataRoot: URL,
        strayDisposition: DomainDataDisposition = .preserveDownloads,
        totalTeardown: TotalTeardownPolicy = .refuse,
        registrar: some DomainRegistrar,
        remover: some DomainRemover
    ) async -> Outcome {
        do {
            let store = try SharedState.open(dataRoot: dataRoot, role: .provider)
            let outcome = try await repair(
                store: store,
                registrar: registrar,
                remover: remover,
                strayDisposition: strayDisposition,
                totalTeardown: totalTeardown
            )
            return .repaired(outcome)
        } catch {
            return .failed(reason: String(describing: error))
        }
    }

    /// Runs one repair pass in the App Group container with the live
    /// registrar and remover — the companion app's user-triggered "repair
    /// File Provider domains" action (SYNC-071), off the main thread. Never
    /// runs at launch. Skips when the container cannot be resolved at all, and
    /// refuses a total teardown by default (`totalTeardown`).
    public static func run(
        strayDisposition: DomainDataDisposition = .preserveDownloads,
        totalTeardown: TotalTeardownPolicy = .refuse,
        registrar: some DomainRegistrar = SystemDomainRegistrar(),
        remover: some DomainRemover = SystemDomainRemover()
    ) async -> Outcome {
        let container: URL
        do {
            container = try AppGroup.containerURL()
        } catch {
            return .skipped(reason: String(describing: error))
        }
        return await run(
            dataRoot: AppGroup.dataRootURL(containerURL: container),
            strayDisposition: strayDisposition,
            totalTeardown: totalTeardown,
            registrar: registrar,
            remover: remover
        )
    }
}
