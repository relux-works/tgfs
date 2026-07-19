import Foundation
import GramDriveCore
import GramDriveSupport

/// The launch-time reconcile pass of the containing app
/// (TASK-260715-3s44pc): on every start, converge the registered domains
/// toward the configured accounts. Registration is durable in the
/// system, so this pass is what "recovers after app/provider restart"
/// means — a healthy install re-runs it into a no-op, a fresh or repaired
/// install re-registers what the account rows say should exist.
public enum DomainStartupReconcile {
    /// What a startup pass amounted to. Never throws: startup must not
    /// fail because domain reconciliation could not run — the outcome is
    /// for the host's log and the next pass.
    public enum Outcome {
        /// The environment has no shared container to reconcile from
        /// (unsigned dev run without the App Group entitlement).
        case skipped(reason: String)
        /// The pass ran; the outcome says what it applied.
        case reconciled(DomainReconcileOutcome)
        /// The pass could not complete (shared state unavailable, or the
        /// platform refused — no embedded extension, denied registration).
        case failed(reason: String)
    }

    /// Runs one reconcile pass against an explicit data root — the
    /// testable form, and the building block of ``run(registrar:)``.
    public static func run(
        dataRoot: URL,
        registrar: some DomainRegistrar
    ) async -> Outcome {
        do {
            let store = try SharedState.open(dataRoot: dataRoot, role: .provider)
            return .reconciled(try await DomainReconciler.reconcile(store: store, using: registrar))
        } catch {
            return .failed(reason: String(describing: error))
        }
    }

    /// Runs one reconcile pass in the App Group container with the live
    /// registrar — what the companion app calls at launch, off the main
    /// thread. Skips when the container cannot be resolved at all.
    public static func run(
        registrar: some DomainRegistrar = SystemDomainRegistrar()
    ) async -> Outcome {
        let container: URL
        do {
            container = try AppGroup.containerURL()
        } catch {
            return .skipped(reason: String(describing: error))
        }
        return await run(
            dataRoot: AppGroup.dataRootURL(containerURL: container),
            registrar: registrar
        )
    }
}
