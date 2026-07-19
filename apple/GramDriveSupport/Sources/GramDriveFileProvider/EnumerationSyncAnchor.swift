import FileProvider
import Foundation
import GramDriveCore

/// The durable form of "everything up to here has been seen": the account,
/// its identity-namespace epoch, the change journal's instance, and the
/// journal sequence of the last change delivered.
///
/// `NSFileProviderSyncAnchor` is opaque data the system persists across
/// extension restarts and reboots, so everything an anchor's validity
/// depends on is bound into it and re-checked on every use:
///
/// * a different **journal instance** means another database life —
///   recovery quarantined the old file and sequences started over
///   (the journal's identity rule in `gramdrive-ffi`);
/// * a different **namespace epoch** means item identities changed
///   wholesale (DOM-021) and no diff can bridge it;
/// * a **sequence beyond the journal's high-water mark** claims knowledge
///   the journal never issued — defensively foreign.
///
/// Any of those answers `NSFileProviderError(.syncAnchorExpired)`, on which
/// the system drops its diff state and re-enumerates fully: recovery is
/// explicit, never a silently wrong diff.
struct EnumerationSyncAnchor: Codable, Equatable {
    /// The one codec version this build mints and accepts.
    static let version = 1

    /// Codec version of this anchor; a future build's anchor is foreign,
    /// never half-understood.
    let version: Int
    /// The account this anchor was minted for.
    let accountId: Int64
    /// The account's identity-namespace epoch at mint time (DOM-021).
    let namespaceVersion: UInt32
    /// The change journal's instance the sequence belongs to.
    let journalInstance: String
    /// The journal sequence of the last change delivered; changes strictly
    /// after it are what the next enumeration owes.
    let sequence: Int64

    init(accountId: Int64, namespaceVersion: UInt32, journalInstance: String, sequence: Int64) {
        self.version = Self.version
        self.accountId = accountId
        self.namespaceVersion = namespaceVersion
        self.journalInstance = journalInstance
        self.sequence = sequence
    }

    /// Decodes a system-held anchor, or `nil` when the data is not this
    /// codec's — which callers must treat as expired.
    static func decode(_ anchor: NSFileProviderSyncAnchor) -> EnumerationSyncAnchor? {
        guard
            let payload = try? JSONDecoder().decode(EnumerationSyncAnchor.self, from: anchor.rawValue),
            payload.version == version
        else {
            return nil
        }
        return payload
    }

    /// Whether this anchor still names a diffable position: same account,
    /// same epoch, same journal life, and a sequence the journal has
    /// actually issued.
    func isCurrent(account: AccountInfo, journal: ChangeJournalState) -> Bool {
        accountId == account.accountId
            && namespaceVersion == account.namespaceVersion
            && journalInstance == journal.instanceId
            && sequence <= journal.latestSequence
    }

    /// The system-facing form.
    func rawAnchor() -> NSFileProviderSyncAnchor {
        let encoder = JSONEncoder()
        encoder.outputFormatting = .sortedKeys
        guard let data = try? encoder.encode(self) else {
            // Encoding concrete integers and strings cannot fail; the guard
            // exists because `JSONEncoder.encode` is typed as throwing.
            preconditionFailure("sync anchor encoding is total")
        }
        return NSFileProviderSyncAnchor(rawValue: data)
    }
}
