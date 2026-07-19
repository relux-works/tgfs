import GramDriveCore
import Testing

@testable import GramDriveFileProvider

/// A configured-account fixture with the fields identity derivation
/// reads; everything else is representative filler.
private func account(
    id: Int64,
    name: String = "Ivan",
    authState: String = "authorized"
) -> AccountInfo {
    AccountInfo(
        accountId: id,
        sourceKind: .localTdlib,
        displayName: name,
        authState: authState,
        namespaceVersion: 1,
        rootItemId: "root-\(id)"
    )
}

@Suite("Domain identity rule")
struct DomainIdentityTests {
    @Test("The identifier is a stable pure function of the account id")
    func identifierIsStable() {
        #expect(DomainIdentity.identifier(forAccountId: 7) == "account-7")
        #expect(DomainIdentity.identifier(forAccountId: 7) == "account-7")
        #expect(DomainIdentity.identifier(forAccountId: -3) == "account--3")
        #expect(
            DomainIdentity.identifier(forAccountId: Int64.max)
                == "account-9223372036854775807"
        )
    }

    @Test("Identifiers round-trip back to their account id")
    func identifierRoundTrips() {
        for id: Int64 in [0, 1, 7, -3, Int64.max, Int64.min] {
            let identifier = DomainIdentity.identifier(forAccountId: id)
            #expect(DomainIdentity.accountId(fromIdentifier: identifier) == id)
        }
    }

    @Test("Only the canonical spelling parses — foreign identifiers never alias an account")
    func parseIsStrict() {
        #expect(DomainIdentity.accountId(fromIdentifier: "account-007") == nil)
        #expect(DomainIdentity.accountId(fromIdentifier: "account-+7") == nil)
        #expect(DomainIdentity.accountId(fromIdentifier: "account-") == nil)
        #expect(DomainIdentity.accountId(fromIdentifier: "account-7x") == nil)
        #expect(DomainIdentity.accountId(fromIdentifier: "account-99999999999999999999") == nil)
        #expect(DomainIdentity.accountId(fromIdentifier: "chat-7") == nil)
        #expect(DomainIdentity.accountId(fromIdentifier: "") == nil)
        #expect(DomainIdentity.accountId(fromIdentifier: "account") == nil)
    }

    @Test("A single account presents as exactly GramDrive (POL-7)")
    func singleAccountName() {
        let domains = DomainIdentity.desiredDomains(for: [account(id: 7)])
        #expect(
            domains == [
                DesiredDomain(accountId: 7, identifier: "account-7", displayName: "GramDrive")
            ]
        )
    }

    @Test("Multiple accounts disambiguate with the account display name, in identity order")
    func multipleAccountNames() {
        let domains = DomainIdentity.desiredDomains(for: [
            account(id: 9, name: "Work"),
            account(id: 7, name: "Ivan"),
        ])
        #expect(
            domains == [
                DesiredDomain(
                    accountId: 7, identifier: "account-7", displayName: "GramDrive — Ivan"),
                DesiredDomain(
                    accountId: 9, identifier: "account-9", displayName: "GramDrive — Work"),
            ]
        )
    }

    @Test("Colliding account names append the account identity, deterministically")
    func collidingNames() {
        let domains = DomainIdentity.desiredDomains(for: [
            account(id: 7, name: "Ivan"),
            account(id: 9, name: "Ivan"),
            account(id: 11, name: "Work"),
        ])
        #expect(
            domains.map(\.displayName) == [
                "GramDrive — Ivan (7)",
                "GramDrive — Ivan (9)",
                "GramDrive — Work",
            ]
        )
    }

    @Test("A blank account name falls back to the account identity")
    func blankAccountName() {
        let domains = DomainIdentity.desiredDomains(for: [
            account(id: 7, name: "  "),
            account(id: 9, name: "Work"),
        ])
        #expect(domains[0].displayName == "GramDrive — Account 7")
    }

    @Test("Authorization state plays no part — reauthorization never moves a domain")
    func authStateIsIrrelevant() {
        let before = DomainIdentity.desiredDomains(for: [
            account(id: 7, authState: "authorized"),
            account(id: 9, authState: "authorized"),
        ])
        let after = DomainIdentity.desiredDomains(for: [
            account(id: 7, authState: "waiting_code"),
            account(id: 9, authState: "logged_out"),
        ])
        #expect(before == after)
    }

    @Test("No accounts means no desired domains")
    func emptyAccounts() {
        #expect(DomainIdentity.desiredDomains(for: []).isEmpty)
    }
}
