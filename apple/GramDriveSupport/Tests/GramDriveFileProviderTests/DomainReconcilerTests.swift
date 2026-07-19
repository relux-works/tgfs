import Foundation
import GramDriveCore
import Testing

@testable import GramDriveFileProvider

private func account(id: Int64, name: String = "Ivan", authState: String = "authorized")
    -> AccountInfo
{
    AccountInfo(
        accountId: id,
        sourceKind: .localTdlib,
        displayName: name,
        authState: authState,
        namespaceVersion: 1,
        rootItemId: "root-\(id)"
    )
}

private func desired(id: Int64, name: String) -> DesiredDomain {
    DesiredDomain(
        accountId: id,
        identifier: DomainIdentity.identifier(forAccountId: id),
        displayName: name
    )
}

/// A registrar double over an in-memory domain set: applies upsert
/// semantics like the platform's `add`, records every mutation, and can
/// be told to fail.
private actor FakeRegistrar: DomainRegistrar {
    private(set) var domains: [RegisteredDomain]
    private(set) var registerCalls: [DesiredDomain] = []
    private let failure: Error?

    init(domains: [RegisteredDomain] = [], failure: Error? = nil) {
        self.domains = domains
        self.failure = failure
    }

    func registeredDomains() async throws -> [RegisteredDomain] {
        if let failure { throw failure }
        return domains
    }

    func register(_ domain: DesiredDomain) async throws {
        if let failure { throw failure }
        registerCalls.append(domain)
        let registered = RegisteredDomain(
            identifier: domain.identifier,
            displayName: domain.displayName
        )
        if let index = domains.firstIndex(where: { $0.identifier == domain.identifier }) {
            domains[index] = registered
        } else {
            domains.append(registered)
        }
    }
}

private struct RegistrarFailure: Error {}

@Suite("Domain reconcile plan")
struct DomainReconcilePlanTests {
    @Test("First run adds every desired domain")
    func firstRunAdds() {
        let plan = DomainReconciler.plan(
            desired: [desired(id: 7, name: "GramDrive")],
            registered: []
        )
        #expect(plan.adds == [desired(id: 7, name: "GramDrive")])
        #expect(plan.renames.isEmpty)
        #expect(plan.keeps.isEmpty)
        #expect(plan.strays.isEmpty)
        #expect(!plan.isSettled)
    }

    @Test("An exactly-registered set plans nothing — the pass is settled")
    func settledPlan() {
        let plan = DomainReconciler.plan(
            desired: [desired(id: 7, name: "GramDrive")],
            registered: [RegisteredDomain(identifier: "account-7", displayName: "GramDrive")]
        )
        #expect(plan.isSettled)
        #expect(plan.keeps == [desired(id: 7, name: "GramDrive")])
    }

    @Test("A stale display name plans a rename, not a second domain")
    func stalePlanRenames() {
        let plan = DomainReconciler.plan(
            desired: [desired(id: 7, name: "GramDrive — Ivan")],
            registered: [RegisteredDomain(identifier: "account-7", displayName: "GramDrive")]
        )
        #expect(plan.adds.isEmpty)
        #expect(plan.renames == [desired(id: 7, name: "GramDrive — Ivan")])
    }

    @Test("Registered domains no account explains are reported as strays, never planned away")
    func straysAreReportedOnly() {
        let stray = RegisteredDomain(identifier: "account-99", displayName: "GramDrive")
        let plan = DomainReconciler.plan(
            desired: [desired(id: 7, name: "GramDrive")],
            registered: [stray]
        )
        #expect(plan.strays == [stray])
        #expect(plan.adds == [desired(id: 7, name: "GramDrive")])
    }
}

@Suite("Domain reconciliation")
struct DomainReconcilerTests {
    @Test("First run registers one domain per account, exactly once")
    func firstRun() async throws {
        let registrar = FakeRegistrar()
        let outcome = try await DomainReconciler.reconcile(
            accounts: [account(id: 7)],
            using: registrar
        )
        #expect(outcome.plan.adds.count == 1)
        let calls = await registrar.registerCalls
        #expect(calls == [desired(id: 7, name: "GramDrive")])
        let domains = await registrar.domains
        #expect(domains == [RegisteredDomain(identifier: "account-7", displayName: "GramDrive")])
    }

    @Test("A repeated pass is a no-op — restart and duplicate install cannot double a domain")
    func repeatedPassIsIdempotent() async throws {
        let registrar = FakeRegistrar()
        _ = try await DomainReconciler.reconcile(accounts: [account(id: 7)], using: registrar)
        let second = try await DomainReconciler.reconcile(
            accounts: [account(id: 7)],
            using: registrar
        )
        #expect(second.plan.isSettled)
        let calls = await registrar.registerCalls
        #expect(calls.count == 1, "the second pass must not touch the registrar")
        let domains = await registrar.domains
        #expect(domains.count == 1)
    }

