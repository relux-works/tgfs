// GramDrive bindings smoke consumer (Swift).
//
// Compiled against the generated GramDriveCore.swift by
// .scripts/smoke/run_bindings_smoke.py; proves the acceptance criteria of
// TASK-260715-265gqq executable-style: the contract surface compiles from
// Swift, async operations and progress callbacks work, structured errors
// round-trip as DriveError cases, and CancellationToken cancellation
// round-trips as DriveError.Cancelled with no callbacks afterwards.

import Foundation

var failures = 0

func check(_ condition: Bool, _ name: String) {
    if condition {
        print("ok - \(name)")
    } else {
        failures += 1
        print("FAIL - \(name)")
    }
}

final class RecordingListener: ProgressListener, @unchecked Sendable {
    private let lock = NSLock()
    private var events: [TransferProgress] = []

    func onProgress(progress: TransferProgress) {
        lock.lock()
        events.append(progress)
        lock.unlock()
    }

    var snapshot: [TransferProgress] {
        lock.lock()
        defer { lock.unlock() }
        return events
    }
}

// -- Contract version ---------------------------------------------------

let version = contractVersion()
check(
    version == ContractVersion(major: 0, minor: 1, patch: 0),
    "contract version is 0.1.0"
)

// -- Constructor validation error round-trips ---------------------------

do {
    _ = try DriveCore(config: CoreConfig(dataDir: ""))
    check(false, "empty dataDir must throw")
} catch let error as DriveError {
    if case .InvalidArgument = error {
        check(true, "constructor error round-trips as DriveError.InvalidArgument")
    } else {
        check(false, "unexpected DriveError from constructor: \(error)")
    }
}

let core = try DriveCore(
    config: CoreConfig(dataDir: FileManager.default.temporaryDirectory.path)
)

// -- Async success path with progress callbacks -------------------------

let listener = RecordingListener()
let transferred = try await core.probeTransfer(
    totalBytes: 100,
    chunkBytes: 40,
    chunkDelayMs: 10,
    listener: listener,
    token: CancellationToken()
)
check(transferred == 100, "probe returns total bytes")
check(
    listener.snapshot.map { $0.bytesTransferred } == [40, 80, 100],
    "progress reports each chunk"
)
check(
    listener.snapshot.allSatisfy { $0.bytesTotal == 100 },
    "progress carries the known total"
)

// -- Async structured error round-trips ----------------------------------

do {
    _ = try await core.probeTransfer(
        totalBytes: 100,
        chunkBytes: 0,
        chunkDelayMs: 1,
        listener: RecordingListener(),
        token: CancellationToken()
    )
    check(false, "zero chunkBytes must throw")
} catch let error as DriveError {
    if case .InvalidArgument = error {
        check(true, "async error round-trips as DriveError.InvalidArgument")
    } else {
        check(false, "unexpected DriveError from probe: \(error)")
    }
}

// -- Cancellation round-trips --------------------------------------------
// Without cancellation this probe would take ~28 hours (1M chunks x 100 ms),
// so the test completing at all proves the token interrupted it.

let cancelListener = RecordingListener()
let token = CancellationToken()
let probeTask = Task {
    try await core.probeTransfer(
        totalBytes: 1_000_000,
        chunkBytes: 1,
        chunkDelayMs: 100,
        listener: cancelListener,
        token: token
    )
}
try await Task.sleep(nanoseconds: 350_000_000)
token.cancel()
check(token.isCancelled(), "token reports cancelled state")
do {
    _ = try await probeTask.value
    check(false, "cancelled probe must throw")
} catch let error as DriveError {
    if case .Cancelled = error {
        check(true, "cancellation round-trips as DriveError.Cancelled")
    } else {
        check(false, "unexpected DriveError from cancelled probe: \(error)")
    }
}
let seenAtCancel = cancelListener.snapshot.count
check(seenAtCancel < 20, "cancellation interrupted the probe early")
try await Task.sleep(nanoseconds: 500_000_000)
check(
    cancelListener.snapshot.count == seenAtCancel,
    "no progress callbacks after cancellation"
)

print(failures == 0 ? "SWIFT SMOKE PASSED" : "SWIFT SMOKE FAILED (\(failures))")
exit(failures == 0 ? 0 : 1)
