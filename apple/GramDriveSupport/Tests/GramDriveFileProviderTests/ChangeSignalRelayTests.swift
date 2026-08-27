import FileProvider
import Foundation
import GramDriveCore
import Testing

@testable import GramDriveFileProvider

/// Records working-set signals instead of talking to the file provider
/// daemon.
private final class RecordingSignaling: ProviderChangeSignaling, @unchecked Sendable {
    private let lock = NSLock()
    private var requests: [Bool] = []
    private var containers: [[String]] = []
    private var generatedItems: [[String]] = []

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
        evictingGeneratedItems: [NSFileProviderItemIdentifier],
        completionHandler: @escaping @Sendable ((any Error)?) -> Void
    ) {
        lock.lock()
        requests.append(includeRoot)
        containers.append(changedContainers.map(\.rawValue))
        generatedItems.append(evictingGeneratedItems.map(\.rawValue))
        lock.unlock()
        completionHandler(nil)
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
    @Test("Generated materializations are evicted before deduplicated change signals")
    func generatedEvictionPrecedesSignals() {
        let recorder = DispatchRecorder()
        let generated = NSFileProviderItemIdentifier("messages-md")
        let parent = NSFileProviderItemIdentifier("month-parent")
        let dispatcher = ProviderChangeDispatcher(
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
                generatedItems: [NSFileProviderItemIdentifier("messages-md")]),
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
            changes.generatedItems.map { $0.rawValue } == ["chat-json"],
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
                    generatedItems: [NSFileProviderItemIdentifier("messages-md")])
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

    init(_ snapshots: [ProviderContainerChanges]) {
        self.snapshots = snapshots
    }

    func next() -> ProviderContainerChanges {
        lock.lock()
        defer { lock.unlock() }
        precondition(!snapshots.isEmpty)
        return snapshots.removeFirst()
    }
}
