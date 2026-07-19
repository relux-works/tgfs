import Foundation
import Testing

@testable import GramDriveAgentCore

/// A unique scratch directory, removed after the test.
func withTemporaryDirectory<T>(_ body: (URL) throws -> T) throws -> T {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent(
            "gramdrive-agent-tests-\(ProcessInfo.processInfo.processIdentifier)-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: url) }
    return try body(url)
}

/// Async variant of ``withTemporaryDirectory(_:)``.
func withTemporaryDirectoryAsync<T>(_ body: (URL) async throws -> T) async throws -> T {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent(
            "gramdrive-agent-tests-\(ProcessInfo.processInfo.processIdentifier)-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: url) }
    return try await body(url)
}

@Suite struct AgentRuntimeLayoutTests {
    @Test func runtimeFilesAreFixedBelowTheDataRoot() {
        let layout = AgentRuntimeLayout(dataRoot: URL(fileURLWithPath: "/container/data"))
        #expect(layout.agentDirectory.path == "/container/data/agent")
        #expect(layout.lockFile.path == "/container/data/agent/agent.lock")
        #expect(layout.healthSocket.path == "/container/data/agent/health.sock")
        #expect(layout.settingsFile.path == "/container/data/agent/settings.json")
    }

    @Test func ensureDirectoriesCreatesTheAgentDirectory() throws {
        try withTemporaryDirectory { root in
            let layout = AgentRuntimeLayout(dataRoot: root.appendingPathComponent("data"))
            try layout.ensureDirectories()
            var isDirectory: ObjCBool = false
            #expect(
                FileManager.default.fileExists(
                    atPath: layout.agentDirectory.path, isDirectory: &isDirectory))
            #expect(isDirectory.boolValue)
        }
    }
}

@Suite struct SingleInstanceLockTests {
    @Test func aHeldLockRefusesASecondAcquisition() throws {
        try withTemporaryDirectory { root in
            let url = root.appendingPathComponent("agent.lock")
            let first = try SingleInstanceLock.acquire(at: url)
            defer { first.release() }
            // flock ownership follows the open file description, so a
            // second open in the same process conflicts exactly like a
            // second process would.
            #expect(throws: SingleInstanceLockError.alreadyHeld(path: url.path)) {
                _ = try SingleInstanceLock.acquire(at: url)
            }
        }
    }

    @Test func releaseMakesTheLockAcquirableAgain() throws {
        try withTemporaryDirectory { root in
            let url = root.appendingPathComponent("agent.lock")
            let first = try SingleInstanceLock.acquire(at: url)
            first.release()
            let second = try SingleInstanceLock.acquire(at: url)
            second.release()
        }
    }

    @Test func releaseIsIdempotent() throws {
        try withTemporaryDirectory { root in
            let url = root.appendingPathComponent("agent.lock")
            let lock = try SingleInstanceLock.acquire(at: url)
            lock.release()
            lock.release()
        }
    }

    @Test func theLockFileCarriesDiagnostics() throws {
        try withTemporaryDirectory { root in
            let url = root.appendingPathComponent("agent.lock")
            let lock = try SingleInstanceLock.acquire(at: url)
            defer { lock.release() }
            let content = try String(contentsOf: url, encoding: .utf8)
            #expect(content.contains("pid=\(ProcessInfo.processInfo.processIdentifier)"))
            #expect(content.contains("acquired_at_ms="))
        }
    }
}

@Suite struct AgentSettingsStoreTests {
    @Test func aMissingFileIsTheDefaults() throws {
        try withTemporaryDirectory { root in
            let store = AgentSettingsStore(fileURL: root.appendingPathComponent("settings.json"))
            let loaded = try store.load()
            #expect(loaded == AgentSettings(launchAtLogin: false))
        }
    }

