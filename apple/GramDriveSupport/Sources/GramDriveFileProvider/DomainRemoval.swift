import Foundation
import GramDriveCore
import GramDriveSupport

/// Targeted File Provider domain removal (TASK-260715-gnat2x).
///
/// The provider-registration step of the account-removal cleanup sequence
/// (SEC-004): logout and the trace-free on-disk wipe are the engine's, but
/// tearing down the *domain* — the Finder entry and everything the system
/// materialized for it — is the containing app's, because domain management
/// resolves the extension from the calling app's bundle (PLAT-MAC-001).
///
/// Two properties the whole flow rests on:
///
/// - **Idempotent.** The domain identifier is a pure function of the account
///   identity (``DomainIdentity``), so removal reads the registered set,
///   removes the one matching identifier if present, and reports a no-op when
///   it is already gone. Re-running a completed removal, or removing an
///   account whose domain never existed, both settle to `wasRegistered:
///   false` without touching anything.
/// - **Interruption-safe by ordering.** The canonical account rows are the
///   source of truth (PLAT-MAC-003); a domain should exist iff its account
///   row does. The safe removal order is therefore *drop the account row
///   first* (the engine's step), *then* remove the domain here. A crash
///   between the two leaves the domain a stray, which ``DomainRepair`` cleans
///   up — never a domain re-registered for an account that is gone. This
///   entry point works purely from the identifier, so it runs correctly
///   whether or not the row is still present.
public enum DomainRemoval {
    /// Removes the domain for one account identity, if it is registered.
    ///
    /// Reads the registered set through the registrar seam, then removes the
    /// domain whose identifier is ``DomainIdentity/identifier(forAccountId:)``
    /// through the remover seam. An absent domain is the idempotent success
    /// case, not an error.
    public static func removeAccountDomain(
        accountId: Int64,
        disposition: DomainDataDisposition,
        registrar: some DomainRegistrar,
        remover: some DomainRemover
    ) async throws -> DomainRemovalOutcome {
        let identifier = DomainIdentity.identifier(forAccountId: accountId)
        return try await remove(
            identifier: identifier,
            disposition: disposition,
            registrar: registrar,
            remover: remover
        )
    }

    /// Removes a domain by identifier, if it is registered. The building
    /// block behind ``removeAccountDomain(accountId:disposition:registrar:remover:)``
    /// and the way ``DomainRepair`` disposes of a single stray.
    public static func remove(
        identifier: String,
        disposition: DomainDataDisposition,
        registrar: some DomainRegistrar,
        remover: some DomainRemover
    ) async throws -> DomainRemovalOutcome {
        let registered = try await registrar.registeredDomains()
        guard let target = registered.first(where: { $0.identifier == identifier }) else {
            return DomainRemovalOutcome(
                identifier: identifier,
                wasRegistered: false,
                disposition: disposition,
                preservedDataLocation: nil
            )
        }
        let preserved = try await remover.remove(target, disposition: disposition)
        return DomainRemovalOutcome(
            identifier: identifier,
            wasRegistered: true,
            disposition: disposition,
            preservedDataLocation: preserved
        )
    }
}