    @Test("Reauthorization changes no domain — same identity, same name, no calls")
    func reauthorizationIsInvisible() async throws {
        let registrar = FakeRegistrar()
        _ = try await DomainReconciler.reconcile(
            accounts: [account(id: 7, authState: "authorized")],
            using: registrar
        )
        let after = try await DomainReconciler.reconcile(
            accounts: [account(id: 7, authState: "waiting_code")],
            using: registrar
        )
        #expect(after.plan.isSettled)
        let calls = await registrar.registerCalls
        #expect(calls.count == 1)
    }

    @Test("A second account renames the first domain and adds the second")
    func secondAccountArrives() async throws {
        let registrar = FakeRegistrar()
        _ = try await DomainReconciler.reconcile(accounts: [account(id: 7)], using: registrar)
        let outcome = try await DomainReconciler.reconcile(
            accounts: [account(id: 7, name: "Ivan"), account(id: 9, name: "Work")],
            using: registrar
        )
        #expect(outcome.plan.renames.map(\.identifier) == ["account-7"])
        #expect(outcome.plan.adds.map(\.identifier) == ["account-9"])
        let domains = await registrar.domains
        #expect(
            Set(domains) == Set([
                RegisteredDomain(identifier: "account-7", displayName: "GramDrive — Ivan"),
                RegisteredDomain(identifier: "account-9", displayName: "GramDrive — Work"),
            ])
        )
    }

    @Test("An account rename lands as a display-name update under the same identifier")
    func accountRename() async throws {
        let registrar = FakeRegistrar()
        _ = try await DomainReconciler.reconcile(
            accounts: [account(id: 7, name: "Ivan"), account(id: 9, name: "Work")],
            using: registrar
        )
        let outcome = try await DomainReconciler.reconcile(
            accounts: [account(id: 7, name: "Renamed"), account(id: 9, name: "Work")],
            using: registrar
        )
        #expect(outcome.plan.renames.map(\.displayName) == ["GramDrive — Renamed"])
        #expect(outcome.plan.keeps.map(\.identifier) == ["account-9"])
        let domains = await registrar.domains
        #expect(
            domains.contains(
                RegisteredDomain(identifier: "account-7", displayName: "GramDrive — Renamed")
            )
        )
    }

    @Test("Strays survive a pass untouched — removal belongs to the removal task")
    func straysSurvive() async throws {
        let stray = RegisteredDomain(identifier: "account-99", displayName: "GramDrive")
        let registrar = FakeRegistrar(domains: [stray])
        let outcome = try await DomainReconciler.reconcile(
            accounts: [account(id: 7)],
            using: registrar
        )
        #expect(outcome.plan.strays == [stray])
        let domains = await registrar.domains
        #expect(domains.contains(stray))
    }

    @Test("A registrar failure surfaces instead of pretending the pass ran")
    func registrarFailureSurfaces() async {
        let registrar = FakeRegistrar(failure: RegistrarFailure())
        await #expect(throws: RegistrarFailure.self) {
            _ = try await DomainReconciler.reconcile(accounts: [account(id: 7)], using: registrar)
        }
    }

    @Test("No accounts reconciles to an empty, settled plan")
    func noAccounts() async throws {
        let registrar = FakeRegistrar()
        let outcome = try await DomainReconciler.reconcile(accounts: [], using: registrar)
        #expect(outcome.plan.isSettled)
        #expect(outcome.desired.isEmpty)
        let calls = await registrar.registerCalls
        #expect(calls.isEmpty)
    }
}

@Suite("Startup reconcile over real shared state")
struct DomainStartupReconcileTests {
    /// A substitute container per test, removed afterwards — the same
    /// rule the shared-state tests use.
    private func withSubstituteDataRoot<T>(_ body: (URL) async throws -> T) async rethrows -> T {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-fp-tests-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: root) }
        return try await body(root)
    }

    @Test("An empty shared container reconciles to a settled empty pass")
    func emptyContainer() async {
        await withSubstituteDataRoot { dataRoot in
            let registrar = FakeRegistrar()
            let outcome = await DomainStartupReconcile.run(
                dataRoot: dataRoot,
                registrar: registrar
            )
            guard case .reconciled(let result) = outcome else {
                Issue.record("expected a reconciled outcome, got \(outcome)")
                return
            }
            #expect(result.desired.isEmpty)
            #expect(result.plan.isSettled)
            let calls = await registrar.registerCalls
            #expect(calls.isEmpty)
        }
    }

    @Test("A registrar failure reports failed, not a crash and not success")
    func registrarFailure() async {
        await withSubstituteDataRoot { dataRoot in
            let outcome = await DomainStartupReconcile.run(
                dataRoot: dataRoot,
                registrar: FakeRegistrar(failure: RegistrarFailure())
            )
            guard case .failed = outcome else {
                Issue.record("expected a failed outcome, got \(outcome)")
                return
            }
        }
    }
}
