import Darwin
import Dispatch
import Foundation
@testable import GramDriveAgentCore
import GramDriveSupport
import SQLite3
import Testing

private final class ProcessExitFlag: @unchecked Sendable {
    private let lock = NSLock()
    private var value = false

    func mark() {
        lock.lock()
        value = true
        lock.unlock()
    }

    var isMarked: Bool {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

/// Exercises the shipped agent executable over its real UNIX sockets. The
/// lifecycle unit tests cover the exhaustive reducer schedules; this test
/// pins the production executable boundary that AppKit observes.
@Suite(.serialized)
struct AgentProcessTerminationTests {
    @Test func directSignalsExitPastBlockedNamespaceCloseAndReleaseProcessOwnership() throws {
        let root = try temporaryRoot()
        defer { try? FileManager.default.removeItem(at: root) }
        try seedAuthorizedAccount(dataRoot: root, accountId: 42)

        let agent = try startAgent(
            root: root,
            extraArguments: [
                "--test-observed-authorization", "authorized",
                "--test-namespace-close-delay-ms", "10000",
            ]
        )
        defer { stopIfNeeded(agent) }
        let layout = AgentRuntimeLayout(dataRoot: root)
        let running = try waitForHealth(socketURL: layout.healthSocket) { snapshot in
            snapshot.accounts?.count == 1
                && snapshot.accounts?.first?.observedAuthorization == .authorized
                && snapshot.namespaceOwnersRestored == true
        }
        let identity = try #require(running.processIdentity)
        let exited = ProcessExitFlag()
        let observer = DispatchSource.makeProcessSource(
            identifier: pid_t(identity.pid), eventMask: .exit, queue: .global(qos: .userInitiated)
        )
        observer.setEventHandler { exited.mark() }
        observer.resume()
        defer { observer.cancel() }

        let signalledAt = ContinuousClock.now
        #expect(Darwin.kill(identity.pid, SIGTERM) == 0)
        let closeEnteredMarker = layout.agentDirectory.appendingPathComponent(
            "test-namespace-close-entered"
        )
        try waitForFile(closeEnteredMarker)
        #expect(Darwin.kill(identity.pid, SIGINT) == 0)
        try waitForExit(identity: identity, observer: exited)

        #expect(signalledAt.duration(to: ContinuousClock.now) < .seconds(3))
        #expect(!processStillMatches(identity))
        #expect(exited.isMarked)

        let replacement = try startAgent(
            root: root,
            extraArguments: ["--test-observed-authorization", "authorized"]
        )
        defer { stopIfNeeded(replacement) }
        let replacementHealth = try waitForHealth(socketURL: layout.healthSocket) { snapshot in
            snapshot.processIdentity?.pid == replacement.processIdentifier
                && snapshot.namespaceOwnersRestored == true
        }
        #expect(replacementHealth.processIdentity != identity)
        #expect(Darwin.kill(replacement.processIdentifier, SIGTERM) == 0)
        try waitForProcessMismatch(#require(replacementHealth.processIdentity))
    }

    @Test func exactIdentityPrepareCommitExitsTheObservedAgentProcess() throws {
        let root = try temporaryRoot()
        defer { try? FileManager.default.removeItem(at: root) }

        let agent = try startAgent(root: root)
        defer { stopIfNeeded(agent) }

        let layout = AgentRuntimeLayout(dataRoot: root)
        let initial = try waitForHealth(socketURL: layout.healthSocket)
        let identity = try #require(initial.processIdentity)
        #expect(identity.isValidTerminationIdentity)
        #expect(identity.pid == agent.processIdentifier)

        let stale = ControlTerminationRequest(
            expectedAgentInstanceID: UUID(), reason: .userQuit
        )
        let staleEvent = try ControlClient.command(
            ControlRequest(operation: .prepareForTermination, termination: stale),
            socketURL: layout.controlSocket,
            timeout: .seconds(2)
        )
        #expect(
            staleEvent == .commandFailed(
                ControlCommandFailure(
                    category: .invalidArgument,
                    detail: "termination prepare was not accepted"
                )
            )
        )
        #expect(try waitForHealth(socketURL: layout.healthSocket).processIdentity == identity)

        let request = ControlTerminationRequest(
            expectedAgentInstanceID: identity.instanceID, reason: .userQuit
        )
        let prepareEvent = try ControlClient.command(
            ControlRequest(operation: .prepareForTermination, termination: request),
            socketURL: layout.controlSocket,
            timeout: .seconds(2)
        )
        #expect(prepareEvent == .commandDone)
        let ready = try waitForTerminationReady(
            socketURL: layout.healthSocket, request: request, identity: identity
        )
        #expect(ready.processIdentity == identity)

        // Register the observer before crossing the irreversible commit
        // boundary; a launchd replacement must never hide the old process.
        let exited = ProcessExitFlag()
        let observer = DispatchSource.makeProcessSource(
            identifier: pid_t(identity.pid), eventMask: .exit, queue: .global(qos: .userInitiated)
        )
        observer.setEventHandler { exited.mark() }
        observer.resume()
        defer { observer.cancel() }

        var commit = request
        commit.action = .commit
        let commitEvent = try ControlClient.command(
            ControlRequest(operation: .prepareForTermination, termination: commit),
            socketURL: layout.controlSocket,
            timeout: .seconds(2)
        )
        #expect(commitEvent == .terminationCommitAccepted)

        try waitForExit(identity: identity, observer: exited)
        #expect(!processStillMatches(identity))
        #expect(exited.isMarked)
    }

    @Test func watchdogForcesExactProcessExitWhenOrdinaryCommittedExitStalls() throws {
        let root = try temporaryRoot()
        defer { try? FileManager.default.removeItem(at: root) }

        let agent = try startAgent(
            root: root,
            extraArguments: ["--test-committed-exit-delay-ms", "4000"]
        )
        defer { stopIfNeeded(agent) }

        let layout = AgentRuntimeLayout(dataRoot: root)
        let identity = try #require(try waitForHealth(socketURL: layout.healthSocket).processIdentity)
        let request = ControlTerminationRequest(
            expectedAgentInstanceID: identity.instanceID, reason: .userQuit
        )
        _ = try ControlClient.command(
            ControlRequest(operation: .prepareForTermination, termination: request),
            socketURL: layout.controlSocket,
            timeout: .seconds(2)
        )
        _ = try waitForTerminationReady(
            socketURL: layout.healthSocket, request: request, identity: identity
        )

        let exited = ProcessExitFlag()
        let observer = DispatchSource.makeProcessSource(
            identifier: pid_t(identity.pid), eventMask: .exit, queue: .global(qos: .userInitiated)
        )
        observer.setEventHandler { exited.mark() }
        observer.resume()
        defer { observer.cancel() }

        var commit = request
        commit.action = .commit
        let beforeCommit = ContinuousClock.now
        let descriptor = try ControlClient.connect(
            socketURL: layout.controlSocket, receiveTimeout: .seconds(2)
        )
        try ControlClient.writeLine(
            ControlRequest(operation: .prepareForTermination, termination: commit),
            to: descriptor,
            path: layout.controlSocket.path
        )
        // Drop the distinct commit-acceptance response after the server has
        // had a chance to write it. The pre-registered process observer, not
        // this socket acknowledgement, is the terminal witness.
        Thread.sleep(forTimeInterval: 0.05)
        Darwin.close(descriptor)
        try waitForExit(identity: identity, observer: exited)

        #expect(beforeCommit.duration(to: ContinuousClock.now) < .milliseconds(3500))
        #expect(!processStillMatches(identity))
    }

    @Test func droppedPrepareReplyRollsBackTheSameLiveProcessAtItsReadyLease() throws {
        let root = try temporaryRoot()
        defer { try? FileManager.default.removeItem(at: root) }

        let agent = try startAgent(
            root: root,
            extraArguments: ["--test-termination-commit-lease-ms", "250"]
        )
        defer { stopIfNeeded(agent) }

        let layout = AgentRuntimeLayout(dataRoot: root)
        let identity = try #require(try waitForHealth(socketURL: layout.healthSocket).processIdentity)
        let request = ControlTerminationRequest(
            expectedAgentInstanceID: identity.instanceID, reason: .userQuit
        )
        let descriptor = try ControlClient.connect(
            socketURL: layout.controlSocket, receiveTimeout: .seconds(2)
        )
        try ControlClient.writeLine(
            ControlRequest(operation: .prepareForTermination, termination: request),
            to: descriptor,
            path: layout.controlSocket.path
        )
        // The server has a chance to acknowledge, but this peer intentionally
        // never reads the result. The ready lease must return this exact
        // process to serving rather than allowing a late exit.
        Thread.sleep(forTimeInterval: 0.05)
        Darwin.close(descriptor)

        let recovered = try waitForTerminationCancellation(
            socketURL: layout.healthSocket, request: request, identity: identity
        )
        #expect(recovered.processIdentity == identity)
        #expect(processStillMatches(identity))
        #expect(recovered.transferAdmissionOpen == true)
        #expect(recovered.namespaceOwnersRestored == true)
        #expect(recovered.servingGeneration != nil)
        let controlEvent = try ControlClient.command(
            ControlRequest(operation: .status), socketURL: layout.controlSocket,
            timeout: .seconds(2)
        )
        guard case let .status(controlSnapshot) = controlEvent else {
            Issue.record("rollback must restore the control endpoint before it reports cancellation")
            return
        }
        #expect(controlSnapshot.processIdentity == recovered.processIdentity)
        #expect(controlSnapshot.terminationRequestID == request.requestID)
        #expect(controlSnapshot.state == .terminationCancelled)
        #expect(controlSnapshot.transferAdmissionOpen == true)
        #expect(controlSnapshot.namespaceOwnersRestored == true)
        #expect(controlSnapshot.servingGeneration == recovered.servingGeneration)
    }

    private func temporaryRoot() throws -> URL {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(
            "gramdrive-agent-process-\(UUID().uuidString)", isDirectory: true
        )
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return root
    }

    private func startAgent(root: URL, extraArguments: [String] = []) throws -> Process {
        let agent = Process()
        agent.executableURL = try agentExecutable()
        agent.arguments = [
            "run",
            "--data-root", root.path,
            "--drain-grace-ms", "25",
            "--drain-cancel-wait-ms", "25",
        ] + extraArguments
        agent.standardOutput = Pipe()
        agent.standardError = Pipe()
        try agent.run()
        return agent
    }

    private func seedAuthorizedAccount(dataRoot: URL, accountId: Int64) throws {
        do {
            let store = try SharedState.open(dataRoot: dataRoot, role: .coordinator)
            _ = try store.schemaVersion()
        }
        let layout = try SharedState.layout(dataRoot: dataRoot)
        var database: OpaquePointer?
        guard sqlite3_open_v2(
            layout.databaseFile,
            &database,
            SQLITE_OPEN_READWRITE,
            nil
        ) == SQLITE_OK, let database else {
            throw AgentProcessTestError.databaseUnavailable
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
            throw AgentProcessTestError.databaseUnavailable
        }
        defer { sqlite3_finalize(statement) }
        guard sqlite3_bind_int64(statement, 1, accountId) == SQLITE_OK,
              sqlite3_step(statement) == SQLITE_DONE
        else {
            throw AgentProcessTestError.databaseUnavailable
        }
    }

    private func stopIfNeeded(_ agent: Process) {
        let identity = processIdentity(pid: agent.processIdentifier)
        if processStillMatches(identity) {
            _ = Darwin.kill(agent.processIdentifier, SIGKILL)
        }
    }

    private func agentExecutable() throws -> URL {
        let source = URL(fileURLWithPath: #filePath)
        let packageRoot = source
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
        let buildRoot = packageRoot.appendingPathComponent(".build", isDirectory: true)
        let candidates = try FileManager.default.contentsOfDirectory(
            at: buildRoot, includingPropertiesForKeys: nil
        )
        .map { $0.appendingPathComponent("debug/gramdrive-agent") }
        guard let executable = candidates.first(where: {
            FileManager.default.isExecutableFile(atPath: $0.path)
        }) else {
            throw AgentProcessTestError.agentExecutableMissing
        }
        return executable
    }

    private func waitForHealth(
        socketURL: URL,
        where predicate: (AgentHealthSnapshot) -> Bool = { _ in true }
    ) throws -> AgentHealthSnapshot {
        let deadline = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < deadline {
            if let snapshot = try? AgentHealthClient.fetch(
                socketURL: socketURL,
                timeout: .milliseconds(100)
            ), predicate(snapshot) {
                return snapshot
            }
            Thread.sleep(forTimeInterval: 0.02)
        }
        throw AgentProcessTestError.healthUnavailable
    }

    private func waitForTerminationReady(
        socketURL: URL,
        request: ControlTerminationRequest,
        identity: AgentProcessIdentity
    ) throws -> AgentHealthSnapshot {
        let deadline = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < deadline {
            if let snapshot = try? AgentHealthClient.fetch(socketURL: socketURL, timeout: .milliseconds(100)),
               snapshot.terminationRequestID == request.requestID,
               snapshot.processIdentity == identity,
               snapshot.state == .terminationReady
            {
                return snapshot
            }
            Thread.sleep(forTimeInterval: 0.02)
        }
        throw AgentProcessTestError.terminationNotReady
    }

    private func waitForTerminationCancellation(
        socketURL: URL,
        request: ControlTerminationRequest,
        identity: AgentProcessIdentity
    ) throws -> AgentHealthSnapshot {
        let deadline = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < deadline {
            if let snapshot = try? AgentHealthClient.fetch(socketURL: socketURL, timeout: .milliseconds(100)),
               snapshot.terminationRequestID == request.requestID,
               snapshot.processIdentity == identity,
               snapshot.state == .terminationCancelled
            {
                return snapshot
            }
            Thread.sleep(forTimeInterval: 0.02)
        }
        throw AgentProcessTestError.terminationDidNotRollBack
    }

    private func waitForExit(identity: AgentProcessIdentity, observer: ProcessExitFlag) throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < deadline {
            if observer.isMarked, !processStillMatches(identity) { return }
            Thread.sleep(forTimeInterval: 0.02)
        }
        throw AgentProcessTestError.processDidNotExit
    }

    private func waitForProcessMismatch(_ identity: AgentProcessIdentity) throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while ContinuousClock.now < deadline {
            if !processStillMatches(identity) { return }
            Thread.sleep(forTimeInterval: 0.02)
        }
        throw AgentProcessTestError.processDidNotExit
    }

    private func waitForFile(_ url: URL) throws {
        let deadline = ContinuousClock.now + .seconds(2)
        while ContinuousClock.now < deadline {
            if FileManager.default.fileExists(atPath: url.path) { return }
            Thread.sleep(forTimeInterval: 0.01)
        }
        throw AgentProcessTestError.namespaceCloseDidNotStart
    }

    private func processIdentity(pid: Int32) -> AgentProcessIdentity {
        var info = proc_bsdinfo()
        let count = proc_pidinfo(
            pid, PROC_PIDTBSDINFO, 0, &info,
            Int32(MemoryLayout<proc_bsdinfo>.size)
        )
        return AgentProcessIdentity(
            instanceID: UUID(),
            pid: pid,
            kernelStartSeconds: count == MemoryLayout<proc_bsdinfo>.size ? Int64(info.pbi_start_tvsec) : 0,
            kernelStartMicroseconds: count == MemoryLayout<proc_bsdinfo>.size ? Int64(info.pbi_start_tvusec) : 0
        )
    }

    private func processStillMatches(_ identity: AgentProcessIdentity) -> Bool {
        guard identity.isValidTerminationIdentity else { return false }
        var info = proc_bsdinfo()
        let count = proc_pidinfo(
            identity.pid, PROC_PIDTBSDINFO, 0, &info,
            Int32(MemoryLayout<proc_bsdinfo>.size)
        )
        guard count == MemoryLayout<proc_bsdinfo>.size else { return false }
        return Int64(info.pbi_start_tvsec) == identity.kernelStartSeconds
            && Int64(info.pbi_start_tvusec) == identity.kernelStartMicroseconds
    }

    private enum AgentProcessTestError: Error {
        case agentExecutableMissing
        case databaseUnavailable
        case healthUnavailable
        case namespaceCloseDidNotStart
        case terminationNotReady
        case terminationDidNotRollBack
        case processDidNotExit
    }
}
