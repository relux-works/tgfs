import FileProvider
import Foundation
import GramDriveCore
import Testing

@testable import GramDriveFileProvider

private func account(id: Int64, name: String = "Ivan") -> AccountInfo {
    AccountInfo(
        accountId: id,
        sourceKind: .localTdlib,
        displayName: name,
        authState: "authorized",
        namespaceVersion: 1,
        displayTimezone: "UTC",
        rootItemId: "root-\(id)"
    )
}

private func registered(_ identifier: String, _ displayName: String = "GramDrive")
    -> RegisteredDomain
{
    RegisteredDomain(identifier: identifier, displayName: displayName)
}

private func accountDomain(_ id: Int64, _ displayName: String = "GramDrive") -> RegisteredDomain {
    registered(DomainIdentity.identifier(forAccountId: id), displayName)
}

private struct SeamFailure: Error {}

/// A double over the *one* system domain registry: it both registers
/// (upsert, like the platform's `add`) and removes, so a repair pass drives
/// a single coherent registered set. Failure injection fires on the Nth
/// mutating call and lands *before* the mutation, modelling a crash partway
/// through a pass; the surviving `domains` are the durable registry a re-run
/// converges from.
private actor FakeDomainRegistry: DomainRegistrar, DomainRemover {
    private(set) var domains: [RegisteredDomain]
    private(set) var registerCalls: [DesiredDomain] = []
    private(set) var removeCalls: [(domain: RegisteredDomain, disposition: DomainDataDisposition)] =
        []

    /// Fail on the Nth mutating call (1-based), before applying it. `nil`
    /// never fails.
    private var failAtMutation: Int?
    private var mutationCount = 0
    /// Where a preserved removal moves data to, keyed by identifier. Absent
    /// means the system kept nothing.
    private let preserved: [String: URL]

    init(
        domains: [RegisteredDomain] = [],
        failAtMutation: Int? = nil,
        preserved: [String: URL] = [:]
    ) {
        self.domains = domains
        self.failAtMutation = failAtMutation
        self.preserved = preserved
    }

    func clearFailure() {
        failAtMutation = nil
    }

    private func gate() throws {
        mutationCount += 1
        if let failAtMutation, mutationCount == failAtMutation {
            throw SeamFailure()
        }
    }

    func registeredDomains() async throws -> [RegisteredDomain] {
        domains
    }

    func register(_ domain: DesiredDomain) async throws {
        try gate()
        registerCalls.append(domain)
        let value = RegisteredDomain(
            identifier: domain.identifier,
            displayName: domain.displayName
        )
        if let index = domains.firstIndex(where: { $0.identifier == domain.identifier }) {
            domains[index] = value
        } else {
            domains.append(value)
        }
    }

    func remove(
        _ domain: RegisteredDomain,
        disposition: DomainDataDisposition
    ) async throws -> URL? {
        try gate()
        removeCalls.append((domain, disposition))
        domains.removeAll { $0.identifier == domain.identifier }
        guard disposition == .preserveDownloads else { return nil }
        return preserved[domain.identifier]
    }
}

@Suite("Domain data disposition")
struct DomainDataDispositionTests {
    @Test("Delete maps to the platform's remove-all mode")
    func deleteMapsToRemoveAll() {
        #expect(DomainDataDisposition.deleteLocalData.removalMode == .removeAll)
    }

    @Test("Preserve maps to the platform's keep-downloads mode")
    func preserveMapsToPreserveDownloads() {
        #expect(
            DomainDataDisposition.preserveDownloads.removalMode == .preserveDownloadedUserData
        )
    }
}

