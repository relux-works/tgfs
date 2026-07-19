import FileProvider
import Foundation
import GramDriveCore
import Testing

@testable import GramDriveFileProvider

// MARK: - Fixtures

private let accountId: Int64 = 7
private let rootId = "acct-root"

private func makeAccount(namespaceVersion: UInt32 = 1) -> AccountInfo {
    AccountInfo(
        accountId: accountId,
        sourceKind: .localTdlib,
        displayName: "Test Account",
        authState: "authorized",
        namespaceVersion: namespaceVersion,
        rootItemId: rootId
    )
}

private func directory(
    id: String, parent: String = rootId, safeName: String? = nil, version: String = "m1"
) -> ItemMetadata {
    ItemMetadata(
        id: id, parent: parent, kind: .chat, isDirectory: true,
        displayName: safeName ?? id, safeName: safeName ?? id, metadataVersion: version,
        mimeType: nil, logicalSize: nil, contentVersion: nil,
        availability: .fetchable, createdAtMs: 1_000, modifiedAtMs: 1_000, deletedAtMs: nil
    )
}

private func rootItem() -> ItemMetadata {
    ItemMetadata(
        id: rootId, parent: nil, kind: .account, isDirectory: true,
        displayName: "Test Account", safeName: "Test Account", metadataVersion: "m1",
        mimeType: nil, logicalSize: nil, contentVersion: nil,
        availability: .fetchable, createdAtMs: 1_000, modifiedAtMs: 1_000, deletedAtMs: nil
    )
}

/// A store holding the root and children `c-a … c-d` under it, all
/// journaled the way the engine's initial sync would have.
private func seededStore() -> ScriptedStore {
    let store = ScriptedStore(account: makeAccount())
    store.apply(rootItem())
    for name in ["c-a", "c-b", "c-c", "c-d"] {
        store.apply(directory(id: name))
    }
    return store
}

private func enumerator(
    over store: ScriptedStore,
    container: NSFileProviderItemIdentifier = .rootContainer,
    pageSize: UInt32 = 2
) -> GramDriveEnumerator {
    GramDriveEnumerator(
        store: store, accountId: accountId, container: container, pageSize: pageSize)
}

/// Walks every listing page and returns the identifiers in delivery order,
/// requiring the walk to terminate within `maxPages`.
private func listAll(
    _ enumerator: GramDriveEnumerator,
    maxPages: Int = 10
) throws -> (identifiers: [String], observers: [RecordingEnumerationObserver]) {
    var page = NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data)
    var observers: [RecordingEnumerationObserver] = []
    for _ in 0..<maxPages {
        let observer = RecordingEnumerationObserver()
        enumerator.enumerateItems(for: observer, startingAt: page)
        observers.append(observer)
        if let error = observer.finishError {
            throw error
        }
        guard let next = observer.finishedPages.last ?? nil else {
            return (observers.flatMap(\.enumeratedIdentifiers), observers)
        }
        page = next
    }
    Issue.record("listing did not terminate in \(maxPages) pages")
    return (observers.flatMap(\.enumeratedIdentifiers), observers)
}

private func mintedAnchor(_ enumerator: GramDriveEnumerator) -> NSFileProviderSyncAnchor {
    var minted: NSFileProviderSyncAnchor?
    enumerator.currentSyncAnchor { minted = $0 }
    guard let minted else {
        Issue.record("currentSyncAnchor answered nil")
        return NSFileProviderSyncAnchor(rawValue: Data())
    }
    return minted
}

// MARK: - Item listing

@Suite("Enumerator item listing")
struct EnumeratorListingTests {
    @Test("Pages compose the exact child set in stable order, no duplicates, no gaps")
    func pagesCompose() throws {
        let store = seededStore()
        let (identifiers, observers) = try listAll(enumerator(over: store))
        #expect(identifiers == ["c-a", "c-b", "c-c", "c-d"])
        #expect(observers.count == 3, "4 children at page size 2: two full pages, one final")
        #expect((observers.last?.finishedPages.last ?? nil) == nil, "the last page ends the listing")
    }

