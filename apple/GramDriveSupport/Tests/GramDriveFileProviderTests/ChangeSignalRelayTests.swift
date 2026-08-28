import FileProvider
import Foundation
import GramDriveAgentCore
import GramDriveCore
import GramDriveSupport
import Testing

@testable import GramDriveCompanion
@testable import GramDriveFileProvider

/// Records working-set signals instead of talking to the file provider
/// daemon.
private final class RecordingSignaling: ProviderChangeSignaling, @unchecked Sendable {
    private let lock = NSLock()
    private var requests: [Bool] = []
    private var containers: [[String]] = []
    private var generatedItems: [[String]] = []
    private var completionErrors: [(any Error)?]

    init(completionErrors: [(any Error)?] = []) {
        self.completionErrors = completionErrors
    }

    var signalCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return requests.count
    }

    var includeRootRequests: [Bool] {
        lock.lock()
        defer { lock.unlock() }
        return requests
    }

    var changedContainerRequests: [[String]] {
        lock.lock()
        defer { lock.unlock() }
        return containers
    }

    var evictedGeneratedItemRequests: [[String]] {
        lock.lock()
        defer { lock.unlock() }
        return generatedItems
    }

    func signalChanges(
        includeRoot: Bool,
        changedContainers: [NSFileProviderItemIdentifier],
        evictingGeneratedItems: [ProviderGeneratedItemChange],
        completionHandler: @escaping @Sendable ((any Error)?) -> Void
    ) {
        lock.lock()
        requests.append(includeRoot)
        containers.append(changedContainers.map(\.rawValue))
        generatedItems.append(evictingGeneratedItems.map(\.item.rawValue))
        let error = completionErrors.isEmpty ? nil : completionErrors.removeFirst()
        lock.unlock()
        completionHandler(error)
    }
}

/// Records requests while leaving their completions under explicit test
/// control, so relay overlap is deterministic instead of scheduler-dependent.
private final class DelayedSignaling: ProviderChangeSignaling, @unchecked Sendable {
    private let lock = NSLock()
    private var requests: [Bool] = []
    private var generatedItems: [[String]] = []
    private var completions: [(@Sendable ((any Error)?) -> Void)] = []

    var signalCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return requests.count
    }

    var includeRootRequests: [Bool] {
        lock.lock()
        defer { lock.unlock() }
        return requests
    }

    var evictedGeneratedItemRequests: [[String]] {
        lock.lock()
        defer { lock.unlock() }
        return generatedItems
    }

    func signalChanges(
        includeRoot: Bool,
        changedContainers: [NSFileProviderItemIdentifier],
        evictingGeneratedItems: [ProviderGeneratedItemChange],
        completionHandler: @escaping @Sendable ((any Error)?) -> Void
    ) {
        lock.lock()
        requests.append(includeRoot)
        generatedItems.append(evictingGeneratedItems.map(\.item.rawValue))
        completions.append(completionHandler)
        lock.unlock()
    }

    func complete(_ index: Int, with error: (any Error)?) {
        lock.lock()
        let completion = completions[index]
        lock.unlock()
        completion(error)
    }
}

/// A scripted probe: each check consumes the next stamped value (or
/// failure).
private final class ScriptedProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var script: [Result<Int64, Error>]

    init(_ script: [Result<Int64, Error>]) {
        self.script = script
    }

    func next() throws -> Int64 {
        lock.lock()
        defer { lock.unlock() }
        precondition(!script.isEmpty, "probe called more often than scripted")
        return try script.removeFirst().get()
    }
}

private struct ProbeDown: Error {}
private struct SignalDown: Error {}

private final class DispatchRecorder: @unchecked Sendable {
    private let lock = NSLock()
    private var recordedEvents: [String] = []
    private var recordedError: (any Error)?

    var events: [String] {
        lock.lock()
        defer { lock.unlock() }
        return recordedEvents
    }

    var error: (any Error)? {
        lock.lock()
        defer { lock.unlock() }
        return recordedError
    }

    func record(_ event: String) {
        lock.lock()
        recordedEvents.append(event)
        lock.unlock()
    }

    func finish(_ error: (any Error)?) {
        lock.lock()
        recordedError = error
        lock.unlock()
    }
}

/// Drives the same resolver -> dispatcher boundary as the companion's live
/// relay while replacing only the two entitlement-bound manager operations.
private final class ProductionPathSignaling: ProviderChangeSignaling, @unchecked Sendable {
    private let recorder: DispatchRecorder
    private let materializedContainerIDs: [String]
    private let didEvict: @Sendable (String) throws -> Void

    init(
        recorder: DispatchRecorder,
        materializedContainerIDs: [String],
        didEvict: @escaping @Sendable (String) throws -> Void = { _ in }
    ) {
        self.recorder = recorder
        self.materializedContainerIDs = materializedContainerIDs
        self.didEvict = didEvict
    }

    func signalChanges(
        includeRoot: Bool,
        changedContainers: [NSFileProviderItemIdentifier],
        evictingGeneratedItems: [ProviderGeneratedItemChange],
        completionHandler: @escaping @Sendable ((any Error)?) -> Void
    ) {
        ProviderChangeDispatcher(
            materializedEnumerator: ScriptedMaterializedEnumerator(
                pages: [materializedContainerIDs.map(materializedContainer)]),
            evict: { [recorder, didEvict] identifier, completion in
                recorder.record("evict:\(identifier.rawValue)")
                do {
                    try didEvict(identifier.rawValue)
                    completion(nil)
                } catch {
                    completion(error)
                }
            },
            signal: { [recorder] identifier, completion in
                recorder.record("signal:\(identifier.rawValue)")
                completion(nil)
            }
        ).dispatch(
            includeRoot: includeRoot,
            changedContainers: changedContainers,
            evictingGeneratedItems: evictingGeneratedItems,
            completionHandler: completionHandler)
    }
}

