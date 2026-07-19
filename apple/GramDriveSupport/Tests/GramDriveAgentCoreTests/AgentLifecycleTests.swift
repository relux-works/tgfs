import Foundation
import GramDriveCore
import GramDriveSupport
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

private func startedLifecycle(
    dataRoot: URL,
    grace: Duration = .seconds(5),
    cancelWait: Duration = .seconds(5),
    power: (any PowerEventSource)? = nil
) throws -> AgentLifecycle {
    let lifecycle = AgentLifecycle(
        configuration: AgentConfiguration(
            dataRoot: dataRoot,
            drainGracePeriod: grace,
            drainCancelWait: cancelWait,
            powerEvents: power))
    try lifecycle.start()
    return lifecycle
}

@Suite struct AgentLifecycleTests {
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
