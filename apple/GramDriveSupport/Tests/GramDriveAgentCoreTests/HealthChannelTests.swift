import Foundation
import GramDriveSupport
import Testing

@testable import GramDriveAgentCore

private func sampleSnapshot(state: AgentRunState = .running) -> AgentHealthSnapshot {
    AgentHealthSnapshot(
        payloadVersion: 1,
        agentVersion: AgentVersion.current,
        contractVersion: "0.2.0",
        pid: ProcessInfo.processInfo.processIdentifier,
        state: state,
        startedAtMs: 1_000,
        launchAtLogin: false,
        stateSchemaVersion: 1,
        dataVersion: 42,
        pendingTransferCount: 0,
        lastSourceUpdateMs: nil,
        changeCursor: nil,
        cachePressure: nil,
        providerRegistrationState: nil,
        lastSleepMs: nil,
        lastWakeMs: nil,
        recentEvents: ["started"])
}

@Suite struct HealthChannelTests {
    @Test func aClientReadsTheServedSnapshot() throws {
        try withTemporaryDirectory { root in
            let socket = root.appendingPathComponent("health.sock")
            let server = try AgentHealthServer.start(socketURL: socket) { sampleSnapshot() }
            defer { server.stop() }
            let fetched = try AgentHealthClient.fetch(socketURL: socket)
            #expect(fetched == sampleSnapshot())
        }
    }

    @Test func aSocketPathBeyondSunPathStillWorks() throws {
        try withTemporaryDirectory { root in
            // Group-container data roots routinely exceed sockaddr_un's
            // 103-byte path budget; the channel must not care.
            var deep = root
            for index in 0..<6 {
                deep = deep.appendingPathComponent(
                    "very-long-directory-component-\(index)-abcdefghijklmnopqrstuvwxyz")
            }
            try FileManager.default.createDirectory(at: deep, withIntermediateDirectories: true)
            let socket = deep.appendingPathComponent("health.sock")
            #expect(socket.path.utf8.count > UnixSocketAddress.maxDirectPathLength)

            let server = try AgentHealthServer.start(socketURL: socket) { sampleSnapshot() }
            defer { server.stop() }
            let fetched = try AgentHealthClient.fetch(socketURL: socket)
            #expect(fetched.pid == ProcessInfo.processInfo.processIdentifier)
        }
    }

    @Test func aStaleSocketFileIsReplacedOnStart() throws {
        try withTemporaryDirectory { root in
            let socket = root.appendingPathComponent("health.sock")
            // A SIGKILLed predecessor leaves its socket file behind; the
            // next start (single-instance lock already held) must reclaim
            // the path.
            try Data().write(to: socket)
            let server = try AgentHealthServer.start(socketURL: socket) { sampleSnapshot() }
            defer { server.stop() }
            let fetched = try AgentHealthClient.fetch(socketURL: socket)
            #expect(fetched.state == .running)
        }
    }

    @Test func sequentialFetchesSeeFreshSnapshots() throws {
        try withTemporaryDirectory { root in
            let socket = root.appendingPathComponent("health.sock")
            let counter = Counter()
            let server = try AgentHealthServer.start(socketURL: socket) {
                var snapshot = sampleSnapshot()
                snapshot.pendingTransferCount = counter.next()
                return snapshot
            }
            defer { server.stop() }
            let first = try AgentHealthClient.fetch(socketURL: socket)
            #expect(first.pendingTransferCount == 1)
            let second = try AgentHealthClient.fetch(socketURL: socket)
            #expect(second.pendingTransferCount == 2)
        }
    }

    @Test func concurrentFetchesBothDecode() async throws {
        try await withTemporaryDirectoryAsync { root in
            let socket = root.appendingPathComponent("health.sock")
            let server = try AgentHealthServer.start(socketURL: socket) { sampleSnapshot() }
            defer { server.stop() }
            async let first = Task.detached {
                try AgentHealthClient.fetch(socketURL: socket)
            }.value
            async let second = Task.detached {
                try AgentHealthClient.fetch(socketURL: socket)
            }.value
            let (one, two) = try await (first, second)
            #expect(one == two)
        }
    }

    @Test func aStoppedServerIsUnavailable() throws {
        try withTemporaryDirectory { root in
            let socket = root.appendingPathComponent("health.sock")
            let server = try AgentHealthServer.start(socketURL: socket) { sampleSnapshot() }
            server.stop()
            // stop() removes the socket file, so the failure is the
            // "agent not running" category, not a raw socket error.
            #expect(!FileManager.default.fileExists(atPath: socket.path))
            #expect(throws: AgentHealthClientError.agentUnavailable(path: socket.path)) {
                _ = try AgentHealthClient.fetch(socketURL: socket)
            }
        }
    }

    @Test func fetchingWithNoServerIsUnavailable() throws {
        try withTemporaryDirectory { root in
            let socket = root.appendingPathComponent("health.sock")
            #expect(throws: AgentHealthClientError.agentUnavailable(path: socket.path)) {
                _ = try AgentHealthClient.fetch(socketURL: socket)
            }
        }
    }
}

private final class Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var value = 0

    func next() -> Int {
        lock.lock()
        defer { lock.unlock() }
        value += 1
        return value
    }
}