private final class StartupHealthScript: @unchecked Sendable {
    private let lock = NSLock()
    private var readouts: [HealthReadout]

    init(_ readouts: [HealthReadout]) {
        self.readouts = readouts
    }

    func next() -> HealthReadout {
        lock.lock()
        defer { lock.unlock() }
        precondition(!readouts.isEmpty, "health read more often than scripted")
        return readouts.count == 1 ? readouts[0] : readouts.removeFirst()
    }
}

private struct GeneratedBoundarySeed {
    let values: [String: String]

    subscript(_ key: String) -> String {
        get throws {
            guard let value = values[key] else {
                throw GeneratedBoundaryFixtureError("seeder omitted \(key)")
            }
            return value
        }
    }

    static func create(at dataRoot: URL) throws -> Self {
        let repositoryRoot = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let process = Process()
        process.currentDirectoryURL = repositoryRoot
        process.executableURL = URL(fileURLWithPath: "/usr/bin/env")
        process.arguments = [
            "cargo", "run", "--quiet", "-p", "gramdrive-ffi", "--example",
            "shared_state_seed", "--", dataRoot.path, "generated-initial",
        ]
        let output = Pipe()
        process.standardOutput = output
        process.standardError = output
        try process.run()
        let bytes = output.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        let text = String(decoding: bytes, as: UTF8.self)
        guard process.terminationStatus == 0 else {
            throw GeneratedBoundaryFixtureError(
                "production fixture seeder exited \(process.terminationStatus): \(text)")
        }
        let values = Dictionary(
            uniqueKeysWithValues: text.split(separator: "\n").compactMap { line in
                let pair = line.split(separator: "=", maxSplits: 1).map(String.init)
                return pair.count == 2 ? (pair[0], pair[1]) : nil
            })
        return Self(values: values)
    }
}

private struct GeneratedBoundaryFixtureError: Error, CustomStringConvertible {
    let description: String

    init(_ description: String) {
        self.description = description
    }
}

/// File-backed stand-in for macOS' system-owned replica. Existing files bypass
/// the extension; after the mocked entitlement-bound eviction, `open` drives
/// the production content fetch and persists the returned materialization.
private final class InstalledGeneratedMaterializations: @unchecked Sendable {
    private let lock = NSLock()
    private let urls: [String: URL]

    init(directory: URL, initial: [String: (name: String, bytes: Data)]) throws {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        var urls: [String: URL] = [:]
        for (identifier, value) in initial {
            let url = directory.appendingPathComponent(value.name, isDirectory: false)
            try value.bytes.write(to: url)
            urls[identifier] = url
        }
        self.urls = urls
    }

    func evict(_ identifier: String) throws {
        lock.lock()
        let url = urls[identifier]
        lock.unlock()
        guard let url, FileManager.default.fileExists(atPath: url.path) else { return }
        try FileManager.default.removeItem(at: url)
    }

    func existingBytes(for identifier: String) throws -> Data? {
        lock.lock()
        let url = urls[identifier]
        lock.unlock()
        guard let url, FileManager.default.fileExists(atPath: url.path) else { return nil }
        return try Data(contentsOf: url)
    }

    @MainActor
    func open(
        _ identifier: String,
        fetch: () async throws -> URL
    ) async throws -> Data {
        let url = lock.withLock { urls[identifier] }
        guard let url else {
            throw GeneratedBoundaryFixtureError("no materialization slot for \(identifier)")
        }
        if FileManager.default.fileExists(atPath: url.path) {
            return try Data(contentsOf: url)
        }
        let fetched = try await fetch()
        try FileManager.default.copyItem(at: fetched, to: url)
        return try Data(contentsOf: url)
    }
}

/// Direct in-process composition of the same production hydrator hosted by
/// the companion's IPC server. The socket and descriptor transfer are tested
/// separately; this regression needs the verified-cache selection itself.
private final class CoreHydrationRequestAdapter: HydrationRequesting, @unchecked Sendable {
    private let hydrator: CoreContentHydrator

    init(hydrator: CoreContentHydrator) {
        self.hydrator = hydrator
    }

    func hydrate(
        _ request: HydrationRequest,
        onProgress: @escaping @Sendable (HydrationProgress) -> Void
    ) async throws -> HydratedContent {
        try await hydrator.hydrate(
            request, progress: onProgress, token: CancellationToken())
    }

    func hydrateAndMaterialize(
        _ request: HydrationRequest,
        onProgress: @escaping @Sendable (HydrationProgress) -> Void,
        materialize: @escaping @Sendable (HydratedContent) throws -> URL
    ) async throws -> URL {
        let content = try await hydrate(request, onProgress: onProgress)
        defer { hydrator.release(content) }
        return try materialize(content)
    }
}

private final class RelayRetainer: @unchecked Sendable {
    private let lock = NSLock()
    private var relays: [ChangeSignalRelay] = []

    func retain(_ relay: ChangeSignalRelay) {
        lock.lock()
        relays.append(relay)
        lock.unlock()
    }
}

