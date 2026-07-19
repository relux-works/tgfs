import Foundation
import GramDriveCore

/// The stable File Provider domain identity rule (TASK-260715-3s44pc;
/// PLAT-MAC-001).
///
/// A domain's identifier is a pure function of the account's stable
/// numeric identity — never of its display name, auth state, or namespace
/// epoch — so the same account always maps to the same domain across app
/// restarts, reinstalls, and reauthorization. Finder state (materialized
/// files, pins) is keyed by the system to this identifier; keeping it
/// stable is what "the domain appears once and recovers" means.
///
/// Display names follow POL-7: the drive presents as GramDrive. With a
/// single account the domain is exactly "GramDrive"; with several, each
/// domain disambiguates with the account's display name so Finder never
/// shows two identical entries.
public enum DomainIdentity {
    /// The user-visible drive name (POL-7 / DEC-019).
    public static let displayNameBase = "GramDrive"

    private static let identifierPrefix = "account-"

    /// The domain identifier for an account: `account-<id>`. Total and
    /// deterministic; the prefix keeps room for future non-account
    /// domains without ambiguity.
    public static func identifier(forAccountId accountId: Int64) -> String {
        "\(identifierPrefix)\(accountId)"
    }

    /// Parses an identifier produced by
    /// ``identifier(forAccountId:)`` back to its account identity.
    ///
    /// Strict by round-trip: only the canonical spelling is accepted
    /// (`account-007` is not `account-7`), so a corrupted or foreign
    /// domain identifier can never silently alias a real account.
    public static func accountId(fromIdentifier identifier: String) -> Int64? {
        guard identifier.hasPrefix(identifierPrefix) else { return nil }
        guard let accountId = Int64(identifier.dropFirst(identifierPrefix.count)) else {
            return nil
        }
        guard self.identifier(forAccountId: accountId) == identifier else { return nil }
        return accountId
    }
}

/// One domain the account set says should exist — identity plus the
/// display name the reconciler keeps Finder showing.
public struct DesiredDomain: Equatable, Sendable {
    /// The account this domain presents.
    public let accountId: Int64
    /// The stable domain identifier, per ``DomainIdentity``.
    public let identifier: String
    /// The user-visible name (POL-7 naming rule).
    public let displayName: String

    public init(accountId: Int64, identifier: String, displayName: String) {
        self.accountId = accountId
        self.identifier = identifier
        self.displayName = displayName
    }
}

extension DomainIdentity {
    /// Derives the desired domain set from the configured accounts, in
    /// stable identity order.
    ///
    /// One domain per account, always — authorization state deliberately
    /// plays no part, so losing and regaining authorization never tears a
    /// domain down (reauthorization keeps Finder state; the extension
    /// serves durable metadata regardless). Naming: a single account is
    /// exactly ``displayNameBase``; several accounts disambiguate with
    /// the account's display name, and accounts whose names still collide
    /// append their identity so the rule stays total and deterministic.
    public static func desiredDomains(for accounts: [AccountInfo]) -> [DesiredDomain] {
        let ordered = accounts.sorted { $0.accountId < $1.accountId }
        if ordered.count == 1, let only = ordered.first {
            return [
                DesiredDomain(
                    accountId: only.accountId,
                    identifier: identifier(forAccountId: only.accountId),
                    displayName: displayNameBase
                )
            ]
        }
        let names = ordered.map { disambiguatedName(for: $0) }
        var counts: [String: Int] = [:]
        for name in names {
            counts[name, default: 0] += 1
        }
        return zip(ordered, names).map { account, name in
            let unique =
                counts[name] == 1
                ? name
                : "\(name) (\(account.accountId))"
            return DesiredDomain(
                accountId: account.accountId,
                identifier: identifier(forAccountId: account.accountId),
                displayName: unique
            )
        }
    }

    private static func disambiguatedName(for account: AccountInfo) -> String {
        let trimmed = account.displayName.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty {
            return "\(displayNameBase) — Account \(account.accountId)"
        }
        return "\(displayNameBase) — \(trimmed)"
    }
}