    @Test("Both initial-page sentinels start from the beginning")
    func initialSentinels() {
        let store = seededStore()
        for sentinel in [
            NSFileProviderPage.initialPageSortedByName as Data,
            NSFileProviderPage.initialPageSortedByDate as Data,
        ] {
            let observer = RecordingEnumerationObserver()
            enumerator(over: store).enumerateItems(
                for: observer, startingAt: NSFileProviderPage(sentinel))
            #expect(observer.finishError == nil)
            #expect(observer.enumeratedIdentifiers == ["c-a", "c-b"])
        }
    }

    @Test("A foreign page answers pageExpired — the explicit restart, never a guess")
    func foreignPage() {
        let store = seededStore()
        let garbage = NSFileProviderPage(rawValue: Data("not a cursor".utf8))
        let foreignContainer = EnumerationPageCursor.page(parent: "some-other-dir", after: "x")
        for page in [garbage, foreignContainer] {
            let observer = RecordingEnumerationObserver()
            enumerator(over: store).enumerateItems(for: observer, startingAt: page)
            #expect(observer.enumeratedIdentifiers.isEmpty)
            #expect(
                (observer.finishError as? NSFileProviderError)?.code == .pageExpired,
                "got \(String(describing: observer.finishError))"
            )
        }
    }

    @Test("The working set lists no items — macOS pulls only its changes")
    func workingSetListsEmpty() {
        let store = seededStore()
        let observer = RecordingEnumerationObserver()
        enumerator(over: store, container: .workingSet).enumerateItems(
            for: observer, startingAt: NSFileProviderPage(rawValue: Data()))
        #expect(observer.finishError == nil)
        #expect(observer.batches.isEmpty)
        #expect(observer.finishedPages.count == 1)
        #expect((observer.finishedPages.last ?? nil) == nil)
    }

    @Test("The root of a not-yet-synced account lists empty, not absent")
    func unsyncedRootListsEmpty() throws {
        let store = ScriptedStore(account: makeAccount())
        let (identifiers, _) = try listAll(enumerator(over: store))
        #expect(identifiers.isEmpty)
    }

    @Test("A missing or tombstoned container answers noSuchItem")
    func deadContainers() {
        let store = seededStore()
        store.tombstone(id: "c-b", atMs: 2_000)
        for container in [
            NSFileProviderItemIdentifier("never-existed"),
            NSFileProviderItemIdentifier("c-b"),
        ] {
            let observer = RecordingEnumerationObserver()
            enumerator(over: store, container: container).enumerateItems(
                for: observer, startingAt: NSFileProviderPage(rawValue: Data()))
            #expect(
                (observer.finishError as? NSFileProviderError)?.code == .noSuchItem,
                "got \(String(describing: observer.finishError))"
            )
        }
    }

    @Test("A gone account answers noSuchItem")
    func goneAccount() {
        let store = seededStore()
        store.removeAccount(accountId: accountId)
        let observer = RecordingEnumerationObserver()
        enumerator(over: store).enumerateItems(
            for: observer, startingAt: NSFileProviderPage(rawValue: Data()))
        #expect((observer.finishError as? NSFileProviderError)?.code == .noSuchItem)
    }

    @Test("The observer's page-size suggestion caps the page")
    func suggestionCaps() {
        let store = seededStore()
        let observer = RecordingEnumerationObserver()
        observer.pageSizeSuggestion = 1
        enumerator(over: store, pageSize: 100).enumerateItems(
            for: observer,
            startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        #expect(observer.enumeratedIdentifiers == ["c-a"])
        #expect((observer.finishedPages.last ?? nil) != nil, "a capped full page continues")
    }

    @Test("Every listing callback completes before the call returns")
    func synchronousCompletion() {
        // The deadline half of the AC, pinned structurally: nothing is ever
        // in flight after enumerateItems returns, so system callback
        // deadlines cannot be outlived.
        let store = seededStore()
        let observer = RecordingEnumerationObserver()
        enumerator(over: store).enumerateItems(
            for: observer,
            startingAt: NSFileProviderPage(NSFileProviderPage.initialPageSortedByName as Data))
        #expect(observer.finishedPages.count == 1 || observer.finishError != nil)
    }
}

// MARK: - Scripted concurrent updates (the AC's core)