@Suite("Targeted domain removal")
struct DomainRemovalTests {
    @Test("Removing a registered account domain unregisters exactly it")
    func removesRegisteredDomain() async throws {
        let registry = FakeDomainRegistry(domains: [accountDomain(7), accountDomain(9)])
        let outcome = try await DomainRemoval.removeAccountDomain(
            accountId: 7,
            disposition: .deleteLocalData,
            registrar: registry,
            remover: registry
        )
        #expect(outcome.wasRegistered)
        #expect(outcome.identifier == "account-7")
        let removeCalls = await registry.removeCalls
        #expect(removeCalls.map(\.domain.identifier) == ["account-7"])
        let domains = await registry.domains
        #expect(domains.map(\.identifier) == ["account-9"], "only the target is removed")
    }

    @Test("Removing an account whose domain is not registered is a no-op success")
    func removingAbsentDomainIsNoOp() async throws {
        let registry = FakeDomainRegistry(domains: [accountDomain(9)])
        let outcome = try await DomainRemoval.removeAccountDomain(
            accountId: 7,
            disposition: .deleteLocalData,
            registrar: registry,
            remover: registry
        )
        #expect(!outcome.wasRegistered)
        #expect(outcome.preservedDataLocation == nil)
        let removeCalls = await registry.removeCalls
        #expect(removeCalls.isEmpty, "nothing to remove — the remover is never called")
        let domains = await registry.domains
        #expect(domains.map(\.identifier) == ["account-9"])
    }

    @Test("A second removal of the same account is idempotent — no second remover call")
    func removalIsIdempotent() async throws {
        let registry = FakeDomainRegistry(domains: [accountDomain(7)])
        _ = try await DomainRemoval.removeAccountDomain(
            accountId: 7,
            disposition: .deleteLocalData,
            registrar: registry,
            remover: registry
        )
        let second = try await DomainRemoval.removeAccountDomain(
            accountId: 7,
            disposition: .deleteLocalData,
            registrar: registry,
            remover: registry
        )
        #expect(!second.wasRegistered)
        let removeCalls = await registry.removeCalls
        #expect(removeCalls.count == 1, "the completed removal is not repeated")
    }

    @Test("Delete disposition preserves nothing")
    func deletePreservesNothing() async throws {
        let registry = FakeDomainRegistry(
            domains: [accountDomain(7)],
            preserved: ["account-7": URL(fileURLWithPath: "/tmp/kept")]
        )
        let outcome = try await DomainRemoval.removeAccountDomain(
            accountId: 7,
            disposition: .deleteLocalData,
            registrar: registry,
            remover: registry
        )
        #expect(outcome.preservedDataLocation == nil)
        let removeCalls = await registry.removeCalls
        #expect(removeCalls.first?.disposition == .deleteLocalData)
    }

    @Test("Preserve disposition surfaces where the kept downloads went")
    func preserveReturnsLocation() async throws {
        let keptURL = URL(fileURLWithPath: "/tmp/gramdrive-preserved/account-7")
        let registry = FakeDomainRegistry(
            domains: [accountDomain(7)],
            preserved: ["account-7": keptURL]
        )
        let outcome = try await DomainRemoval.removeAccountDomain(
            accountId: 7,
            disposition: .preserveDownloads,
            registrar: registry,
            remover: registry
        )
        #expect(outcome.wasRegistered)
        #expect(outcome.preservedDataLocation == keptURL)
        let removeCalls = await registry.removeCalls
        #expect(removeCalls.first?.disposition == .preserveDownloads)
    }

    @Test("A remover failure surfaces instead of reporting a false removal")
    func removerFailureSurfaces() async {
        let registry = FakeDomainRegistry(domains: [accountDomain(7)], failAtMutation: 1)
        await #expect(throws: SeamFailure.self) {
            _ = try await DomainRemoval.removeAccountDomain(
                accountId: 7,
                disposition: .deleteLocalData,
                registrar: registry,
                remover: registry
            )
        }
    }
}

