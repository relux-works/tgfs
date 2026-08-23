import Foundation
import GramDriveCore
import GramDriveSupport
import SQLite3
import Testing

@testable import GramDriveAgentCore

/// Hand-driven power-event source.
private final class FakePowerEventSource: PowerEventSource, @unchecked Sendable {
    private let lock = NSLock()
    private var handler: (@Sendable (PowerEvent) -> Void)?

    func observe(
        _ handler: @escaping @Sendable (PowerEvent) -> Void
    ) -> PowerEventObservation {
        lock.lock()
        self.handler = handler
        lock.unlock()
        return PowerEventObservation { [weak self] in
            self?.lock.lock()
            self?.handler = nil
            self?.lock.unlock()
        }
    }

    func emit(_ event: PowerEvent) {
        lock.lock()
        let handler = self.handler
        lock.unlock()
        handler?(event)
    }
}

private final class NoopProgressListener: ProgressListener {
    func onProgress(progress: TransferProgress) {}
}

private final class FakeNamespaceSession: AgentNamespaceSessionHosting, @unchecked Sendable {
    private let lock = NSLock()
    private(set) var closed = false
    private var priorities: [(Int64, AgentChatHistoryPriority)] = []

    func setChatHistoryPriority(chatId: Int64, priority: AgentChatHistoryPriority) throws {
        lock.lock()
        priorities.append((chatId, priority))
        lock.unlock()
    }

    func close() {
        lock.lock()
        closed = true
        lock.unlock()
    }

    func prioritySnapshot() -> [(Int64, AgentChatHistoryPriority)] {
        lock.lock()
        defer { lock.unlock() }
        return priorities
    }
}

private final class FakeNamespaceBootstrapper: AgentNamespaceBootstrapping,
    @unchecked Sendable
{
    private let lock = NSLock()
    private var listeners: [Int64: @Sendable (AgentNamespaceProgress) -> Void] = [:]
    private var hosted: [Int64: FakeNamespaceSession] = [:]
    private var starts: [Int64: Int] = [:]
    var failure: Error?

    func start(
        accountId: Int64,
        onProgress: @escaping @Sendable (AgentNamespaceProgress) -> Void
    ) throws -> any AgentNamespaceSessionHosting {
        lock.lock()
        defer { lock.unlock() }
        if let failure { throw failure }
        let session = FakeNamespaceSession()
        listeners[accountId] = onProgress
        hosted[accountId] = session
        starts[accountId, default: 0] += 1
        return session
    }

    func emit(_ progress: AgentNamespaceProgress, accountId: Int64) {
        lock.lock()
        let listener = listeners[accountId]
        lock.unlock()
        listener?(progress)
    }

    func session(accountId: Int64) -> FakeNamespaceSession? {
        lock.lock()
        defer { lock.unlock() }
        return hosted[accountId]
    }

    func startCount(accountId: Int64) -> Int {
        lock.lock()
        defer { lock.unlock() }
        return starts[accountId, default: 0]
    }
}

private enum FakeNamespaceError: Error {
    case unavailable
}

private enum TestCommitWatchdog {
    static func armed() -> Bool { true }
    static func failedToArm() -> Bool { false }
}

private func startedLifecycle(
    dataRoot: URL,
    grace: Duration = .seconds(5),
    cancelWait: Duration = .seconds(5),
    power: (any PowerEventSource)? = nil,
    namespaceBootstrapper: (any AgentNamespaceBootstrapping)? = nil,
    terminationCommitLease: Duration = .seconds(30)
) throws -> AgentLifecycle {
    let lifecycle = AgentLifecycle(
        configuration: AgentConfiguration(
            dataRoot: dataRoot,
            drainGracePeriod: grace,
            drainCancelWait: cancelWait,
            powerEvents: power,
            namespaceBootstrapper: namespaceBootstrapper,
            terminationCommitLease: terminationCommitLease))
    try lifecycle.start()
    return lifecycle
}

/// Seeds one account through the actual on-disk state boundary. Production
/// writes this row only through the core authorization owner; tests use the
/// minimum valid durable row to verify health is read-only with respect to it.
private func seedAuthorizedAccount(dataRoot: URL, accountId: Int64) throws {
    let layout = try sharedStateLayout(dataRoot: dataRoot.path)
    var database: OpaquePointer?
    let openResult = sqlite3_open_v2(
        layout.databaseFile,
        &database,
        SQLITE_OPEN_READWRITE,
        nil)
    guard openResult == SQLITE_OK, let database else {
        throw CocoaError(.fileReadUnknown)
    }
    defer { sqlite3_close(database) }

    let statementSQL = """
        INSERT INTO accounts (
            account_id, source_kind, display_name, auth_state, namespace_version,
            retention_mode, archive_mode, created_at_ms, updated_at_ms, display_timezone
        ) VALUES (?, 'local_tdlib', 'Private', 'authorized', 0, 'mirror', 0, 1, 1, 'UTC')
        """
    var statement: OpaquePointer?
    guard sqlite3_prepare_v2(database, statementSQL, -1, &statement, nil) == SQLITE_OK,
          let statement
    else {
        throw CocoaError(.fileReadCorruptFile)
    }
    defer { sqlite3_finalize(statement) }
    guard sqlite3_bind_int64(statement, 1, accountId) == SQLITE_OK,
          sqlite3_step(statement) == SQLITE_DONE
    else {
        throw CocoaError(.fileWriteUnknown)
    }
}

