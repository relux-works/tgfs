import Darwin
import FileProvider
import Foundation
import GramDriveCore
import GramDriveSupport
import Testing

@testable import GramDriveFileProvider

// MARK: - Fixtures

private let accountId: Int64 = 7
private let rootId = "acct-root"

private func makeAccount() -> AccountInfo {
    AccountInfo(
        accountId: accountId,
        sourceKind: .localTdlib,
        displayName: "Test Account",
        authState: "authorized",
        namespaceVersion: 1,
        rootItemId: rootId
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

private func file(
    id: String,
    size: UInt64? = 5,
    contentVersion: String? = "v1",
    availability: ItemAvailability = .fetchable
) -> ItemMetadata {
    ItemMetadata(
        id: id, parent: rootId, kind: .attachment, isDirectory: false,
        displayName: id, safeName: id + ".bin", metadataVersion: "m1",
        mimeType: "application/octet-stream", logicalSize: size,
        contentVersion: contentVersion,
        availability: availability, createdAtMs: 1_000, modifiedAtMs: 1_000, deletedAtMs: nil
    )
}

private func directory(id: String) -> ItemMetadata {
    ItemMetadata(
        id: id, parent: rootId, kind: .chat, isDirectory: true,
        displayName: id, safeName: id, metadataVersion: "m1",
        mimeType: nil, logicalSize: nil, contentVersion: nil,
        availability: .fetchable, createdAtMs: 1_000, modifiedAtMs: 1_000, deletedAtMs: nil
    )
}

/// A fetcher over a scripted store and scripted hydration; every test
/// starts here.
private struct Harness {
    let store: ScriptedStore
    let hydration: ScriptedHydration
    let fetcher: ContentFetcher

    init(
        scratch: URL,
        configuration: ContentFetcherConfiguration = ContentFetcherConfiguration()
    ) {
        let store = ScriptedStore(account: makeAccount())
        store.apply(rootItem())
        self.store = store
        self.hydration = ScriptedHydration()
        self.fetcher = ContentFetcher(
            hydration: hydration,
            scratchDirectory: { scratch },
            configuration: configuration)
    }

    /// Runs one fetch to completion; the returned outcome is the recorded
    /// completion call.
    func fetch(
        _ identifier: NSFileProviderItemIdentifier,
        version: NSFileProviderItemVersion? = nil
    ) async -> FetchOutcome {
        let future = TestFuture<FetchOutcome>()
        _ = start(identifier, version: version, future: future)
        return await future.settled
    }

    /// Starts one fetch without awaiting it.
    func start(
        _ identifier: NSFileProviderItemIdentifier,
        version: NSFileProviderItemVersion? = nil,
        future: TestFuture<FetchOutcome>
    ) -> Progress {
        fetcher.fetchContents(
            itemIdentifier: identifier,
            requestedVersion: version,
            context: { [store] in (account: makeAccount(), store: store) },
            completionHandler: { url, item, error in
                future.fulfill(FetchOutcome(url: url, item: item, error: error))
            })
    }
}

@Suite("Content fetch")
struct ContentFetcherTests {
    // MARK: Success path

    @Test("A fetch materializes verified staged content atomically")
    func successMaterializesVerifiedContent() async throws {
        try await withFetchScratchDirectory { scratch in
            let staging = scratch.appendingPathComponent("cache", isDirectory: true)
            try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            let stagedPath = try stageContent(Data("hello".utf8), in: staging)
            harness.hydration.enqueueSuccess(
                HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 5),
                progress: [
                    HydrationProgress(bytesTransferred: 2, bytesTotal: 5),
                    HydrationProgress(bytesTransferred: 5, bytesTotal: 5),
                ])

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            #expect(outcome.error == nil)
            let url = try #require(outcome.url)
            #expect(try Data(contentsOf: url) == Data("hello".utf8))
            // A copy in the extension's scratch, never the staged original.
            #expect(url.path != stagedPath)
            #expect(url.path.hasPrefix(scratch.path))
            #expect(FileManager.default.fileExists(atPath: stagedPath))
            // The returned item is the fetched snapshot.
            #expect(outcome.item?.itemIdentifier.rawValue == "f1")
            // The request pinned the observed content version.
            #expect(harness.hydration.requests.count == 1)
            #expect(harness.hydration.requests.first?.contentVersion == "v1")
            #expect(harness.hydration.requests.first?.accountId == accountId)
        }
    }

    @Test("Progress counts the hydration's bytes and completes with them")
    func progressReflectsHydration() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
            harness.hydration.enqueueSuccess(
                HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 5),
                progress: [HydrationProgress(bytesTransferred: 2, bytesTotal: 5)])

            let future = TestFuture<FetchOutcome>()
            let progress = harness.start(
                NSFileProviderItemIdentifier("f1"), future: future)
            let outcome = await future.settled

            #expect(outcome.error == nil)
            #expect(progress.totalUnitCount == 5)
            #expect(progress.completedUnitCount == 5)
            #expect(progress.fractionCompleted == 1.0)
        }
    }

    @Test("A stale requested version is served as the current version")
    func staleRequestedVersionServesCurrent() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
            harness.hydration.enqueueSuccess(
                HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 5))

            let stale = NSFileProviderItemVersion(
                contentVersion: Data("v0".utf8), metadataVersion: Data("m0".utf8))
            let outcome = await harness.fetch(
                NSFileProviderItemIdentifier("f1"), version: stale)

            #expect(outcome.error == nil)
            #expect(outcome.url != nil)
            // The current version was pinned and delivered; the returned
            // item carries it for the system to reconcile.
            #expect(harness.hydration.requests.first?.contentVersion == "v1")
            #expect(
                outcome.fetchedItem?.itemVersion.contentVersion == Data("v1".utf8))
        }
    }

    // MARK: Refusals before any IPC (POL-4, shape)

    @Test(
        "POL-4 content is refused without contacting the agent",
        arguments: [ItemAvailability.restricted, .unavailable])
    func pol4RefusedWithoutIPC(availability: ItemAvailability) async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1", availability: availability))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectCocoaError(.fileReadNoPermission)
            #expect(harness.hydration.requests.isEmpty)
        }
    }

    @Test("A directory has no content to fetch")
    func directoryRefused() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(directory(id: "d1"))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("d1"))

            outcome.expectCocoaError(.featureUnsupported)
            #expect(harness.hydration.requests.isEmpty)
        }
    }

    @Test("An unknown item answers noSuchItem without contacting the agent")
    func unknownItemRefused() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("ghost"))

            outcome.expectProviderError(.noSuchItem)
            #expect(harness.hydration.requests.isEmpty)
        }
    }

    @Test("A POL-3 tombstone answers noSuchItem")
    func tombstoneRefused() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            harness.store.tombstone(id: "f1", atMs: 2_000)

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectProviderError(.noSuchItem)
            #expect(harness.hydration.requests.isEmpty)
        }
    }

    // MARK: Version races (SYNC-042 as seen from the provider)

    @Test("A mid-fetch version conflict restarts once against the fresh snapshot")
    func versionConflictRestartsOnce() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1", contentVersion: "v1"))
            let stagedPath = try stageContent(Data("fresh".utf8), in: scratch)
            harness.hydration.enqueue { [store = harness.store] _, _ in
                // The engine observed a newer version mid-fetch; by the
                // time the conflict answer arrives, the store shows it.
                store.apply(file(id: "f1", contentVersion: "v2"))
                throw HydrationFailure(category: .versionConflict, detail: "stale pin")
            }
            harness.hydration.enqueueSuccess(
                HydratedContent(stagedPath: stagedPath, contentVersion: "v2", byteCount: 5))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            #expect(outcome.error == nil)
            #expect(outcome.url != nil)
            let pins = harness.hydration.requests.map(\.contentVersion)
            #expect(pins == ["v1", "v2"])
            #expect(
                outcome.fetchedItem?.itemVersion.contentVersion == Data("v2".utf8))
        }
    }

    @Test("A conflict the store has not observed yet fails transiently, not in a spin")
    func conflictWithoutStoreMovementFailsSafely() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1", contentVersion: "v1"))
            harness.hydration.enqueueFailure(
                HydrationFailure(category: .versionConflict, detail: "stale pin"))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectProviderError(.serverUnreachable)
            #expect(harness.hydration.requests.count == 1)
        }
    }

    @Test("A second conflict fails safely instead of chasing versions")
    func secondConflictFailsSafely() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1", contentVersion: "v1"))
            harness.hydration.enqueue { [store = harness.store] _, _ in
                store.apply(file(id: "f1", contentVersion: "v2"))
                throw HydrationFailure(category: .versionConflict, detail: "stale pin")
            }
            harness.hydration.enqueue { [store = harness.store] _, _ in
                store.apply(file(id: "f1", contentVersion: "v3"))
                throw HydrationFailure(category: .versionConflict, detail: "stale pin")
            }

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectProviderError(.serverUnreachable)
            #expect(harness.hydration.requests.count == 2)
        }
    }

    @Test("Staged bytes reporting a foreign version are treated as the conflict they are")
    func stagedVersionMismatchIsAConflict() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1", contentVersion: "v1"))
            let stagedPath = try stageContent(Data("wrong".utf8), in: scratch)
            harness.hydration.enqueueSuccess(
                HydratedContent(stagedPath: stagedPath, contentVersion: "v9", byteCount: 5))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            // The store never observed v9, so the restart refuses to spin.
            outcome.expectProviderError(.serverUnreachable)
        }
    }

    @Test("An item that turns restricted across a restart is refused, not fetched")
    func restartReChecksAvailability() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1", contentVersion: "v1"))
            harness.hydration.enqueue { [store = harness.store] _, _ in
                store.apply(
                    file(id: "f1", contentVersion: "v2", availability: .restricted))
                throw HydrationFailure(category: .versionConflict, detail: "stale pin")
            }

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectCocoaError(.fileReadNoPermission)
            #expect(harness.hydration.requests.count == 1)
        }
    }

    // MARK: Materialization guarantees (PRD-043)

    @Test("A staged file shorter than promised is never published")
    func partialContentNeverPublished() async throws {
        try await withFetchScratchDirectory { scratch in
            let staging = scratch.appendingPathComponent("cache", isDirectory: true)
            try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1", size: 100))
            let stagedPath = try stageContent(Data("short".utf8), in: staging)
            harness.hydration.enqueueSuccess(
                HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 100))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectProviderError(.cannotSynchronize)
            // The failed copy was deleted, not left for anyone to find.
            let scratchLeftovers = try FileManager.default.contentsOfDirectory(atPath: scratch.path)
                .filter { $0.hasPrefix("fetch-") }
            #expect(scratchLeftovers.isEmpty)
        }
    }

    @Test("A staged file that vanished is a transient service condition")
    func missingStagedFileIsTransient() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            harness.hydration.enqueueSuccess(
                HydratedContent(
                    stagedPath: scratch.appendingPathComponent("gone.bin").path,
                    contentVersion: "v1",
                    byteCount: 5))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectProviderError(.serverUnreachable)
        }
    }

    // MARK: Agent unavailable and failure mapping

    @Test("An unreachable agent maps to serverUnreachable")
    func agentUnavailableMapsToServerUnreachable() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            harness.hydration.enqueueTransportFailure(
                .agentUnavailable(path: "/nowhere/hydration.sock"))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectProviderError(.serverUnreachable)
        }
    }

    @Test(
        "A raw socket fault below the protocol layer maps to serverUnreachable",
        arguments: [EPIPE, ECONNRESET, EINTR, EMFILE, EPERM, ENOTCONN])
    func rawSocketFaultMapsToServerUnreachable(code: Int32) async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            // What the client throws on socket() exhaustion, an EPIPE send to
            // a dead agent, or an interrupted/reset read — none of which are
            // HydrationTransportError, so they must not escape unmapped.
            harness.hydration.enqueueSocketFailure(
                .failed(operation: "read", code: code))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectProviderError(.serverUnreachable)
        }
    }

    @Test("An unrepresentable socket path maps to serverUnreachable")
    func unrepresentableSocketPathMapsToServerUnreachable() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            harness.hydration.enqueueSocketFailure(
                .pathUnrepresentable(path: "/some/very/long/socket/path"))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectProviderError(.serverUnreachable)
        }
    }

    @Test("A failure category the agent reports maps end to end")
    func reportedFailureMapsEndToEnd() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            harness.hydration.enqueueFailure(
                HydrationFailure(category: .authRequired, detail: "logged out"))

            let outcome = await harness.fetch(NSFileProviderItemIdentifier("f1"))

            outcome.expectProviderError(.notAuthenticated)
        }
    }

    @Test("Every wire category maps onto the provider error surface")
    func categoryMappingTable() {
        func code(_ category: HydrationFailureCategory) -> (domain: String, code: Int) {
            let error = ContentFetcher.providerError(
                for: HydrationFailure(category: category, detail: "test")) as NSError
            return (error.domain, error.code)
        }
        let provider = NSFileProviderError.errorDomain
        let cocoa = CocoaError.errorDomain
        #expect(code(.notFound) == (provider, NSFileProviderError.Code.noSuchItem.rawValue))
        #expect(code(.restricted) == (cocoa, CocoaError.Code.fileReadNoPermission.rawValue))
        #expect(
            code(.authRequired) == (provider, NSFileProviderError.Code.notAuthenticated.rawValue))
        #expect(code(.cancelled) == (cocoa, CocoaError.Code.userCancelled.rawValue))
        for transient in [HydrationFailureCategory.versionConflict, .rateLimited,
                          .sourceUnavailable, .draining, .busy]
        {
            #expect(
                code(transient)
                    == (provider, NSFileProviderError.Code.serverUnreachable.rawValue))
        }
        for broken in [HydrationFailureCategory.storage, .integrity, .internalError] {
            #expect(
                code(broken)
                    == (provider, NSFileProviderError.Code.cannotSynchronize.rawValue))
        }
    }

    // MARK: Cancellation

    @Test("Cancelling the returned Progress cancels a running hydration")
    func cancellationDuringHydration() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            let started = ArrivalCounter()
            harness.hydration.enqueue { _, _ in
                started.signal()
                // Parks until the surrounding task is cancelled — the
                // client contract's cancellation shape.
                try await Task.sleep(for: .seconds(600))
                throw HydrationFailure(category: .internalError, detail: "unreachable")
            }

            let future = TestFuture<FetchOutcome>()
            let progress = harness.start(NSFileProviderItemIdentifier("f1"), future: future)
            await started.waitFor(1)
            progress.cancel()
            let outcome = await future.settled

            outcome.expectCocoaError(.userCancelled)
            #expect(future.fulfillmentCount == 1)
        }
    }

    @Test("A fetch cancelled while queued never contacts the agent")
    func cancellationWhileQueued() async throws {
        try await withFetchScratchDirectory { scratch in
            var configuration = ContentFetcherConfiguration()
            configuration.maxConcurrentFetches = 1
            let harness = Harness(scratch: scratch, configuration: configuration)
            harness.store.apply(file(id: "f1"))
            harness.store.apply(file(id: "f2"))
            let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
            let started = ArrivalCounter()
            let release = ManualGate()
            harness.hydration.enqueue { _, _ in
                started.signal()
                await release.waitUntilOpen()
                return HydratedContent(stagedPath: stagedPath, contentVersion: "v1", byteCount: 5)
            }

            let first = TestFuture<FetchOutcome>()
            _ = harness.start(NSFileProviderItemIdentifier("f1"), future: first)
            await started.waitFor(1)

            let second = TestFuture<FetchOutcome>()
            let queuedProgress = harness.start(NSFileProviderItemIdentifier("f2"), future: second)
            queuedProgress.cancel()
            let queuedOutcome = await second.settled
            queuedOutcome.expectCocoaError(.userCancelled)

            release.open()
            let firstOutcome = await first.settled
            #expect(firstOutcome.error == nil)
            // The cancelled fetch never reached the agent.
            #expect(harness.hydration.requests.map(\.itemId) == ["f1"])
        }
    }

    @Test("cancelAll unwinds every in-flight fetch (the invalidation path)")
    func cancelAllUnwindsInFlight() async throws {
        try await withFetchScratchDirectory { scratch in
            let harness = Harness(scratch: scratch)
            harness.store.apply(file(id: "f1"))
            let started = ArrivalCounter()
            harness.hydration.enqueue { _, _ in
                started.signal()
                try await Task.sleep(for: .seconds(600))
                throw HydrationFailure(category: .internalError, detail: "unreachable")
            }

            let future = TestFuture<FetchOutcome>()
            _ = harness.start(NSFileProviderItemIdentifier("f1"), future: future)
            await started.waitFor(1)
            harness.fetcher.cancelAll()
            let outcome = await future.settled

            outcome.expectCocoaError(.userCancelled)
        }
    }

    // MARK: Concurrency bound (NFR-021)

    @Test("Concurrent fetches are bounded by the gate, and all complete")
    func concurrentFetchesBounded() async throws {
        try await withFetchScratchDirectory { scratch in
            var configuration = ContentFetcherConfiguration()
            configuration.maxConcurrentFetches = 2
            let harness = Harness(scratch: scratch, configuration: configuration)
            let stagedPath = try stageContent(Data("hello".utf8), in: scratch)
            let started = ArrivalCounter()
            let release = ManualGate()
            for index in 1...5 {
                harness.store.apply(file(id: "f\(index)"))
                harness.hydration.enqueue { _, _ in
                    started.signal()
                    await release.waitUntilOpen()
                    return HydratedContent(
                        stagedPath: stagedPath, contentVersion: "v1", byteCount: 5)
                }
            }

            var futures: [TestFuture<FetchOutcome>] = []
            for index in 1...5 {
                let future = TestFuture<FetchOutcome>()
                futures.append(future)
                _ = harness.start(NSFileProviderItemIdentifier("f\(index)"), future: future)
            }
            // Exactly the gate's width starts; the rest wait their turn.
            await started.waitFor(2)
            release.open()
            for future in futures {
                let outcome = await future.settled
                #expect(outcome.error == nil)
            }
            #expect(harness.hydration.concurrentHighWater == 2)
            #expect(harness.hydration.requests.count == 5)
        }
    }
}
