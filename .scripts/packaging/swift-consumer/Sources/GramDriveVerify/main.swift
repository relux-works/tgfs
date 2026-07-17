// Minimal Swift consumer of the packaged GramDrive core.
//
// Run by .scripts/packaging/build_core_artifacts.py against the staged
// artifact. Two jobs, both narrow on purpose:
//
//  1. Prove the packaged artifact is *consumable*: this file imports
//     GramDriveCore and nothing else, so it compiles only if the XCFramework,
//     its headers, its modulemap, and the generated Swift sources all survived
//     packaging and link on macOS arm64 (POL-5).
//
//  2. Report the contract version *as the built binary reports it*, on stdout
//     as JSON. The packaging script records that value in the manifest rather
//     than parsing it out of Rust source, so the manifest cannot claim a
//     version the shipped binary does not actually implement. Same principle
//     as UniFFI library mode: the binary is the source of truth.
//
// The contract's own guarantees are the bindings smoke gate's job
// (.scripts/smoke/, TASK-260715-265gqq). probe_transfer is exercised here only
// far enough to show the linked core really executes across the boundary — a
// package that compiles but traps on the first call is not "consumable".

import Foundation
import GramDriveCore

final class RecordingListener: ProgressListener, @unchecked Sendable {
    private let lock = NSLock()
    private var events: [UInt64] = []

    func onProgress(progress: TransferProgress) {
        lock.lock()
        events.append(progress.bytesTransferred)
        lock.unlock()
    }

    var snapshot: [UInt64] {
        lock.lock()
        defer { lock.unlock() }
        return events
    }
}

func fail(_ reason: String) -> Never {
    FileHandle.standardError.write(Data("VERIFY FAILED: \(reason)\n".utf8))
    exit(1)
}

// -- The artifact reports its own contract version -----------------------

let version = contractVersion()

// -- The linked core actually runs across the boundary -------------------

let core: DriveCore
do {
    core = try DriveCore(
        config: CoreConfig(dataDir: FileManager.default.temporaryDirectory.path)
    )
} catch {
    fail("DriveCore construction threw: \(error)")
}

let listener = RecordingListener()
let transferred: UInt64
do {
    transferred = try await core.probeTransfer(
        totalBytes: 100,
        chunkBytes: 40,
        chunkDelayMs: 1,
        listener: listener,
        token: CancellationToken()
    )
} catch {
    fail("probeTransfer threw: \(error)")
}

guard transferred == 100 else {
    fail("probeTransfer returned \(transferred), expected 100")
}

// Progress callbacks cross back from Rust into Swift; if packaging dropped the
// callback scaffolding this is where it shows up rather than at link time.
let progress = listener.snapshot
guard progress == [40, 80, 100] else {
    fail("progress events were \(progress), expected [40, 80, 100]")
}

// stdout is parsed by the packaging script: keep it a single JSON object.
let report: [String: Any] = [
    "contract_version": "\(version.major).\(version.minor).\(version.patch)",
    "probe_transferred_bytes": Int(transferred),
    "progress_events": progress.map(Int.init),
]
let json = try JSONSerialization.data(withJSONObject: report, options: [.sortedKeys])
print(String(decoding: json, as: UTF8.self))