@Suite("Domain repair")
struct DomainRepairTests {
    @Test("Repair re-registers an account's lost domain under its stable identity")
    func reRegistersLostDomain() async throws {
        // The account exists but its domain is gone — a crash or a system
        // that dropped the registration.
        let registry = FakeDomainRegistry(domains: [])
        let outcome = try await DomainRepair.repair(
            accounts: [account(id: 7)],
            registrar: registry,
            remover: registry
        )
        #expect(outcome.plan.adds.map(\.identifier) == ["account-7"])
        #expect(outcome.removedStrays.isEmpty)
        let domains = await registry.domains
        #expect(
            domains == [accountDomain(7, "GramDrive")],
            "re-adding the stable identifier recovers the account's domain"
        )
    }

    @Test("Repair removes strays no account explains, preserving their downloads")
    func removesStrays() async throws {
        let keptURL = URL(fileURLWithPath: "/tmp/preserved/account-99")
        let registry = FakeDomainRegistry(
            domains: [accountDomain(7), registered("account-99")],
            preserved: ["account-99": keptURL]
        )
        let outcome = try await DomainRepair.repair(
            accounts: [account(id: 7)],
            registrar: registry,
            remover: registry
        )
        #expect(outcome.removedStrays.map(\.identifier) == ["account-99"])
        #expect(outcome.removedStrays.first?.preservedDataLocation == keptURL)
        let domains = await registry.domains
        #expect(domains.map(\.identifier) == ["account-7"], "the stray is gone, the account stays")
        let removeCalls = await registry.removeCalls
        #expect(
            removeCalls.first?.disposition == .preserveDownloads,
            "stray cleanup keeps downloads by default — no data loss"
        )
    }

    @Test("Repair leaves a settled registered set untouched")
    func settledRepairDoesNothing() async throws {
        let registry = FakeDomainRegistry(domains: [accountDomain(7, "GramDrive")])
        let outcome = try await DomainRepair.repair(
            accounts: [account(id: 7)],
            registrar: registry,
            remover: registry
        )
        #expect(outcome.wasSettled)
        let registerCalls = await registry.registerCalls
        let removeCalls = await registry.removeCalls
        #expect(registerCalls.isEmpty)
        #expect(removeCalls.isEmpty)
    }

    @Test("A second repair pass is a settled no-op")
    func repairIsIdempotent() async throws {
        // The account's domain is missing and a stray is present, so the
        // first pass both adds and removes; the second must do neither.
        let registry = FakeDomainRegistry(domains: [registered("account-99")])
        let first = try await DomainRepair.repair(
            accounts: [account(id: 7)],
            registrar: registry,
            remover: registry
        )
        #expect(first.plan.adds.map(\.identifier) == ["account-7"])
        #expect(first.removedStrays.map(\.identifier) == ["account-99"])
        let second = try await DomainRepair.repair(
            accounts: [account(id: 7)],
            registrar: registry,
            remover: registry
        )
        #expect(second.wasSettled)
        let registerCalls = await registry.registerCalls
        let removeCalls = await registry.removeCalls
        #expect(registerCalls.count == 1, "the second pass registers nothing new")
        #expect(removeCalls.count == 1, "the second pass removes nothing new")
    }

    @Test("Interruption during re-registration recovers on the next pass")
    func interruptionDuringAddsRecovers() async throws {
        // Two accounts missing (two adds), plus a stray. Fail on the second
        // mutation — the second add — so the first add lands and the pass
        // dies before the stray removal.
        let registry = FakeDomainRegistry(
            domains: [registered("account-99")],
            failAtMutation: 2
        )
        await #expect(throws: SeamFailure.self) {
            _ = try await DomainRepair.repair(
                accounts: [account(id: 7, name: "A"), account(id: 9, name: "B")],
                registrar: registry,
                remover: registry
            )
        }
        // Partial: one account added, the stray untouched (removal comes
        // after all adds).
        let mid = await registry.domains
        #expect(Set(mid.map(\.identifier)) == ["account-7", "account-99"])

        await registry.clearFailure()
        let outcome = try await DomainRepair.repair(
            accounts: [account(id: 7, name: "A"), account(id: 9, name: "B")],
            registrar: registry,
            remover: registry
        )
        #expect(
            outcome.plan.adds.map(\.identifier) == ["account-9"], "only the missing add re-runs")
        #expect(outcome.removedStrays.map(\.identifier) == ["account-99"])
        let final = await registry.domains
        #expect(Set(final.map(\.identifier)) == ["account-7", "account-9"], "converged, no stray")
    }

    @Test("Interruption during stray removal recovers on the next pass")
    func interruptionDuringStrayRemovalRecovers() async throws {
        // One missing account (one add) and two strays. Adds run first, so
        // mutations are: (1) add account-7, (2) remove stray-98, (3) remove
        // stray-99. Fail on the third — one stray already gone, one left.
        let registry = FakeDomainRegistry(
            domains: [registered("account-98"), registered("account-99")],
            failAtMutation: 3
        )
        await #expect(throws: SeamFailure.self) {
            _ = try await DomainRepair.repair(
                accounts: [account(id: 7)],
                registrar: registry,
                remover: registry
            )
        }
        let mid = await registry.domains
        #expect(
            Set(mid.map(\.identifier)) == ["account-7", "account-99"],
            "account re-registered, first stray gone, second stray survives the crash"
        )

        await registry.clearFailure()
        let outcome = try await DomainRepair.repair(
            accounts: [account(id: 7)],
            registrar: registry,
            remover: registry
        )
        #expect(outcome.plan.adds.isEmpty, "the account is already registered")
        #expect(
            outcome.removedStrays.map(\.identifier) == ["account-99"], "only the leftover stray")
        let final = await registry.domains
        #expect(final.map(\.identifier) == ["account-7"], "converged, both strays gone")
    }

    @Test("A repair failure surfaces instead of reporting a completed pass")
    func repairFailureSurfaces() async {
        let registry = FakeDomainRegistry(domains: [accountDomain(7)], failAtMutation: 1)
        await #expect(throws: SeamFailure.self) {
            _ = try await DomainRepair.repair(
                accounts: [account(id: 7, name: "A"), account(id: 9, name: "B")],
                registrar: registry,
                remover: registry
            )
        }
    }

    @Test("Repair with no accounts and no domains is a settled no-op")
    func emptyRepairSettles() async throws {
        let registry = FakeDomainRegistry(domains: [])
        let outcome = try await DomainRepair.repair(
            accounts: [],
            registrar: registry,
            remover: registry
        )
        #expect(outcome.wasSettled)
        #expect(outcome.desired.isEmpty)
        #expect(!outcome.withheldTotalTeardown)
    }

    @Test("Repair refuses a total teardown: empty accounts leave every domain in place")
    func refusesTotalTeardown() async throws {
        // The exact spurious-empty vector: the canonical read returns zero
        // accounts (a normal, non-throwing answer) while two domains are still
        // registered — so every registered domain looks like a stray. The
        // default policy must withhold them all, removing nothing.
        let registry = FakeDomainRegistry(domains: [accountDomain(7), registered("account-99")])
        let outcome = try await DomainRepair.repair(
            accounts: [],
            registrar: registry,
            remover: registry
        )
        #expect(outcome.withheldTotalTeardown)
        #expect(!outcome.wasSettled)
        #expect(outcome.removedStrays.isEmpty)
        #expect(
            Set(outcome.withheldStrays.map(\.identifier)) == ["account-7", "account-99"],
            "every registered domain is withheld, none removed"
        )
        let removeCalls = await registry.removeCalls
        #expect(removeCalls.isEmpty, "a spurious-empty read must not remove a single domain")
        let domains = await registry.domains
        #expect(
            Set(domains.map(\.identifier)) == ["account-7", "account-99"],
            "the registered set survives untouched"
        )
    }

    @Test("An explicitly allowed total teardown removes every registered domain")
    func allowedTotalTeardownRemovesAll() async throws {
        let registry = FakeDomainRegistry(domains: [accountDomain(7), registered("account-99")])
        let outcome = try await DomainRepair.repair(
            accounts: [],
            registrar: registry,
            remover: registry,
            totalTeardown: .allow
        )
        #expect(!outcome.withheldTotalTeardown)
        #expect(
            Set(outcome.removedStrays.map(\.identifier)) == ["account-7", "account-99"],
            "the confirmed teardown removes everything"
        )
        let domains = await registry.domains
        #expect(domains.isEmpty)
    }

    @Test("The teardown guard is narrow: strays still clean when an account remains")
    func straysRemovedWhenAnAccountRemains() async throws {
        // Desired is non-empty (account 7 is configured), so the read is not a
        // spurious-empty one — the guard stays out of the way and the genuine
        // orphan is cleaned as usual.
        let registry = FakeDomainRegistry(domains: [accountDomain(7), registered("account-99")])
        let outcome = try await DomainRepair.repair(
            accounts: [account(id: 7)],
            registrar: registry,
            remover: registry
        )
        #expect(!outcome.withheldTotalTeardown)
        #expect(outcome.removedStrays.map(\.identifier) == ["account-99"])
        let domains = await registry.domains
        #expect(domains.map(\.identifier) == ["account-7"], "the account stays, the stray goes")
    }
}