private final class ScriptedMaterializedEnumerator: NSObject, NSFileProviderEnumerator,
    @unchecked Sendable
{
    private let lock = NSLock()
    private var pages: [[any NSFileProviderItem]]
    private let holdListing: Bool
    private let listingError: (any Error)?
    private var invalidateCount = 0
    private var firstPageWasEmptyData = false
    private var hasEnumeratedPage = false

    var wasInvalidated: Bool {
        lock.lock()
        defer { lock.unlock() }
        return invalidateCount > 0
    }

    var usedMaterializedInitialPage: Bool {
        lock.lock()
        defer { lock.unlock() }
        return firstPageWasEmptyData
    }

    init(
        pages: [[any NSFileProviderItem]],
        holdListing: Bool = false,
        listingError: (any Error)? = nil
    ) {
        self.pages = pages
        self.holdListing = holdListing
        self.listingError = listingError
    }

    func invalidate() {
        lock.lock()
        invalidateCount += 1
        lock.unlock()
    }

    func enumerateItems(
        for observer: NSFileProviderEnumerationObserver,
        startingAt page: NSFileProviderPage
    ) {
        lock.lock()
        if !hasEnumeratedPage {
            firstPageWasEmptyData = page == NSFileProviderPage(Data())
            hasEnumeratedPage = true
        }
        guard !holdListing else {
            lock.unlock()
            return
        }
        if let listingError {
            lock.unlock()
            observer.finishEnumeratingWithError(listingError)
            return
        }
        let items = pages.isEmpty ? [] : pages.removeFirst()
        let hasNext = !pages.isEmpty
        lock.unlock()
        observer.didEnumerate(items)
        observer.finishEnumerating(upTo: hasNext ? NSFileProviderPage(Data([1])) : nil)
    }

    func enumerateChanges(
        for observer: NSFileProviderChangeObserver,
        from syncAnchor: NSFileProviderSyncAnchor
    ) {
        observer.finishEnumeratingWithError(NSFileProviderError(.cannotSynchronize))
    }
}

private final class RacyTimeout: @unchecked Sendable {
    private let lock = NSLock()
    private var action: (@Sendable () -> Void)?

    func schedule(
        _ action: @escaping @Sendable () -> Void
    ) -> MaterializedGeneratedItemSelector.CancelTimeout {
        lock.lock()
        self.action = action
        lock.unlock()
        return {
            // Deliberately does not suppress `fire()`: this models a submitted
            // DispatchWorkItem whose cancellation races with execution.
        }
    }

    func fire() {
        lock.lock()
        let action = self.action
        lock.unlock()
        action?()
    }
}

private final class ManualTimeout: @unchecked Sendable {
    private let lock = NSLock()
    private var action: (@Sendable () -> Void)?
    private var cancelled = false

    func schedule(
        _ action: @escaping @Sendable () -> Void
    ) -> MaterializedGeneratedItemSelector.CancelTimeout {
        lock.lock()
        self.action = action
        cancelled = false
        lock.unlock()
        return { [weak self] in
            self?.cancel()
        }
    }

    func fire() {
        lock.lock()
        let action = cancelled ? nil : self.action
        lock.unlock()
        action?()
    }

    private func cancel() {
        lock.lock()
        cancelled = true
        action = nil
        lock.unlock()
    }
}

/// The real materialized-items API enumerates containers, not files. Keeping
/// that platform shape explicit is the regression's semantic mutant proof:
/// matching the generated item identifier itself must stay red.
private func materializedContainer(_ id: String) -> any NSFileProviderItem {
    GramDriveFileProviderItem(
        metadata: ItemMetadata(
            contractVersion: 1,
            id: id,
            parent: "root",
            kind: .chat,
            isDirectory: true,
            displayName: "Container",
            safeName: "Container",
            metadataVersion: "metadata-v1",
            mimeType: nil,
            logicalSize: nil,
            attachmentLogicalKind: nil,
            attachmentRepresentation: nil,
            attachmentFidelity: nil,
            attachmentSourceName: nil,
            attachmentExactSize: nil,
            contentVersion: nil,
            availability: .fetchable,
            createdAtMs: 1,
            modifiedAtMs: 2,
            deletedAtMs: nil),
        accountRootId: "root")
}

private func generatedChange(
    _ item: String,
    parent: String = "parent"
) -> ProviderGeneratedItemChange {
    ProviderGeneratedItemChange(
        item: NSFileProviderItemIdentifier(item),
        parent: NSFileProviderItemIdentifier(parent))
}

/// A cancellable token the tests own, standing in for the Darwin
/// observation.
private final class RecordingToken: ChangeObservationToken, @unchecked Sendable {
    private let lock = NSLock()
    private var cancelCount = 0

    var cancelled: Bool {
        lock.lock()
        defer { lock.unlock() }
        return cancelCount > 0
    }

    func cancel() {
        lock.lock()
        cancelCount += 1
        lock.unlock()
    }
}

@Suite("Change-signal relay")
struct ChangeSignalRelayTests {
    @MainActor
    @Test("Installed initial publication repairs generated bytes absent from the journal")
    func installedStartupPublishesExactGeneratedBytesAfterReadinessConverges() async throws {
        let dataRoot = FileManager.default.temporaryDirectory
            .appendingPathComponent("gramdrive-build154-boundary-\(UUID().uuidString)")
        let scratch = dataRoot.appendingPathComponent("provider-scratch", isDirectory: true)
        let replica = dataRoot.appendingPathComponent("system-replica", isDirectory: true)
        try FileManager.default.createDirectory(at: scratch, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dataRoot) }

        let seed = try GeneratedBoundarySeed.create(at: dataRoot)
        #expect(try seed["journal_latest_sequence"] == "0")
        let store = try SharedStateStore.open(dataRoot: dataRoot.path, role: .provider)
        guard let account = try store.account(accountId: 7) else {
            throw GeneratedBoundaryFixtureError("real FFI store omitted synthetic account")
        }
        let journal = try store.changeJournalState()
        #expect(journal.latestSequence == 0, "the regression must not manufacture a journal row")

        func providerIdentifier(_ key: String) throws -> String {
            ItemIdentifierMapping.providerIdentifier(
                forCoreItemId: try seed[key],
                accountRootId: account.rootItemId
            ).rawValue
        }