private func seedDurableNamespaceReadiness(dataRoot: URL, accountId: Int64) throws {
    let layout = try sharedStateLayout(dataRoot: dataRoot.path)
    var database: OpaquePointer?
    guard sqlite3_open_v2(layout.databaseFile, &database, SQLITE_OPEN_READWRITE, nil) == SQLITE_OK,
          let database
    else { throw CocoaError(.fileReadUnknown) }
    defer { sqlite3_close(database) }
    let sql = """
        INSERT INTO namespace_readiness (
            account_id, namespace_version, generation, published_at_ms,
            projection_after_chat_id, convergence_complete, updated_at_ms
        ) VALUES (?, 0, 1, 1000, NULL, 0, 1000)
        """
    var statement: OpaquePointer?
    guard sqlite3_prepare_v2(database, sql, -1, &statement, nil) == SQLITE_OK,
          let statement
    else { throw CocoaError(.fileReadCorruptFile) }
    defer { sqlite3_finalize(statement) }
    guard sqlite3_bind_int64(statement, 1, accountId) == SQLITE_OK,
          sqlite3_step(statement) == SQLITE_DONE
    else { throw CocoaError(.fileWriteUnknown) }
}

@Suite struct AgentLifecycleTests {
    @Test func namespaceProgressSignalsReadinessAndShutdownClosesTheOwner() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root, namespaceBootstrapper: bootstrapper))
            try lifecycle.start()

            lifecycle.startNamespace(accountId: 42)
            #expect(lifecycle.namespaceStatus(accountId: 42) == .preparing)
            bootstrapper.emit(
                .ready(canonicalChatCount: 12, appearanceCount: 19), accountId: 42)
            #expect(
                lifecycle.namespaceStatus(accountId: 42)
                    == .ready(canonicalChatCount: 12, appearanceCount: 19))
            #expect(lifecycle.healthSnapshot().recentEvents.contains("namespace-ready"))

            let session = try #require(bootstrapper.session(accountId: 42))
            await lifecycle.shutdown(reason: .terminate)
            #expect(session.closed)
        }
    }

    @Test func historyPriorityRoutesOnlyThroughTheOwnedNamespace() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root, namespaceBootstrapper: bootstrapper))
            try lifecycle.start()

            #expect(
                try lifecycle.setChatHistoryPriority(
                    accountId: 42, chatId: 900, priority: .visible) == false)
            lifecycle.startNamespace(accountId: 42)
            #expect(
                try lifecycle.setChatHistoryPriority(
                    accountId: 42, chatId: 900, priority: .visible))
            let ipcEvent = try ControlClient.command(
                ControlRequest(
                    operation: .historyPriority,
                    historyPriority: HistoryPriorityRequest(
                        accountId: 42, chatId: 900, priority: .requested)),
                socketURL: lifecycle.runtimeLayout.controlSocket,
                timeout: .seconds(5))
            #expect(ipcEvent == .commandDone)
            #expect(
                try lifecycle.setChatHistoryPriority(
                    accountId: 42, chatId: 900, priority: .background))

            let session = try #require(bootstrapper.session(accountId: 42))
            let priorities = session.prioritySnapshot()
            #expect(priorities.count == 3)
            #expect(priorities[0].0 == 900)
            #expect(priorities[0].1 == .visible)
            #expect(priorities[1].1 == .requested)
            #expect(priorities[2].1 == .background)

            // Health counts what arrived, including the hint that predated the
            // namespace. Without that, "the opened chat did not advance" cannot
            // be attributed to the provider or to the agent on an installed
            // build (BUG-260728-2qfzbd).
            let hints = try #require(lifecycle.healthSnapshot().historyPriorityHints)
            #expect(hints.accepted == 3, "one per hint that reached a live session")
            #expect(hints.visible == 1)
            #expect(hints.requested == 1, "the hint delivered over the socket counts too")
            #expect(hints.background == 1)
            #expect(hints.unroutable == 1, "the hint that predated the namespace")
            #expect(hints.lastAtMs != nil)

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func historyPriorityHintCountsStartEmptyAndCarryNoIdentity() throws {
        try withTemporaryDirectory { root in
            let lifecycle = AgentLifecycle(configuration: AgentConfiguration(dataRoot: root))
            try lifecycle.start()
            let hints = try #require(lifecycle.healthSnapshot().historyPriorityHints)
            #expect(hints == HistoryPriorityHintCounts())

            // The payload is a diagnostic, not a record of what the user opened.
            let encoded = try #require(
                String(data: JSONEncoder().encode(hints), encoding: .utf8))
            #expect(!encoded.contains("chat"))
            #expect(!encoded.contains("account"))
        }
    }

    @Test func namespaceFailureIsActionableAndAnInterruptedOwnerCanRestart() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root, namespaceBootstrapper: bootstrapper))
            try lifecycle.start()

            lifecycle.startNamespace(accountId: 7)
            let first = try #require(bootstrapper.session(accountId: 7))
            bootstrapper.emit(
                .failed(category: "rate-limited", retryable: true), accountId: 7)
            #expect(
                lifecycle.namespaceStatus(accountId: 7)
                    == .failed(category: "rate-limited", retryable: true))
            #expect(lifecycle.healthSnapshot().recentEvents.contains("namespace-failed"))

            lifecycle.stopNamespace(accountId: 7)
            #expect(first.closed)
            #expect(lifecycle.namespaceStatus(accountId: 7) == nil)
            lifecycle.startNamespace(accountId: 7)
            #expect(lifecycle.namespaceStatus(accountId: 7) == .preparing)

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func postReadySourceFailureRecoversWithoutInvalidatingProvenReadiness() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root,
                    namespaceBootstrapper: bootstrapper,
                    namespaceRecoveryDelay: .milliseconds(1)))
            try lifecycle.start()

            lifecycle.startNamespace(accountId: 7)
            let first = try #require(bootstrapper.session(accountId: 7))
            bootstrapper.emit(
                .ready(canonicalChatCount: 2, appearanceCount: 3), accountId: 7)
            bootstrapper.emit(
                .failed(category: "source", retryable: true), accountId: 7)

            for _ in 0..<100 where bootstrapper.startCount(accountId: 7) < 2 {
                try await Task.sleep(for: .milliseconds(5))
            }
            #expect(bootstrapper.startCount(accountId: 7) == 2)
            #expect(first.closed)
            #expect(lifecycle.namespaceStatus(accountId: 7) == .preparing)
            #expect(lifecycle.healthSnapshot().recentEvents.contains("namespace-recovering"))

            bootstrapper.emit(
                .ready(canonicalChatCount: 2, appearanceCount: 3), accountId: 7)
            #expect(
                lifecycle.namespaceStatus(accountId: 7)
                    == .ready(canonicalChatCount: 2, appearanceCount: 3))
            // The core's own retryable flag is the whole contract. An
            // allow-list of categories here silently made every retryable
            // storage, projection and render failure permanent — one
            // transient write failure ended history backfill for the life of
            // the process (BUG-260728-2qfzbd).
            #expect(AgentLifecycle.isRecoverableSourceFailure(category: "source", retryable: true))
            #expect(AgentLifecycle.isRecoverableSourceFailure(category: "storage", retryable: true))
            #expect(
                AgentLifecycle.isRecoverableSourceFailure(
                    category: "projection-node-upsert-storage", retryable: true))
            #expect(
                AgentLifecycle.isRecoverableSourceFailure(category: "render", retryable: true),
                "a category nobody thought to list is still retryable when the core says so")
            #expect(
                !AgentLifecycle.isRecoverableSourceFailure(
                    category: "auth-required", retryable: false),
                "a non-retryable failure would meet the same wall on restart")
            #expect(
                !AgentLifecycle.isRecoverableSourceFailure(
                    category: "source", retryable: false))

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func preservedAuthorizedBootstrapRecoversFromIncompleteSnapshotBeforeFirstReady()
        async throws
    {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root,
                    namespaceBootstrapper: bootstrapper,
                    namespaceRecoveryDelay: .milliseconds(1)))
            try lifecycle.start()
            // Reproduce the material state that matters after an installed
            // migration: authorization is durable before the first namespace
            // owner attempts snapshot bootstrap.
            try seedAuthorizedAccount(dataRoot: root, accountId: 7)
            lifecycle.startNamespace(accountId: 7)

            #expect(bootstrapper.startCount(accountId: 7) == 1)
            let first = try #require(bootstrapper.session(accountId: 7))
            bootstrapper.emit(
                .failed(category: "snapshot-membership-incomplete", retryable: true),
                accountId: 7)

            for _ in 0..<200 where bootstrapper.startCount(accountId: 7) < 2 {
                try await Task.sleep(for: .milliseconds(5))
            }
            #expect(
                bootstrapper.startCount(accountId: 7) == 2,
                "a retryable first-bootstrap failure must resume without relaunch or login")
            #expect(first.closed)
            #expect(lifecycle.namespaceStatus(accountId: 7) == .preparing)
            #expect(lifecycle.healthSnapshot().finderContentState == .preparing)
            #expect(lifecycle.healthSnapshot().finderContentFailure == nil)

            bootstrapper.emit(
                .ready(canonicalChatCount: 2, appearanceCount: 3), accountId: 7)
            #expect(
                lifecycle.namespaceStatus(accountId: 7)
                    == .ready(canonicalChatCount: 2, appearanceCount: 3))
            #expect(lifecycle.observedAuthorizationState(accountId: 7) == .authorized)

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func durableReadinessSurvivesRestartFailureAndAuthorizationPublishesIndependently()
        async throws
    {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root,
                    namespaceBootstrapper: bootstrapper,
                    namespaceRecoveryDelay: .milliseconds(1)))
            try lifecycle.start()
            try seedAuthorizedAccount(dataRoot: root, accountId: 11)
            _ = try lifecycle.store?.ensureRootStructure()
            try seedDurableNamespaceReadiness(dataRoot: root, accountId: 11)

            lifecycle.startNamespace(accountId: 11)
            bootstrapper.emit(.authorized, accountId: 11)
            bootstrapper.emit(.folderCatalog, accountId: 11)
            bootstrapper.emit(.snapshotList, accountId: 11)
            bootstrapper.emit(
                .failed(category: "snapshot-membership-incomplete", retryable: true),
                accountId: 11)

            #expect(lifecycle.observedAuthorizationState(accountId: 11) == .authorized)
            #expect(lifecycle.healthSnapshot().finderContentState == .ready)
            #expect(
                lifecycle.healthSnapshot().finderSourceDegradation?.category
                    == "snapshot-membership-incomplete")
            for _ in 0 ..< 200 where bootstrapper.startCount(accountId: 11) < 2 {
                try await Task.sleep(for: .milliseconds(5))
            }
            #expect(bootstrapper.startCount(accountId: 11) == 2)
            #expect(
                lifecycle.observedAuthorizationState(accountId: 11) == .authorized,
                "replacement startup must not hide the prior definitive live authorization")
            bootstrapper.emit(.projectionSlice(processedChatCount: 16), accountId: 11)
            #expect(lifecycle.healthSnapshot().finderContentState == .ready)
            bootstrapper.emit(
                .failed(category: "auth-required", retryable: false), accountId: 11)
            #expect(
                lifecycle.observedAuthorizationState(accountId: 11)
                    == .authorizationRequired,
                "the replacement owner's definitive auth refusal must supersede authorized")

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func finderReadinessStaysUsableAcrossPostReadyFailureAndRecovery() {
        let sourceFailure = AgentActionableFailure(
            category: "source",
            message: "Telegram metadata is unavailable. Check the connection and retry.",
            retryable: true)

        #expect(
            AgentLifecycle.namespaceReadinessDisposition(
                progress: .ready(canonicalChatCount: 2, appearanceCount: 3),
                hasReachedReady: false)
                == .usable(degradation: nil))
        #expect(
            AgentLifecycle.namespaceReadinessDisposition(
                progress: .failed(category: "source", retryable: true),
                hasReachedReady: true)
                == .usable(degradation: sourceFailure))
        #expect(
            AgentLifecycle.namespaceReadinessDisposition(
                progress: .preparing,
                hasReachedReady: true,
                existingDegradation: sourceFailure)
                == .usable(degradation: sourceFailure))
        #expect(
            AgentLifecycle.namespaceReadinessDisposition(
                progress: .ready(canonicalChatCount: 2, appearanceCount: 3),
                hasReachedReady: true,
                existingDegradation: sourceFailure)
                == .usable(degradation: nil))

        #expect(
            AgentLifecycle.namespaceReadinessDisposition(
                progress: .failed(category: "source", retryable: true),
                hasReachedReady: false)
                == .preparing)
        // Authorization expiry is the core's canonical *non*-retryable
        // failure: restarting the owner would meet the same wall, so proven
        // readiness is genuinely invalidated and the user has to act.
        #expect(
            AgentLifecycle.namespaceReadinessDisposition(
                progress: .failed(category: "auth-required", retryable: false),
                hasReachedReady: true)
                == .failed(
                    AgentActionableFailure(
                        category: "auth-required",
                        message: "Telegram authorization expired. Sign in again to retry.",
                        retryable: false)))
        // A retryable storage failure after readiness is a degradation the
        // agent recovers from on its own, not a dead Finder namespace
        // (BUG-260728-2qfzbd).
        #expect(
            AgentLifecycle.namespaceReadinessDisposition(
                progress: .failed(category: "storage", retryable: true),
                hasReachedReady: true)
                == .usable(
                    degradation: AgentActionableFailure(
                        category: "storage",
                        message: "Finder metadata could not be saved. GramDrive is retrying.",
                        retryable: true)))
    }

    @Test func aRetryableStorageFailureRecreatesTheNamespaceWithoutARelaunch() async throws {
        // The defect this covers was observed live: an agent that had been
        // ready fifteen times reported finderContentState=failed with a
        // retryable storage category and then sat idle for hours, because
        // "storage" was missing from a hardcoded recovery allow-list
        // (BUG-260728-2qfzbd).
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root,
                    namespaceBootstrapper: bootstrapper,
                    namespaceRecoveryDelay: .milliseconds(1)))
            try lifecycle.start()

            lifecycle.startNamespace(accountId: 7)
            let first = try #require(bootstrapper.session(accountId: 7))
            bootstrapper.emit(.ready(canonicalChatCount: 5, appearanceCount: 5), accountId: 7)
            bootstrapper.emit(.failed(category: "storage", retryable: true), accountId: 7)

            for _ in 0..<200 where bootstrapper.startCount(accountId: 7) < 2 {
                try await Task.sleep(for: .milliseconds(5))
            }
            #expect(
                bootstrapper.startCount(accountId: 7) == 2,
                "the owner is recreated with no user action and no relaunch")
            #expect(first.closed)
            #expect(lifecycle.healthSnapshot().recentEvents.contains("namespace-recovering"))

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func repeatedRecoveryBacksOffAndResetsOnceTheNamespaceIsReadyAgain() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root,
                    namespaceBootstrapper: bootstrapper,
                    namespaceRecoveryDelay: .seconds(1)))
            try lifecycle.start()

            // A deterministic failure must not become a restart loop that
            // replays the whole snapshot cycle every second.
            #expect(lifecycle.namespaceRecoveryDelay(attempt: 1) == .seconds(1))
            #expect(lifecycle.namespaceRecoveryDelay(attempt: 2) == .seconds(2))
            #expect(lifecycle.namespaceRecoveryDelay(attempt: 4) == .seconds(8))
            #expect(
                lifecycle.namespaceRecoveryDelay(attempt: 40)
                    == AgentLifecycle.maxNamespaceRecoveryDelay,
                "the backoff is capped, so a permanently failing account stays cheap")

            lifecycle.startNamespace(accountId: 7)
            bootstrapper.emit(.ready(canonicalChatCount: 1, appearanceCount: 1), accountId: 7)
            bootstrapper.emit(.failed(category: "storage", retryable: true), accountId: 7)
            for _ in 0..<200 where bootstrapper.startCount(accountId: 7) < 2 {
                try await Task.sleep(for: .milliseconds(10))
            }
            // Reaching ready is what proves the failure was transient, so the
            // next incident starts over from the configured delay rather than
            // inheriting a long backoff.
            bootstrapper.emit(.ready(canonicalChatCount: 1, appearanceCount: 1), accountId: 7)
            #expect(lifecycle.namespaceRecoveryDelay(attempt: 1) == .seconds(1))

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func itemLocalDegradationKeepsTheNamespaceOwnerAliveUntilReadyReturns() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root, namespaceBootstrapper: bootstrapper))
            try lifecycle.start()

            lifecycle.startNamespace(accountId: 7)
            bootstrapper.emit(
                .ready(canonicalChatCount: 1, appearanceCount: 1), accountId: 7)
            bootstrapper.emit(
                .degraded(category: "chat-metadata", retryable: true), accountId: 7)
            #expect(
                lifecycle.namespaceStatus(accountId: 7)
                    == .degraded(category: "chat-metadata", retryable: true))
            #expect(bootstrapper.startCount(accountId: 7) == 1)
            #expect(lifecycle.healthSnapshot().recentEvents.contains("namespace-degraded"))

            bootstrapper.emit(
                .ready(canonicalChatCount: 2, appearanceCount: 2), accountId: 7)
            #expect(
                lifecycle.namespaceStatus(accountId: 7)
                    == .ready(canonicalChatCount: 2, appearanceCount: 2))
            #expect(bootstrapper.startCount(accountId: 7) == 1)

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func synchronousNamespaceStartFailureDoesNotLeaveConnectingForever() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            bootstrapper.failure = FakeNamespaceError.unavailable
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root, namespaceBootstrapper: bootstrapper))
            try lifecycle.start()

            lifecycle.startNamespace(accountId: 9)
            #expect(
                lifecycle.namespaceStatus(accountId: 9)
                    == .failed(category: "source", retryable: true))
            #expect(lifecycle.healthSnapshot().recentEvents.contains("namespace-start-failed"))

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func synchronousNamespaceStartFailurePreservesSafeDriveErrorCategory() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            bootstrapper.failure = DriveError.AuthRequired(detail: "private diagnostic")
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root, namespaceBootstrapper: bootstrapper))
            try lifecycle.start()

            lifecycle.startNamespace(accountId: 9)

            #expect(
                lifecycle.namespaceStatus(accountId: 9)
                    == .failed(category: "auth-required", retryable: false))
            #expect(
                lifecycle.observedAuthorizationState(accountId: 9)
                    == .authorizationRequired)
            #expect(lifecycle.healthSnapshot().recentEvents.contains("namespace-start-failed"))

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func repairAuthRequiredObservationSurvivesRestartAndPreservesDurableHealthRow() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = AgentLifecycle(
                configuration: AgentConfiguration(
                    dataRoot: root, namespaceBootstrapper: bootstrapper))
            try lifecycle.start()
            try seedAuthorizedAccount(dataRoot: root, accountId: 9)

            let repairer = CoreRepairRunner(
                configuration: CoreAuthConfiguration(dataRoot: root),
                vault: KeychainSecretVault(),
                accounts: { lifecycle.healthSnapshot().accounts ?? [] },
                beforeRepair: { lifecycle.stopAllNamespaces() },
                afterRepair: { lifecycle.restartNamespaces() },
                onAuthorizationObserved: { accountId, state in
                    lifecycle.recordObservedAuthorization(state, accountId: accountId)
                },
                authorizationProbe: { _, accountId, _ in
                    #expect(accountId == 9)
                    return .signedOut(kind: "waitPhoneNumber")
                })

            let outcome = await repairer.repair()
            #expect(
                outcome
                    == .failed(
                        ControlCommandFailure(
                            category: .authRequired,
                            detail: "account 9 needs a fresh sign-in")))
            #expect(bootstrapper.startCount(accountId: 9) == 1)
            #expect(lifecycle.namespaceStatus(accountId: 9) == .preparing)

            // The replacement owner starts in a transitional state. That must
            // not erase the repair result before a companion health read.
            bootstrapper.emit(.preparing, accountId: 9)
            let afterRestart = try AgentHealthClient.fetch(
                socketURL: lifecycle.runtimeLayout.healthSocket)
            let durableButSignedOut = try #require(afterRestart.accounts?.first)
            #expect(durableButSignedOut.authState == "authorized")
            #expect(durableButSignedOut.observedAuthorization == .authorizationRequired)

            // A definitive replacement result supersedes the held probe
            // result, while the durable row stays untouched throughout.
            bootstrapper.emit(
                .ready(canonicalChatCount: 1, appearanceCount: 1), accountId: 9)
            #expect(lifecycle.observedAuthorizationState(accountId: 9) == .authorized)
            #expect(lifecycle.healthSnapshot().accounts?.first?.authState == "authorized")

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func startReachesRunningWithStateOpenAndHealthServing() async throws {
        try await withTemporaryDirectoryAsync { root in
            let lifecycle = try startedLifecycle(dataRoot: root)
            #expect(lifecycle.currentState == .running)

            // Health through the real bounded IPC channel, as the app
            // would read it.
            let health = try AgentHealthClient.fetch(
                socketURL: lifecycle.runtimeLayout.healthSocket)
            #expect(health.state == .running)
            #expect(health.pid == ProcessInfo.processInfo.processIdentifier)
            #expect(health.pendingTransferCount == 0)
            #expect(health.recentEvents.contains("started"))
            #expect(health.recentEvents.contains("root-structure-ready"))
            #expect(health.finderContentState == .waitingForAuthorization)
            #expect(health.finderFirstPageItemCount == 0)
            let schemaVersion = try #require(health.stateSchemaVersion)
            #expect(schemaVersion > 0)

            // The reported contract version is the linked core's.
            let contract = contractVersion()
            #expect(
                health.contractVersion
                    == "\(contract.major).\(contract.minor).\(contract.patch)")

            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func authDiagnosticsPersistAcrossRelaunchAndRedactAuthPayloads() async throws {
        try await withTemporaryDirectoryAsync { root in
            let first = try startedLifecycle(dataRoot: root)
            let sensitiveValues = [
                "+15551234567", "843921", "correct-horse-battery-staple",
                "tg://login?token=private", "987654321", "Ada Lovelace",
            ]
            let rejection = ControlAuthRejection(
                kind: "other",
                code: 987_654_321,
                detail: sensitiveValues.joined(separator: " "))
            let refusal = AuthDiagnosticCode.refusal(for: rejection)
            let expected: [AuthDiagnosticCode] = [
                .sessionStarted,
                refusal,
                .finalizeSucceeded,
                .finalizeFailed,
                .probeSignedOut,
            ]
            for code in expected {
                first.recordAuthDiagnostic(code)
            }

            let firstPayload = try #require(
                String(data: JSONEncoder().encode(first.healthSnapshot().recentEvents), encoding: .utf8))
            let persistedPayload = try #require(
                String(data: Data(contentsOf: first.runtimeLayout.authDiagnosticsFile), encoding: .utf8))
            for value in sensitiveValues {
                #expect(!firstPayload.localizedCaseInsensitiveContains(value))
                #expect(!persistedPayload.localizedCaseInsensitiveContains(value))
            }
            #expect(
                AuthDiagnosticTrail.logMessage(for: refusal)
                    == "event=auth-refused-other")

            await first.shutdown(reason: .terminate)
            let second = try startedLifecycle(dataRoot: root)
            let restored = second.healthSnapshot().recentEvents
            for code in expected {
                #expect(restored.contains(code.rawValue))
            }
            await second.shutdown(reason: .terminate)
        }
    }

    @Test func providerFetchHealthIsDurableAndExposesOnlyAggregateCounts() async throws {
        try await withTemporaryDirectoryAsync { root in
            let lifecycle = try startedLifecycle(dataRoot: root)
            let client = AgentProviderFetchHealthClient(
                socketURL: { lifecycle.runtimeLayout.controlSocket })
            client.signal(
                ProviderFetchHealthReport(
                    succeeded: false,
                    engineFailure: true,
                    providerMapping: true,
                    noSuchItem: true,
                    retryable: false,
                    observedAtMs: 1_000))
            client.signal(
                ProviderFetchHealthReport(
                    succeeded: true,
                    engineFailure: false,
                    providerMapping: false,
                    noSuchItem: false,
                    retryable: false,
                    observedAtMs: 2_000))
            client.signal(
                ProviderFetchHealthReport(
                    succeeded: false,
                    engineFailure: true,
                    providerMapping: true,
                    noSuchItem: false,
                    retryable: true,
                    observedAtMs: 3_000))

            let deadline = ContinuousClock.now + .seconds(5)
            var health = try AgentHealthClient.fetch(
                socketURL: lifecycle.runtimeLayout.healthSocket)
            while health.providerFetchHealth?.callbacks != 3,
                  ContinuousClock.now < deadline
            {
                try await Task.sleep(for: .milliseconds(10))
                health = try AgentHealthClient.fetch(
                    socketURL: lifecycle.runtimeLayout.healthSocket)
            }
            #expect(
                health.providerFetchHealth
                    == ProviderFetchHealthCounts(
                        callbacks: 3,
                        succeeded: 1,
                        engineFailures: 2,
                        providerMappings: 2,
                        noSuchItem: 1,
                        retryable: 1))

            let encoded = try #require(
                String(data: JSONEncoder().encode(health.providerFetchHealth), encoding: .utf8))
            for forbidden in ["fp-", "Alice", "123456789", "telegram", "account"] {
                #expect(!encoded.localizedCaseInsensitiveContains(forbidden))
            }
            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func aSecondAgentOverTheSameContainerIsRefused() async throws {
        try await withTemporaryDirectoryAsync { root in
            let first = try startedLifecycle(dataRoot: root)
            let second = AgentLifecycle(
                configuration: AgentConfiguration(dataRoot: root))
            #expect(throws: AgentStartError.self) {
                try second.start()
            }
            // The refusal touched nothing: the first agent still serves.
            #expect(first.currentState == .running)
            let health = try AgentHealthClient.fetch(
                socketURL: first.runtimeLayout.healthSocket)
            #expect(health.state == .running)
            await first.shutdown(reason: .terminate)
        }
    }

    @Test func startupQuarantinesACorruptDatabaseAndRecovers() async throws {
        try await withTemporaryDirectoryAsync { root in
            // A crashed writer left a corrupt database behind.
            let layout = try SharedState.layout(dataRoot: root)
            try FileManager.default.createDirectory(
                atPath: layout.stateDir, withIntermediateDirectories: true)
            try Data("garbage, not a database".utf8)
                .write(to: URL(fileURLWithPath: layout.databaseFile))

            let lifecycle = try startedLifecycle(dataRoot: root)
            #expect(lifecycle.currentState == .running)
            let health = lifecycle.healthSnapshot()
            #expect(health.recentEvents.contains("state-quarantined"))
            // The damaged file was preserved, not destroyed.
            let quarantined = try FileManager.default.contentsOfDirectory(
                atPath: layout.quarantineDir)
            #expect(!quarantined.isEmpty)
            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func shutdownDrainsAHostedTransferThroughItsToken() async throws {
        try await withTemporaryDirectoryAsync { root in
            let lifecycle = try startedLifecycle(
                dataRoot: root, grace: .milliseconds(50))
            let core = try #require(lifecycle.core)

            // A real in-flight operation through the FFI contract: the
            // boundary probe, registered the way the agent hosts work.
            let token = CancellationToken()
            let ticket = try lifecycle.transfers.begin(token: token)
            let probe = Task {
                defer { lifecycle.transfers.end(ticket) }
                // ~100 s if never cancelled; the drain must cut it short.
                return try await core.probeTransfer(
                    totalBytes: 1_000,
                    chunkBytes: 1,
                    chunkDelayMs: 100,
                    listener: NoopProgressListener(),
                    token: token)
            }
            #expect(lifecycle.transfers.pendingCount == 1)

            let outcome = await lifecycle.shutdown(reason: .terminate)
            #expect(outcome == DrainOutcome(completed: 0, cancelled: 1, abandoned: 0))
            await #expect(throws: DriveError.self) {
                _ = try await probe.value
            }
            #expect(lifecycle.currentState == .stopped)
        }
    }

    @Test func shutdownTearsDownEndpointAndLock() async throws {
        try await withTemporaryDirectoryAsync { root in
            let lifecycle = try startedLifecycle(dataRoot: root)
            let layout = lifecycle.runtimeLayout
            await lifecycle.shutdown(reason: .logout)
            #expect(lifecycle.currentState == .stopped)

            // Endpoint gone...
            #expect(throws: AgentHealthClientError.self) {
                _ = try AgentHealthClient.fetch(socketURL: layout.healthSocket)
            }
            #expect(!FileManager.default.fileExists(atPath: layout.healthSocket.path))
            // ...and the container is free for a successor.
            let successor = try SingleInstanceLock.acquire(at: layout.lockFile)
            successor.release()
        }
    }

    @Test func abandonedDrainKeepsHealthEndpointAliveAndReportsCancellation() async throws {
        try await withTemporaryDirectoryAsync { root in
            let lifecycle = try startedLifecycle(
                dataRoot: root,
                grace: .milliseconds(1),
                cancelWait: .milliseconds(1))
            let layout = lifecycle.runtimeLayout
            _ = try lifecycle.transfers.begin(token: nil)  // deliberately never ends

            let outcome = await lifecycle.shutdown(reason: .update)

            #expect(outcome.abandoned == 1)
            #expect(lifecycle.currentState == .terminationCancelled)
            let health = try AgentHealthClient.fetch(socketURL: layout.healthSocket)
            #expect(health.state == .terminationCancelled)
            #expect(FileManager.default.fileExists(atPath: layout.controlSocket.path))
            let recoveredTicket = try lifecycle.transfers.begin(token: nil)
            lifecycle.transfers.end(recoveredTicket)
        }
    }

    @Test func explicitCancellationRestoresTransferAdmissionAfterTheBoundedDrain() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = try startedLifecycle(
                dataRoot: root,
                grace: .milliseconds(100),
                cancelWait: .milliseconds(100),
                namespaceBootstrapper: bootstrapper)
            lifecycle.startNamespace(accountId: 42)
            let originalSession = try #require(bootstrapper.session(accountId: 42))
            let request = ControlTerminationRequest(
                expectedAgentInstanceID: try #require(lifecycle.healthSnapshot().processIdentity?.instanceID),
                reason: .update, targetBuild: "137")
            let ticket = try lifecycle.transfers.begin(token: nil)
            let worker = Task {
                try? await Task.sleep(for: .milliseconds(5))
                lifecycle.transfers.end(ticket)
            }

            lifecycle.beginTermination(request)
            var cancellation = request
            cancellation.action = .cancel
            lifecycle.cancelTermination(cancellation)
            let outcome = await lifecycle.shutdown(reason: .update)
            await worker.value

            #expect(outcome == DrainOutcome(completed: 1, cancelled: 0, abandoned: 0))
            #expect(lifecycle.currentState == .terminationCancelled)
            let resumedTicket = try lifecycle.transfers.begin(token: nil)
            lifecycle.transfers.end(resumedTicket)
            #expect(originalSession.closed)
            #expect(bootstrapper.startCount(accountId: 42) == 2)
            let recoveredSession = try #require(bootstrapper.session(accountId: 42))
            #expect(recoveredSession !== originalSession)
            #expect(
                try lifecycle.setChatHistoryPriority(
                    accountId: 42, chatId: 900, priority: .visible))
        }
    }

    @Test func uncommittedPreparedDrainRestoresTheSameNamespaceOwnersAtLeaseExpiry() async throws {
        try await withTemporaryDirectoryAsync { root in
            let bootstrapper = FakeNamespaceBootstrapper()
            let lifecycle = try startedLifecycle(
                dataRoot: root,
                namespaceBootstrapper: bootstrapper,
                terminationCommitLease: .milliseconds(5))
            lifecycle.startNamespace(accountId: 42)
            let originalSession = try #require(bootstrapper.session(accountId: 42))
            let request = ControlTerminationRequest(
                expectedAgentInstanceID: try #require(lifecycle.healthSnapshot().processIdentity?.instanceID),
                reason: .update, targetBuild: "137")

            lifecycle.beginTermination(request)
            let outcome = await lifecycle.shutdown(reason: .update)

            #expect(outcome.abandoned == 0)
            #expect(lifecycle.currentState == .terminationReady)
            #expect(originalSession.closed)
            for _ in 0 ..< 100 where lifecycle.currentState != .terminationCancelled {
                try await Task.sleep(for: .milliseconds(2))
            }
            #expect(lifecycle.currentState == .terminationCancelled)
            #expect(bootstrapper.startCount(accountId: 42) == 2)
            let resumedTicket = try lifecycle.transfers.begin(token: nil)
            lifecycle.transfers.end(resumedTicket)
        }
    }

    @Test func committedPreparedDrainStopsOnlyAfterTheMatchingCommit() async throws {
        try await withTemporaryDirectoryAsync { root in
            let lifecycle = try startedLifecycle(dataRoot: root)
            let request = ControlTerminationRequest(
                expectedAgentInstanceID: try #require(lifecycle.healthSnapshot().processIdentity?.instanceID),
                reason: .update, targetBuild: "137")

            lifecycle.beginTermination(request)
            _ = await lifecycle.shutdown(reason: .update)
            #expect(lifecycle.currentState == .terminationReady)

            var wrong = request
            wrong.requestID = UUID()
            wrong.action = .commit
            #expect(!lifecycle.acceptTerminationCommit(wrong, armWatchdog: TestCommitWatchdog.armed))
            #expect(lifecycle.currentState == .terminationReady)

            var commit = request
            commit.action = .commit
            #expect(lifecycle.acceptTerminationCommit(commit, armWatchdog: TestCommitWatchdog.armed))
            // A claimed commit keeps the health endpoint's last live state
            // reversible-looking rather than advertising `.stopped` before
            // process death. The companion must wait for socket/process
            // disappearance, never use this payload as a terminal witness.
            #expect(lifecycle.currentState == .terminationReady)
            #expect(FileManager.default.fileExists(atPath: lifecycle.runtimeLayout.healthSocket.path))
            #expect(lifecycle.finishAcceptedTerminationCommit(commit))
            #expect(!FileManager.default.fileExists(atPath: lifecycle.runtimeLayout.healthSocket.path))

            // Commit must not explicitly release the flock or durable owners:
            // only process death may make the next agent eligible to acquire
            // this data root.
            let contender = AgentLifecycle(configuration: AgentConfiguration(dataRoot: root))
            #expect(throws: AgentStartError.self) {
                try contender.start()
            }
        }
    }

    @Test func watchdogArmFailureLeavesThePreparedDrainRollbackSafe() async throws {
        try await withTemporaryDirectoryAsync { root in
            let lifecycle = try startedLifecycle(dataRoot: root)
            let request = ControlTerminationRequest(
                expectedAgentInstanceID: try #require(lifecycle.healthSnapshot().processIdentity?.instanceID),
                reason: .update, targetBuild: "137")
            lifecycle.beginTermination(request)
            _ = await lifecycle.shutdown(reason: .update)
            var commit = request
            commit.action = .commit

            #expect(
                !lifecycle.acceptTerminationCommit(
                    commit, armWatchdog: TestCommitWatchdog.failedToArm))
            #expect(lifecycle.currentState == .terminationReady)

            var cancel = request
            cancel.action = .cancel
            lifecycle.cancelTermination(cancel)
            #expect(lifecycle.currentState == .terminationCancelled)
            let resumedTicket = try lifecycle.transfers.begin(token: nil)
            lifecycle.transfers.end(resumedTicket)
        }
    }

    @Test func terminationRejectsDelayedCommandsForAnotherProcessInstance() async throws {
        try await withTemporaryDirectoryAsync { root in
            let lifecycle = try startedLifecycle(dataRoot: root)
            let identity = lifecycle.healthSnapshot().processIdentity
            #expect(identity != nil)

            let stale = ControlTerminationRequest(
                expectedAgentInstanceID: UUID(), reason: .update, targetBuild: "137")
            lifecycle.beginTermination(stale)
            #expect(lifecycle.currentState == .running)

            let request = ControlTerminationRequest(
                expectedAgentInstanceID: try #require(identity?.instanceID),
                reason: .update,
                targetBuild: "137")
            lifecycle.beginTermination(request)
            _ = await lifecycle.shutdown(reason: .update)
            #expect(lifecycle.currentState == .terminationReady)

            var staleCommit = request
            staleCommit.action = .commit
            staleCommit.expectedAgentInstanceID = UUID()
            #expect(
                !lifecycle.acceptTerminationCommit(
                    staleCommit, armWatchdog: TestCommitWatchdog.armed))
            #expect(lifecycle.currentState == .terminationReady)
        }
    }

    @Test func newWorkIsRefusedWhileDraining() async throws {
        try await withTemporaryDirectoryAsync { root in
            let lifecycle = try startedLifecycle(dataRoot: root)
            await lifecycle.shutdown(reason: .terminate)
            #expect(throws: TransferRegistryError.draining) {
                _ = try lifecycle.transfers.begin(token: nil)
            }
        }
    }

    @Test func wakeIsRecordedAndReprobesSharedState() async throws {
        try await withTemporaryDirectoryAsync { root in
            let power = FakePowerEventSource()
            let lifecycle = try startedLifecycle(dataRoot: root, power: power)
            power.emit(.willSleep)
            power.emit(.didWake)
            let health = lifecycle.healthSnapshot()
            #expect(health.lastSleepMs != nil)
            #expect(health.lastWakeMs != nil)
            #expect(health.recentEvents.contains("sleep"))
            #expect(health.recentEvents.contains("wake"))
            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func unreadableSettingsAreReportedNotFatal() async throws {
        try await withTemporaryDirectoryAsync { root in
            let layout = AgentRuntimeLayout(dataRoot: root)
            try layout.ensureDirectories()
            try Data("not json".utf8).write(to: layout.settingsFile)

            let lifecycle = try startedLifecycle(dataRoot: root)
            #expect(lifecycle.currentState == .running)
            let health = lifecycle.healthSnapshot()
            #expect(health.launchAtLogin == nil)
            #expect(health.recentEvents.contains("settings-unreadable"))
            await lifecycle.shutdown(reason: .terminate)
        }
    }

    @Test func theLaunchPreferenceSurfacesInHealth() async throws {
        try await withTemporaryDirectoryAsync { root in
            let layout = AgentRuntimeLayout(dataRoot: root)
            try layout.ensureDirectories()
            try AgentSettingsStore(fileURL: layout.settingsFile)
                .save(AgentSettings(launchAtLogin: true))

            let lifecycle = try startedLifecycle(dataRoot: root)
            #expect(lifecycle.healthSnapshot().launchAtLogin == true)
            await lifecycle.shutdown(reason: .terminate)
        }
    }
}