@Suite("Domain repair over real shared state")
struct DomainRepairRunTests {
    /// A substitute container per test, removed afterwards — the shared-state
    /// tests' rule.
    private func withSubstituteDataRoot<T>(_ body: (URL) async throws -> T) async rethrows -> T {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-fp-repair-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: root) }
        return try await body(root)
    }

    @Test("An empty shared container repairs to a settled pass")
    func emptyContainer() async {
        await withSubstituteDataRoot { dataRoot in
            let registry = FakeDomainRegistry()
            let outcome = await DomainRepair.run(
                dataRoot: dataRoot,
                registrar: registry,
                remover: registry
            )
            guard case .repaired(let result) = outcome else {
                Issue.record("expected a repaired outcome, got \(outcome)")
                return
            }
            #expect(result.wasSettled)
            let registerCalls = await registry.registerCalls
            #expect(registerCalls.isEmpty)
        }
    }

    @Test("A stray with no account and a failing remover reports failed, not success")
    func failureReportsFailed() async {
        await withSubstituteDataRoot { dataRoot in
            // No accounts in the empty container, one registered stray, and a
            // remover that fails on its first call. An explicitly-allowed
            // teardown bypasses the total-teardown guard so the pass actually
            // reaches the remover — proving a remover failure surfaces as
            // `.failed`, not a false-success.
            let registry = FakeDomainRegistry(
                domains: [registered("account-99")],
                failAtMutation: 1
            )
            let outcome = await DomainRepair.run(
                dataRoot: dataRoot,
                totalTeardown: .allow,
                registrar: registry,
                remover: registry
            )
            guard case .failed = outcome else {
                Issue.record("expected a failed outcome, got \(outcome)")
                return
            }
        }
    }

    @Test("Repair over real shared state refuses a total teardown from an empty container")
    func runRefusesTotalTeardown() async {
        await withSubstituteDataRoot { dataRoot in
            // An empty container yields zero accounts (empty desired set) while
            // two domains are registered — the spurious-empty vector reaching
            // the app entry point. The default `run()` must withhold, not tear
            // everything down.
            let registry = FakeDomainRegistry(
                domains: [registered("account-7"), registered("account-99")]
            )
            let outcome = await DomainRepair.run(
                dataRoot: dataRoot,
                registrar: registry,
                remover: registry
            )
            guard case .repaired(let result) = outcome else {
                Issue.record("expected a repaired outcome, got \(outcome)")
                return
            }
            #expect(result.withheldTotalTeardown)
            let removeCalls = await registry.removeCalls
            #expect(removeCalls.isEmpty, "no domain removed from an empty container")
            let domains = await registry.domains
            #expect(domains.count == 2, "both domains survive")
        }
    }
}