@Suite("Enumeration under concurrent updates")
struct EnumeratorConcurrencyTests {
    @Test("A new item landing between pages never duplicates, and changes close the gap")
    func insertBetweenPages() throws {
        let store = seededStore()
        let listEnumerator = enumerator(over: store)
        let anchor = mintedAnchor(listEnumerator)

        // Between page one ([c-a, c-b]) and page two, the engine commits a
        // new message's directory sorting *behind* the page anchor.
        store.scriptBeforeNextChildrenCall {}  // page one: untouched
        store.scriptBeforeNextChildrenCall {
            store.apply(directory(id: "c-ab"))
        }
        let (identifiers, _) = try listAll(listEnumerator)

        #expect(
            identifiers == ["c-a", "c-b", "c-c", "c-d"],
            "keyset pages never duplicate and never resurrect behind the anchor"
        )
        #expect(Set(identifiers).count == identifiers.count, "no duplicates, ever")

        // What the listing could not see, change enumeration replays from
        // the anchor minted before the listing began.
        let changes = RecordingChangeObserver()
        listEnumerator.enumerateChanges(for: changes, from: anchor)
        #expect(changes.finishError == nil)
        #expect(changes.updatedIdentifiers == ["c-ab"])
    }

    @Test("A rename between pages never duplicates; the change feed carries the new name")
    func renameBetweenPages() throws {
        let store = seededStore()
        let listEnumerator = enumerator(over: store)
        let anchor = mintedAnchor(listEnumerator)

        store.scriptBeforeNextChildrenCall {}
        store.scriptBeforeNextChildrenCall {
            store.apply(directory(id: "c-a", safeName: "c-a-renamed", version: "m2"))
        }
        let (identifiers, _) = try listAll(listEnumerator)
        #expect(identifiers == ["c-a", "c-b", "c-c", "c-d"], "identity is the id, not the name")

        let changes = RecordingChangeObserver()
        listEnumerator.enumerateChanges(for: changes, from: anchor)
        #expect(changes.updatedIdentifiers == ["c-a"])
        #expect(changes.updatedBatches.flatMap { $0.map(\.filename) } == ["c-a-renamed"])
    }

    @Test("A tombstone between pages stops listing; the change feed reports the deletion")
    func tombstoneBetweenPages() throws {
        let store = seededStore()
        let listEnumerator = enumerator(over: store)
        let anchor = mintedAnchor(listEnumerator)

        store.scriptBeforeNextChildrenCall {}
        store.scriptBeforeNextChildrenCall {
            store.tombstone(id: "c-c", atMs: 2_000)
        }
        let (identifiers, _) = try listAll(listEnumerator)
        #expect(identifiers == ["c-a", "c-b", "c-d"], "a tombstoned item stops being listed")

        let changes = RecordingChangeObserver()
        listEnumerator.enumerateChanges(for: changes, from: anchor)
        #expect(changes.updatedIdentifiers.isEmpty)
        #expect(changes.deletedIdentifiers == ["c-c"])
    }
}

// MARK: - Change enumeration and anchors

@Suite("Change enumeration and sync anchors")
struct EnumeratorChangeTests {
    @Test("Changes page by sequence with explicit anchors until the journal runs dry")
    func changesPage() {
        let store = seededStore()  // seeds journal sequences 1…5
        let changeEnumerator = enumerator(over: store, container: .workingSet)

        // From "seen nothing" (sequence 0): 5 changes at batch size 2 are
        // three batches, the last one short and final.
        var anchor = EnumerationSyncAnchor(
            accountId: accountId, namespaceVersion: 1,
            journalInstance: "life-1", sequence: 0
        ).rawAnchor()
        var seen: [String] = []
        var rounds = 0
        while rounds < 10 {
            rounds += 1
            let observer = RecordingChangeObserver()
            changeEnumerator.enumerateChanges(for: observer, from: anchor)
            #expect(observer.finishError == nil)
            guard let finish = observer.finishes.last else {
                Issue.record("no finish")
                return
            }
            seen += observer.updatedIdentifiers
            anchor = finish.anchor
            if !finish.moreComing { break }
        }
        #expect(seen == [NSFileProviderItemIdentifier.rootContainer.rawValue, "c-a", "c-b", "c-c", "c-d"])
        #expect(rounds == 3, "5 changes at batch size 2: two full batches, one short")

        // The final anchor is the journal's high-water mark: enumerating
        // from it again owes nothing.
        let quiet = RecordingChangeObserver()
        changeEnumerator.enumerateChanges(for: quiet, from: anchor)
        #expect(quiet.updatedBatches.isEmpty && quiet.deletedBatches.isEmpty)
        #expect(quiet.finishes.last?.moreComing == false)
        #expect(EnumerationSyncAnchor.decode(quiet.finishes.last!.anchor)?.sequence == store.latestSequence)
    }

