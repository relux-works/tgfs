import FileProvider
import Foundation
import GramDriveCore

/// Why a page handed back by the system cannot continue this listing: it
/// was not minted by this codec for this container. Typed so the enumerator
/// can answer the platform's explicit recovery — `NSFileProviderError
/// (.pageExpired)`, on which the system restarts from the initial page —
/// instead of guessing a start position that could duplicate or skip items
/// (SYNC-003 makes both contract failures).
enum EnumerationPageCursorError: Error, Equatable {
    /// The page data is not this codec's, is a later codec version, or was
    /// minted for a different container.
    case foreignPage
}

/// The durable form of a mid-listing position: which container, and the
/// last item identifier already delivered (the SYNC-003 keyset anchor the
/// core's `children(parent:after:limit:)` pages by).
///
/// `NSFileProviderPage` is opaque data the system stores and replays
/// verbatim, possibly across extension restarts, so the encoding is
/// versioned and self-describing. Binding the container in means a page
/// replayed against the wrong enumerator is caught as foreign rather than
/// silently anchoring one directory's listing inside another.
enum EnumerationPageCursor {
    /// The encoded payload. `version` gates the whole decode: a future
    /// build's page is foreign, never half-understood.
    private struct Payload: Codable {
        let version: Int
        let accountId: Int64
        let namespaceVersion: UInt32
        let journalInstance: String
        let parent: String
        let after: String
    }

    /// The one version this build mints and accepts.
    static let version = 2

    /// The continuation page after `after` within `parent`.
    static func page(
        parent: String,
        after: String,
        account: AccountInfo,
        journal: ChangeJournalState
    ) -> NSFileProviderPage {
        let encoder = JSONEncoder()
        encoder.outputFormatting = .sortedKeys
        guard
            let data = try? encoder.encode(
                Payload(
                    version: version,
                    accountId: account.accountId,
                    namespaceVersion: account.namespaceVersion,
                    journalInstance: journal.instanceId,
                    parent: parent,
                    after: after))
        else {
            // Encoding three concrete strings cannot fail; the guard exists
            // because `JSONEncoder.encode` is typed as throwing.
            preconditionFailure("page cursor encoding is total")
        }
        return NSFileProviderPage(rawValue: data)
    }

    /// The keyset anchor a listing of `parent` starts after: `nil` for both
    /// initial-page sentinels — items are served in the stable core
    /// identifier order regardless of the requested sort; the system's own
    /// replica does the presentation sorting — and the recorded last
    /// identifier for a page this codec minted for this `parent`. Anything
    /// else is [`EnumerationPageCursorError.foreignPage`].
    static func startAnchor(
        of page: NSFileProviderPage,
        parent: String,
        account: AccountInfo,
        journal: ChangeJournalState
    ) throws -> String? {
        let raw = page.rawValue
        if raw == NSFileProviderPage.initialPageSortedByName as Data
            || raw == NSFileProviderPage.initialPageSortedByDate as Data
        {
            return nil
        }
        guard
            let payload = try? JSONDecoder().decode(Payload.self, from: raw),
            payload.version == version,
            payload.accountId == account.accountId,
            payload.namespaceVersion == account.namespaceVersion,
            payload.journalInstance == journal.instanceId,
            payload.parent == parent
        else {
            throw EnumerationPageCursorError.foreignPage
        }
        return payload.after
    }
}