        let markdown = try providerIdentifier("markdown")
        let ndjson = try providerIdentifier("ndjson")
        let chatJSON = try providerIdentifier("chat_json")
        let unrelated = try providerIdentifier("unrelated")
        let deleted = try providerIdentifier("deleted")
        let attachment = try providerIdentifier("attachment")
        let chatParent = try providerIdentifier("chat_parent")
        let current: [String: (bytes: Data, version: String)] = [
            markdown: (Data("# synthetic g2\n".utf8), "markdown-v2"),
            ndjson: (Data("{\"synthetic\":2}\n".utf8), "ndjson-v2"),
            chatJSON: (Data("{\"generation\":2}\n".utf8), "chat-json-v2"),
        ]
        for (identifier, expected) in current {
            let coreID = ItemIdentifierMapping.coreItemId(
                for: NSFileProviderItemIdentifier(identifier),
                accountRootId: account.rootItemId)
            let metadata = try #require(try store.item(id: coreID))
            #expect(metadata.contentVersion == expected.version)
            #expect(metadata.logicalSize == UInt64(expected.bytes.count))
        }
        let liveGenerated = try store.liveGeneratedItems(accountId: account.accountId)
        #expect(
            Set(liveGenerated.map(\.id)).isSuperset(
                of: Set(
                    current.keys.map { key in
                        ItemIdentifierMapping.coreItemId(
                            for: NSFileProviderItemIdentifier(key),
                            accountRootId: account.rootItemId)
                    })))

        let stale = Data("synthetic-g1".utf8)
        let unchanged = Data("keep".utf8)
        let materializations = try InstalledGeneratedMaterializations(
            directory: replica,
            initial: [
                markdown: ("markdown.materialized", stale),
                ndjson: ("ndjson.materialized", stale),
                chatJSON: ("chat-json.materialized", stale),
                unrelated: ("unrelated.materialized", unchanged),
                deleted: ("deleted.materialized", unchanged),
                attachment: ("attachment.materialized", unchanged),
            ])
        for identifier in current.keys {
            #expect(try materializations.existingBytes(for: identifier) == stale)
        }

        let core = try DriveCore(config: CoreConfig(dataDir: dataRoot.path))
        let hydration = CoreHydrationRequestAdapter(
            hydrator: CoreContentHydrator(hydrator: try core.hydrator()))
        let fetcher = ContentFetcher(
            hydration: hydration,
            scratchDirectory: { scratch })
        func productionFetch(_ identifier: String) async throws -> (URL, NSFileProviderItemVersion)
        {
            let future = TestFuture<FetchOutcome>()
            _ = fetcher.fetchContents(
                itemIdentifier: NSFileProviderItemIdentifier(identifier),
                requestedVersion: nil,
                context: { (account: account, store: store) },
                completionHandler: { url, item, error in
                    future.fulfill(FetchOutcome(url: url, item: item, error: error))
                })
            let outcome = await future.settled
            if let error = outcome.error {
                throw error
            }
            guard let url = outcome.url, let item = outcome.fetchedItem else {
                throw GeneratedBoundaryFixtureError("production fetch returned no materialization")
            }
            return (url, item.itemVersion)
        }

        let recorder = DispatchRecorder()
        let retainedRelays = RelayRetainer()
        let setup = CoalescingFileProviderDomainSetup {
            let relay = ChangeSignalRelay(
                probe: { try store.dataVersion() },
                containerProbe: {
                    try ProviderContainerChangeResolver.changes(
                        store: store, account: account, after: $0)
                },
                signaling: ProductionPathSignaling(
                    recorder: recorder,
                    materializedContainerIDs: [chatParent],
                    didEvict: { try materializations.evict($0) }))
            try relay.start(observe: { _ in RecordingToken() })
            retainedRelays.retain(relay)
            return FileProviderDomainSetupResult(
                rootURL: URL(fileURLWithPath: "/synthetic/GramDrive"))
        }
        let health = StartupHealthScript([
            .running(previewSnapshot(accounts: nil, finderContentState: .preparing)),
            .running(
                previewSnapshot(
                    accounts: [
                        AccountHealthSummary(
                            accountId: 7, displayName: "Account", authState: "authorized")
                    ],
                    finderContentState: .ready)),
        ])
        let model = CompanionViewModel(
            backend: InMemoryCompanionBackend(healthProvider: { health.next() }),
            diskProbe: FixedDiskSpaceProbe(available: 1_000_000),
            accountLabel: "Account",
            domainSetup: setup,
            onboardingStore: InMemoryOnboardingCompletionStore(completed: true))

        await model.startAgentSession()

        for (identifier, expected) in current {
            let exposed = try await materializations.open(identifier) {
                let (url, version) = try await productionFetch(identifier)
                #expect(version.contentVersion == Data(expected.version.utf8))
                return url
            }
            #expect(exposed == expected.bytes)
        }
        #expect(try materializations.existingBytes(for: attachment) == unchanged)
        #expect(try materializations.existingBytes(for: deleted) == unchanged)
        #expect(try materializations.existingBytes(for: unrelated) == unchanged)
        #expect(
            Array(recorder.events.prefix(3))
                == current.keys.sorted().map { "evict:\($0)" },
            "the retained-state startup must evict stale bytes before change publication")
    }

    @Test("Startup bootstrap and later generated versions evict beneath materialized containers")
    func productionPathEvictsGeneratedVersionsBeforePublication() throws {
        let account = AccountInfo(
            accountId: 7,
            sourceKind: .localTdlib,
            displayName: "Account",
            authState: "authorized",
            namespaceVersion: 1,
            displayTimezone: "UTC",
            rootItemId: "root")
        let store = ScriptedStore(account: account)
        var generated = ItemMetadata(
            contractVersion: 1,
            id: "chat-json",
            parent: "chat-parent",
            kind: .generatedDoc,
            isDirectory: false,
            displayName: ".chat.json",
            safeName: ".chat.json",
            metadataVersion: "m1",
            mimeType: "application/json",
            logicalSize: 3,
            attachmentLogicalKind: nil,
            attachmentRepresentation: nil,
            attachmentFidelity: nil,
            attachmentSourceName: nil,
            attachmentExactSize: nil,
            contentVersion: "v1",
            availability: .fetchable,
            createdAtMs: 1,
            modifiedAtMs: 2,
            deletedAtMs: nil)
        store.apply(generated)
        store.apply(
            ItemMetadata(
                contractVersion: 1,
                id: "attachment",
                parent: "chat-parent",
                kind: .attachment,
                isDirectory: false,
                displayName: "attachment.bin",
                safeName: "attachment.bin",
                metadataVersion: "m1",
                mimeType: "application/octet-stream",
                logicalSize: 3,
                attachmentLogicalKind: nil,
                attachmentRepresentation: nil,
                attachmentFidelity: nil,
                attachmentSourceName: nil,
                attachmentExactSize: 3,
                contentVersion: "v1",
                availability: .fetchable,
                createdAtMs: 1,
                modifiedAtMs: 2,
                deletedAtMs: nil))
        var deleted = generated
        deleted.id = "deleted-generated"
        deleted.safeName = ".deleted.json"
        deleted.deletedAtMs = 3
        store.apply(deleted)

        let recorder = DispatchRecorder()
        let relay = ChangeSignalRelay(
            probe: { try store.dataVersion() },
            containerProbe: {
                try ProviderContainerChangeResolver.changes(
                    store: store, account: account, after: $0)
            },
            signaling: ProductionPathSignaling(
                recorder: recorder,
                materializedContainerIDs: ["chat-parent"]))
        var ring: (@Sendable () -> Void)?
        try relay.start(observe: { handler in
            ring = handler
            return RecordingToken()
        })

        generated.contentVersion = "v2"
        generated.metadataVersion = "m2"
        generated.modifiedAtMs = 4
        store.apply(generated)
        store.stampedDataVersion = 2
        ring?()

        #expect(
            recorder.events.filter { $0.hasPrefix("evict:") }
                == ["evict:chat-json", "evict:chat-json"],
            "startup replays the migrated journal and the next version reuses the same path")
        #expect(
            !recorder.events.contains("evict:attachment")
                && !recorder.events.contains("evict:deleted-generated"),
            "attachments and deleted generated rows are never eviction candidates")
        #expect(
            recorder.events.first == "evict:chat-json",
            "eviction must precede initial generated-version publication")
        let secondEviction = recorder.events.lastIndex(of: "evict:chat-json")
        let lastSignal = recorder.events.lastIndex {
            $0.hasPrefix("signal:")
        }
        #expect(
            secondEviction != nil && lastSignal != nil && secondEviction! < lastSignal!,
            "the later generated version must also evict before publication")
    }

    @Test("A change without generated candidates signals without reading materialized state")
    func noGeneratedCandidatesBypassMaterializedEnumeration() {
        let recorder = DispatchRecorder()
        let enumerator = ScriptedMaterializedEnumerator(pages: [], holdListing: true)
        let dispatcher = ProviderChangeDispatcher(
            materializedEnumerator: enumerator,
            evict: { identifier, completion in
                recorder.record("evict:\(identifier.rawValue)")
                completion(nil)
            },
            signal: { identifier, completion in
                recorder.record("signal:\(identifier.rawValue)")
                completion(nil)
            })

        dispatcher.dispatch(
            includeRoot: false,
            changedContainers: [],
            evictingGeneratedItems: [],
            completionHandler: { recorder.finish($0) })

        #expect(
            recorder.events == [
                "signal:\(NSFileProviderItemIdentifier.workingSet.rawValue)"
            ])
        #expect(!enumerator.usedMaterializedInitialPage)
        #expect(recorder.error == nil)
    }

    @Test("Generated materializations are evicted before deduplicated change signals")
    func generatedEvictionPrecedesSignals() {
        let recorder = DispatchRecorder()
        let generated = generatedChange("messages-md", parent: "month-parent")
        let parent = NSFileProviderItemIdentifier("month-parent")
        let dispatcher = ProviderChangeDispatcher(
            materializedEnumerator: ScriptedMaterializedEnumerator(
                pages: [[materializedContainer("month-parent")]]),
            evict: { identifier, completion in
                recorder.record("evict:\(identifier.rawValue)")
                completion(ProbeDown())
            },
            signal: { identifier, completion in
                recorder.record("signal:\(identifier.rawValue)")
                completion(identifier == .workingSet ? SignalDown() : nil)
            })

        dispatcher.dispatch(
            includeRoot: true,
            changedContainers: [.rootContainer, parent, parent],
            evictingGeneratedItems: [generated, generated],
            completionHandler: { recorder.finish($0) })

        #expect(
            recorder.events == [
                "evict:messages-md",
                "signal:\(NSFileProviderItemIdentifier.workingSet.rawValue)",
                "signal:\(NSFileProviderItemIdentifier.rootContainer.rawValue)",
                "signal:month-parent",
            ])
        #expect(recorder.error is ProbeDown)
    }

    @Test("A materialized parent evicts every changed generated child before publication")
    func materializedParentEvictsAllGeneratedChildren() {
        let recorder = DispatchRecorder()
        let dispatcher = ProviderChangeDispatcher(
            materializedEnumerator: ScriptedMaterializedEnumerator(
                pages: [[materializedContainer("chat-parent")]]),
            evict: { identifier, completion in
                recorder.record("evict:\(identifier.rawValue)")
                completion(nil)
            },
            signal: { identifier, completion in
                recorder.record("signal:\(identifier.rawValue)")
                completion(nil)
            })

        dispatcher.dispatch(
            includeRoot: false,
            changedContainers: [NSFileProviderItemIdentifier("chat-parent")],
            evictingGeneratedItems: [
                generatedChange("messages-md", parent: "chat-parent"),
                generatedChange("messages-ndjson", parent: "chat-parent"),
                generatedChange("other-chat-json", parent: "other-parent"),
            ],
            completionHandler: { recorder.finish($0) })

        #expect(
            recorder.events == [
                "evict:messages-md",
                "evict:messages-ndjson",
                "signal:\(NSFileProviderItemIdentifier.workingSet.rawValue)",
                "signal:chat-parent",
            ],
            "all generated siblings must evict before publication; unrelated parents must not")
        #expect(recorder.error == nil)
    }

    @Test("Startup backlog evicts only generated items actually materialized by File Provider")
    func startupBacklogIntersectsMaterializedSet() {
        let recorder = DispatchRecorder()
        let candidates = (0..<4_140).map {
            generatedChange("generated-\($0)", parent: "parent-\($0)")
        }
        let materialized = [7, 900, 4_139]
        let enumerator = ScriptedMaterializedEnumerator(
            pages: [
                [
                    materializedContainer("parent-\(materialized[0])"),
                    materializedContainer("ordinary-parent"),
                ],
                [
                    materializedContainer("parent-\(materialized[1])"),
                    materializedContainer("parent-\(materialized[2])"),
                ],
            ])
        let dispatcher = ProviderChangeDispatcher(
            materializedEnumerator: enumerator,
            evict: { identifier, completion in
                recorder.record("evict:\(identifier.rawValue)")
                completion(nil)
            },
            signal: { identifier, completion in
                recorder.record("signal:\(identifier.rawValue)")
                completion(nil)
            })

        dispatcher.dispatch(
            includeRoot: false,
            changedContainers: [],
            evictingGeneratedItems: candidates,
            completionHandler: { recorder.finish($0) })

        #expect(
            recorder.events == [
                "evict:generated-7",
                "evict:generated-900",
                "evict:generated-4139",
                "signal:\(NSFileProviderItemIdentifier.workingSet.rawValue)",
            ],
            "the build-149 lifetime journal must not become 4,140 serial File Provider evictions")
        #expect(recorder.error == nil)
        #expect(
            enumerator.usedMaterializedInitialPage,
            "the materialized-set API requires an empty NSData initial page")
    }

    @Test("A blocked materialized-set read is cancelled and never becomes an empty-set success")
    func materializedSelectionTimeoutIsBoundedAndFailClosed() {
        let recorder = DispatchRecorder()
        let timeout = ManualTimeout()
        let enumerator = ScriptedMaterializedEnumerator(pages: [], holdListing: true)
        let dispatcher = ProviderChangeDispatcher(
            materializedEnumerator: enumerator,
            scheduleSelectionTimeout: timeout.schedule,
            evict: { identifier, completion in
                recorder.record("evict:\(identifier.rawValue)")
                completion(nil)
            },
            signal: { identifier, completion in
                recorder.record("signal:\(identifier.rawValue)")
                completion(nil)
            })

        dispatcher.dispatch(
            includeRoot: false,
            changedContainers: [],
            evictingGeneratedItems: [generatedChange("generated")],
            completionHandler: { recorder.finish($0) })
        #expect(recorder.events.isEmpty)

        timeout.fire()

        #expect(enumerator.wasInvalidated)
        #expect(
            recorder.events == [
                "signal:\(NSFileProviderItemIdentifier.workingSet.rawValue)"
            ])
        #expect(recorder.error != nil, "failed selection is not legitimate absence")
    }

    @Test("A materialized-set enumeration error is preserved while signals still publish")
    func materializedSelectionErrorIsFailClosed() {
        let recorder = DispatchRecorder()
        let enumerator = ScriptedMaterializedEnumerator(
            pages: [],
            listingError: ProbeDown())
        let dispatcher = ProviderChangeDispatcher(
            materializedEnumerator: enumerator,
            evict: { identifier, completion in
                recorder.record("evict:\(identifier.rawValue)")
                completion(nil)
            },
            signal: { identifier, completion in
                recorder.record("signal:\(identifier.rawValue)")
                completion(nil)
            })

        dispatcher.dispatch(
            includeRoot: false,
            changedContainers: [],
            evictingGeneratedItems: [generatedChange("generated")],
            completionHandler: { recorder.finish($0) })

        #expect(
            recorder.events == [
                "signal:\(NSFileProviderItemIdentifier.workingSet.rawValue)"
            ])
        #expect(recorder.error is ProbeDown)
        #expect(!enumerator.wasInvalidated)
    }

    @Test("A cancelled watchdog cannot invalidate an already completed materialized read")
    func lateTimeoutAfterSuccessIsInert() {
        let recorder = DispatchRecorder()
        let timeout = RacyTimeout()
        let enumerator = ScriptedMaterializedEnumerator(
            pages: [[materializedContainer("parent")]])
        let dispatcher = ProviderChangeDispatcher(
            materializedEnumerator: enumerator,
            scheduleSelectionTimeout: timeout.schedule,
            evict: { identifier, completion in
                recorder.record("evict:\(identifier.rawValue)")
                completion(nil)
            },
            signal: { identifier, completion in
                recorder.record("signal:\(identifier.rawValue)")
                completion(nil)
            })

        dispatcher.dispatch(
            includeRoot: false,
            changedContainers: [],
            evictingGeneratedItems: [generatedChange("generated")],
            completionHandler: { recorder.finish($0) })
        let completedEvents = recorder.events

        timeout.fire()

        #expect(!enumerator.wasInvalidated)
        #expect(recorder.events == completedEvents)
        #expect(recorder.error == nil)
    }

    @Test("Start probes once — covering rings missed while not running — and signals")
    func startSignalsOnce() throws {
        let signaling = RecordingSignaling()
        let probe = ScriptedProbe([.success(1)])
        let relay = ChangeSignalRelay(probe: { try probe.next() }, signaling: signaling)
        let token = RecordingToken()
        try relay.start(observe: { _ in token })
        #expect(signaling.signalCount == 1, "the first probe always differs from 'never probed'")
        #expect(signaling.includeRootRequests == [true])
    }

    @Test("A ring with an unmoved stamp stays quiet; movement signals")
    func movementGates() throws {
        let signaling = RecordingSignaling()
        let probe = ScriptedProbe([.success(1), .success(1), .success(2)])
        let relay = ChangeSignalRelay(probe: { try probe.next() }, signaling: signaling)
        var ring: (@Sendable () -> Void)?
        try relay.start(observe: { handler in
            ring = handler
            return RecordingToken()
        })
        #expect(signaling.signalCount == 1)

        ring?()  // doorbell coalesced ring, nothing actually committed
        #expect(signaling.signalCount == 1, "no movement, no signal — the doorbell is advisory")

        ring?()  // a real foreign commit moved the stamp
        #expect(signaling.signalCount == 2)
        #expect(
            signaling.includeRootRequests == [true, false],
            "history/render commits signal only the working-set change feed")
    }

    @Test("A moved journal signals every changed item's parent container")
    func movementSignalsChangedContainers() throws {
        let signaling = RecordingSignaling()
        let probe = ScriptedProbe([.success(1), .success(2)])
        let snapshots = LockedSnapshots([
            ProviderContainerChanges(
                journal: ChangeJournalState(instanceId: "life", latestSequence: 10),
                containers: []),
            ProviderContainerChanges(
                journal: ChangeJournalState(instanceId: "life", latestSequence: 12),
                containers: [
                    NSFileProviderItemIdentifier("chat-parent"),
                    NSFileProviderItemIdentifier("month-parent"),
                ],
                generatedItems: [generatedChange("messages-md", parent: "month-parent")]),
        ])
        let relay = ChangeSignalRelay(
            probe: { try probe.next() },
            containerProbe: { _ in snapshots.next() },
            signaling: signaling)
        var ring: (@Sendable () -> Void)?
        try relay.start(observe: { handler in
            ring = handler
            return RecordingToken()
        })
        ring?()

        #expect(
            signaling.changedContainerRequests == [
                [],
                ["chat-parent", "month-parent"],
            ])
        #expect(signaling.evictedGeneratedItemRequests == [[], ["messages-md"]])
    }

    @Test("Journal deltas resolve generated metadata to its parent container")
    func journalDeltaResolvesParent() throws {
        let account = AccountInfo(
            accountId: 7,
            sourceKind: .localTdlib,
            displayName: "Account",
            authState: "authorized",
            namespaceVersion: 1,
            displayTimezone: "UTC",
            rootItemId: "root")
        let store = ScriptedStore(account: account)
        store.apply(
            ItemMetadata(
                contractVersion: 1,
                id: "chat-json",
                parent: "chat-parent",
                kind: .generatedDoc,
                isDirectory: false,
                displayName: ".chat.json",
                safeName: ".chat.json",
                metadataVersion: "m1",
                mimeType: "application/json",
                logicalSize: 3,
                attachmentLogicalKind: nil,
                attachmentRepresentation: nil,
                attachmentFidelity: nil,
                attachmentSourceName: nil,
                attachmentExactSize: nil,
                contentVersion: "v1",
                availability: .fetchable,
                createdAtMs: 1,
                modifiedAtMs: 2,
                deletedAtMs: nil))
        store.apply(
            ItemMetadata(
                contractVersion: 1,
                id: "attachment",
                parent: "chat-parent",
                kind: .attachment,
                isDirectory: false,
                displayName: "attachment.bin",
                safeName: "attachment.bin",
                metadataVersion: "m1",
                mimeType: "application/octet-stream",
                logicalSize: 3,
                attachmentLogicalKind: nil,
                attachmentRepresentation: nil,
                attachmentFidelity: nil,
                attachmentSourceName: nil,
                attachmentExactSize: 3,
                contentVersion: "v1",
                availability: .fetchable,
                createdAtMs: 1,
                modifiedAtMs: 2,
                deletedAtMs: nil))
        store.apply(
            ItemMetadata(
                contractVersion: 1,
                id: "deleted-chat-json",
                parent: "chat-parent",
                kind: .generatedDoc,
                isDirectory: false,
                displayName: ".deleted-chat.json",
                safeName: ".deleted-chat.json",
                metadataVersion: "m1",
                mimeType: "application/json",
                logicalSize: 3,
                attachmentLogicalKind: nil,
                attachmentRepresentation: nil,
                attachmentFidelity: nil,
                attachmentSourceName: nil,
                attachmentExactSize: nil,
                contentVersion: "v1",
                availability: .fetchable,
                createdAtMs: 1,
                modifiedAtMs: 3,
                deletedAtMs: 3))

        let changes = try ProviderContainerChangeResolver.changes(
            store: store,
            account: account,
            after: ChangeJournalState(instanceId: "life-1", latestSequence: 0))

        #expect(changes.journal.latestSequence == 3)
        #expect(changes.containers.map { $0.rawValue } == ["chat-parent"])
        #expect(
            changes.generatedItems.map(\.item.rawValue) == ["chat-json"]
                && changes.generatedItems.map(\.parent.rawValue) == ["chat-parent"],
            "ordinary attachments and deleted generated nodes must never be evicted")
    }

    @Test("A failing probe signals nothing; the next successful ring recovers")
    func probeFailureIsQuiet() throws {
        let signaling = RecordingSignaling()
        let probe = ScriptedProbe([.failure(ProbeDown()), .success(5)])
        let relay = ChangeSignalRelay(probe: { try probe.next() }, signaling: signaling)
        var ring: (@Sendable () -> Void)?
        try relay.start(observe: { handler in
            ring = handler
            return RecordingToken()
        })
        #expect(signaling.signalCount == 0, "a store mid-recovery is not a change")

        ring?()
        #expect(signaling.signalCount == 1)
    }

    @Test("A failed materialized-set proof does not advance the relay checkpoint")
    func signalingFailureRetriesTheSameVersion() throws {
        let signaling = RecordingSignaling(completionErrors: [SignalDown(), nil])
        let probe = ScriptedProbe([.success(5), .success(5)])
        let relay = ChangeSignalRelay(probe: { try probe.next() }, signaling: signaling)
        var ring: (@Sendable () -> Void)?
        try relay.start(observe: { handler in
            ring = handler
            return RecordingToken()
        })
        #expect(signaling.signalCount == 1)

        ring?()

        #expect(
            signaling.signalCount == 2,
            "a failed materialized-set read is retried, never laundered into absence")
    }

    @Test("A newer doorbell cannot discard an older failed dispatch checkpoint")
    func overlappingFailureRetriesTheUnconfirmedJournalRange() throws {
        let signaling = DelayedSignaling()
        let probe = ScriptedProbe([.success(1), .success(2), .success(2)])
        let snapshots = LockedSnapshots([
            ProviderContainerChanges(
                journal: ChangeJournalState(instanceId: "life", latestSequence: 1),
                containers: [],
                generatedItems: [generatedChange("generated-a")]),
            ProviderContainerChanges(
                journal: ChangeJournalState(instanceId: "life", latestSequence: 2),
                containers: [],
                generatedItems: [
                    generatedChange("generated-a"),
                    generatedChange("generated-b"),
                ]),
        ])
        let relay = ChangeSignalRelay(
            probe: { try probe.next() },
            containerProbe: { prior in snapshots.next(after: prior) },
            signaling: signaling)
        var ring: (@Sendable () -> Void)?
        try relay.start(observe: { handler in
            ring = handler
            return RecordingToken()
        })
        #expect(signaling.signalCount == 1)

        ring?()
        #expect(
            signaling.signalCount == 1,
            "a newer check must wait while the older checkpoint is unconfirmed")

        signaling.complete(0, with: SignalDown())
        #expect(signaling.signalCount == 2)
        #expect(
            signaling.evictedGeneratedItemRequests == [
                ["generated-a"],
                ["generated-a", "generated-b"],
            ],
            "the retry must restart at the last confirmed journal, not after failed item A")
        #expect(snapshots.priorSequences == [nil, nil])

        signaling.complete(1, with: nil)
        ring?()
        #expect(signaling.signalCount == 2, "the successful retry confirms the newer version")
    }

    @Test("Agent replacement waits for and retries an older failed journal range")
    func replacementRetriesTheUnconfirmedJournalRange() throws {
        let signaling = DelayedSignaling()
        let probe = ScriptedProbe([.success(1)])
        let snapshots = LockedSnapshots([
            ProviderContainerChanges(
                journal: ChangeJournalState(instanceId: "life", latestSequence: 1),
                containers: [],
                generatedItems: [generatedChange("generated-a")]),
            ProviderContainerChanges(
                journal: ChangeJournalState(instanceId: "life", latestSequence: 2),
                containers: [],
                generatedItems: [
                    generatedChange("generated-a"),
                    generatedChange("generated-b"),
                ]),
        ])
        let relay = ChangeSignalRelay(
            probe: { try probe.next() },
            containerProbe: { prior in snapshots.next(after: prior) },
            signaling: signaling)
        try relay.start(observe: { _ in RecordingToken() })

        relay.signalEnumeratorsAfterAgentReplacement()
        #expect(
            signaling.signalCount == 1,
            "replacement must share the relay's single checkpoint lane")

        signaling.complete(0, with: SignalDown())
        #expect(signaling.signalCount == 2)
        #expect(signaling.includeRootRequests == [true, true])
        #expect(
            signaling.evictedGeneratedItemRequests == [
                ["generated-a"],
                ["generated-a", "generated-b"],
            ])
        #expect(snapshots.priorSequences == [nil, nil])
        signaling.complete(1, with: nil)
    }

    @Test("Stop cancels the observation")
    func stopCancels() throws {
        let relay = ChangeSignalRelay(probe: { 1 }, signaling: RecordingSignaling())
        let token = RecordingToken()
        try relay.start(observe: { _ in token })
        #expect(!token.cancelled)
        relay.stop()
        #expect(token.cancelled)
    }

    @Test("Agent replacement re-signals root even without a new state stamp")
    func replacementSignalsEnumerators() {
        let signaling = RecordingSignaling()
        let relay = ChangeSignalRelay(
            probe: { 1 },
            containerProbe: { _ in
                ProviderContainerChanges(
                    journal: ChangeJournalState(instanceId: "life", latestSequence: 4),
                    containers: [NSFileProviderItemIdentifier("chat-parent")],
                    generatedItems: [generatedChange("messages-md", parent: "chat-parent")])
            },
            signaling: signaling)

        relay.signalEnumeratorsAfterAgentReplacement()

        #expect(signaling.includeRootRequests == [true])
        #expect(signaling.changedContainerRequests == [["chat-parent"]])
        #expect(signaling.evictedGeneratedItemRequests == [["messages-md"]])
    }
}

private final class LockedSnapshots: @unchecked Sendable {
    private let lock = NSLock()
    private var snapshots: [ProviderContainerChanges]
    private var recordedPriorSequences: [Int64?] = []

    init(_ snapshots: [ProviderContainerChanges]) {
        self.snapshots = snapshots
    }

    func next() -> ProviderContainerChanges {
        lock.lock()
        defer { lock.unlock() }
        precondition(!snapshots.isEmpty)
        return snapshots.removeFirst()
    }

    var priorSequences: [Int64?] {
        lock.lock()
        defer { lock.unlock() }
        return recordedPriorSequences
    }

    func next(after prior: ChangeJournalState?) -> ProviderContainerChanges {
        lock.lock()
        defer { lock.unlock() }
        recordedPriorSequences.append(prior?.latestSequence)
        precondition(!snapshots.isEmpty)
        return snapshots.removeFirst()
    }
}
