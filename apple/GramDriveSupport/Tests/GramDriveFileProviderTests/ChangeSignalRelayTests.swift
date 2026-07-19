import Foundation
import Testing

@testable import GramDriveFileProvider

/// Records working-set signals instead of talking to the file provider
/// daemon.
private final class RecordingSignaling: WorkingSetSignaling, @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0

    var signalCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return count
    }

    func signalWorkingSet(completionHandler: @escaping @Sendable ((any Error)?) -> Void) {
        lock.lock()
        count += 1
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
    @Test("Start probes once — covering rings missed while not running — and signals")
    func startSignalsOnce() throws {
        let signaling = RecordingSignaling()
        let probe = ScriptedProbe([.success(1)])
        let relay = ChangeSignalRelay(probe: { try probe.next() }, signaling: signaling)
        let token = RecordingToken()
        try relay.start(observe: { _ in token })
        #expect(signaling.signalCount == 1, "the first probe always differs from 'never probed'")
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
}