    @Test("The account root folds onto rootContainer in the change feed")
    func rootFoldsInChanges() {
        let store = seededStore()
        let observer = RecordingChangeObserver()
        enumerator(over: store, container: .workingSet).enumerateChanges(
            for: observer,
            from: EnumerationSyncAnchor(
                accountId: accountId, namespaceVersion: 1,
                journalInstance: "life-1", sequence: 0
            ).rawAnchor())
        #expect(
            observer.updatedIdentifiers.first
                == NSFileProviderItemIdentifier.rootContainer.rawValue,
            "the reserved identifier, never the raw root id"
        )
    }

    @Test("currentSyncAnchor mints the journal's high-water mark")
    func currentAnchor() {
        let store = seededStore()
        let minted = EnumerationSyncAnchor.decode(mintedAnchor(enumerator(over: store)))
        #expect(minted?.sequence == store.latestSequence)
        #expect(minted?.accountId == accountId)
        #expect(minted?.namespaceVersion == 1)
        #expect(minted?.journalInstance == "life-1")
    }

    @Test("Foreign, epoch-bumped, other-life, and overtaking anchors expire explicitly")
    func expiredAnchors() {
        let cases: [(String, (ScriptedStore) -> NSFileProviderSyncAnchor)] = [
            (
                "undecodable data",
                { _ in NSFileProviderSyncAnchor(rawValue: Data("junk".utf8)) }
            ),
            (
                "namespace epoch bumped",
                { store in
                    let anchor = mintedAnchor(enumerator(over: store))
                    store.replaceAccount(makeAccount(namespaceVersion: 2))
                    return anchor
                }
            ),
            (
                "another journal life",
                { store in
                    let anchor = mintedAnchor(enumerator(over: store))
                    store.restartJournalLife(instance: "life-2")
                    return anchor
                }
            ),
            (
                "a sequence the journal never issued",
                { _ in
                    EnumerationSyncAnchor(
                        accountId: accountId, namespaceVersion: 1,
                        journalInstance: "life-1", sequence: 9_999
                    ).rawAnchor()
                }
            ),
        ]
        for (name, makeAnchor) in cases {
            let store = seededStore()
            let anchor = makeAnchor(store)
            let observer = RecordingChangeObserver()
            enumerator(over: store, container: .workingSet).enumerateChanges(
                for: observer, from: anchor)
            #expect(
                (observer.finishError as? NSFileProviderError)?.code == .syncAnchorExpired,
                "\(name): got \(String(describing: observer.finishError))"
            )
            #expect(observer.updatedBatches.isEmpty && observer.deletedBatches.isEmpty)
        }
    }

    @Test("A gone account fails change enumeration with noSuchItem")
    func goneAccount() {
        let store = seededStore()
        let anchor = mintedAnchor(enumerator(over: store))
        store.removeAccount(accountId: accountId)
        let observer = RecordingChangeObserver()
        enumerator(over: store, container: .workingSet).enumerateChanges(
            for: observer, from: anchor)
        #expect((observer.finishError as? NSFileProviderError)?.code == .noSuchItem)
    }

    @Test("Directory containers serve the same domain-wide change feed")
    func containersShareTheFeed() {
        let store = seededStore()
        let anchor = mintedAnchor(enumerator(over: store))
        store.apply(directory(id: "c-e"))
        store.tombstone(id: "c-a", atMs: 2_000)

        let observer = RecordingChangeObserver()
        enumerator(over: store, container: NSFileProviderItemIdentifier("c-b"))
            .enumerateChanges(for: observer, from: anchor)
        #expect(observer.finishError == nil)
        #expect(observer.updatedIdentifiers == ["c-e"])
        #expect(observer.deletedIdentifiers == ["c-a"])
    }
}