    @Test func settingsRoundTrip() throws {
        try withTemporaryDirectory { root in
            let store = AgentSettingsStore(fileURL: root.appendingPathComponent("settings.json"))
            try store.save(AgentSettings(launchAtLogin: true))
            let enabled = try store.load()
            #expect(enabled == AgentSettings(launchAtLogin: true))
            try store.save(AgentSettings(launchAtLogin: false))
            let disabled = try store.load()
            #expect(disabled == AgentSettings(launchAtLogin: false))
        }
    }

    @Test func aCorruptFileThrowsInsteadOfGuessing() throws {
        try withTemporaryDirectory { root in
            let url = root.appendingPathComponent("settings.json")
            try Data("not json".utf8).write(to: url)
            let store = AgentSettingsStore(fileURL: url)
            #expect(throws: (any Error).self) {
                _ = try store.load()
            }
        }
    }
}

/// Hand-driven ``LoginItemService`` for the policy matrix.
private final class FakeLoginItemService: LoginItemService {
    var status: LoginItemStatus
    var statusAfterRegister: LoginItemStatus
    var registerError: Error?
    var unregisterError: Error?
    private(set) var registerCalls = 0
    private(set) var unregisterCalls = 0

    init(status: LoginItemStatus, statusAfterRegister: LoginItemStatus = .enabled) {
        self.status = status
        self.statusAfterRegister = statusAfterRegister
    }

    func register() throws {
        registerCalls += 1
        if let registerError { throw registerError }
        status = statusAfterRegister
    }

    func unregister() throws {
        unregisterCalls += 1
        if let unregisterError { throw unregisterError }
        status = .notRegistered
    }
}

private struct FakeServiceError: Error {}

@Suite struct LaunchAtLoginPolicyTests {
    @Test func enablingRegistersWhenNotRegistered() throws {
        let service = FakeLoginItemService(status: .notRegistered)
        let action = try LaunchAtLoginPolicy.reconcile(preference: true, service: service)
        #expect(action == .registered)
        #expect(service.registerCalls == 1)
        #expect(service.status == .enabled)
    }

    @Test func enablingAnAlreadyEnabledItemChangesNothing() throws {
        let service = FakeLoginItemService(status: .enabled)
        #expect(try LaunchAtLoginPolicy.reconcile(preference: true, service: service) == .noChange)
        #expect(service.registerCalls == 0)
    }

    @Test func pendingApprovalIsSurfacedNotRetried() throws {
        let service = FakeLoginItemService(status: .requiresApproval)
        let action = try LaunchAtLoginPolicy.reconcile(preference: true, service: service)
        #expect(action == .awaitingApproval)
        #expect(service.registerCalls == 0)
    }

    @Test func registrationLandingInApprovalIsReportedAsAwaiting() throws {
        let service = FakeLoginItemService(
            status: .notRegistered, statusAfterRegister: .requiresApproval)
        let action = try LaunchAtLoginPolicy.reconcile(preference: true, service: service)
        #expect(action == .awaitingApproval)
        #expect(service.registerCalls == 1)
    }

    @Test func disablingUnregistersAnEnabledItem() throws {
        let service = FakeLoginItemService(status: .enabled)
        let action = try LaunchAtLoginPolicy.reconcile(preference: false, service: service)
        #expect(action == .unregistered)
        #expect(service.unregisterCalls == 1)
        #expect(service.status == .notRegistered)
    }

    @Test func disablingAPendingApprovalUnregistersIt() throws {
        let service = FakeLoginItemService(status: .requiresApproval)
        #expect(
            try LaunchAtLoginPolicy.reconcile(preference: false, service: service)
                == .unregistered)
    }

    @Test func disablingAnUnregisteredItemChangesNothing() throws {
        let service = FakeLoginItemService(status: .notRegistered)
        #expect(try LaunchAtLoginPolicy.reconcile(preference: false, service: service) == .noChange)
        #expect(service.unregisterCalls == 0)
    }

    @Test func serviceErrorsPropagate() {
        let service = FakeLoginItemService(status: .notRegistered)
        service.registerError = FakeServiceError()
        #expect(throws: FakeServiceError.self) {
            _ = try LaunchAtLoginPolicy.reconcile(preference: true, service: service)
        }
    }
}
