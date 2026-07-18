// Shared-state smoke process (TASK-260715-gnsa2s), driven by
// .scripts/smoke/run_shared_state_smoke.py. One binary, three modes:
//
//   read    — open the container as a provider and print every item's
//             metadata as deterministic lines. The harness runs two of
//             these concurrently and the outputs must be byte-identical:
//             that is the "two processes read consistent item metadata"
//             proof.
//   watch   — open as a provider, report readiness, then wait until BOTH
//             the change doorbell has rung AND dataVersion() has moved
//             (the harness mutates via the Rust seeder and rings via
//             `signal`); print what the re-read observed.
//   signal  — ring the change doorbell and exit (the writer host's
//             post-commit step, isolated).
//
// The container path arrives via --container; the data root is *derived*
// here through AppGroup.dataRootURL, deliberately — if the Swift-side
// derivation ever disagrees with the layout the seeder wrote under, every
// read comes back empty and the smoke fails.

import Foundation
import GramDriveCore
import GramDriveSupport

// The harness reads this process through a pipe and synchronizes on
// individual lines (WATCH-READY); stdio would block-buffer a pipe.
setbuf(stdout, nil)

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data((message + "\n").utf8))
    exit(1)
}

func describe(_ item: ItemMetadata) -> String {
    let content = item.contentVersion ?? "-"
    let size = item.logicalSize.map(String.init) ?? "-"
    let mime = item.mimeType ?? "-"
    let parent = item.parent ?? "-"
    return "item id=\(item.id) parent=\(parent) kind=\(item.kind) dir=\(item.isDirectory) "
        + "name=\(item.safeName) meta=\(item.metadataVersion) content=\(content) "
        + "size=\(size) mime=\(mime) availability=\(item.availability)"
}

/// Depth-first listing in stable id order, paged two at a time so the
/// smoke also exercises page anchoring, not just one big page.
func printTree(store: SharedStateStore, rootId: String) throws {
    guard let root = try store.item(id: rootId) else {
        fail("root item \(rootId) not found")
    }
    print(describe(root))
    var stack: [String] = [root.id]
    while let parent = stack.popLast() {
        var after: String? = nil
        while true {
            let page = try store.children(parent: parent, after: after, limit: 2)
            for child in page {
                print(describe(child))
                if child.isDirectory {
                    stack.append(child.id)
                }
            }
            guard let last = page.last, page.count == 2 else { break }
            after = last.id
        }
    }
}

// -- Argument parsing ---------------------------------------------------

var container: String?
var mode = "read"
var rootId: String?
var timeoutSeconds = 30.0

var arguments = Array(CommandLine.arguments.dropFirst()).makeIterator()
while let argument = arguments.next() {
    switch argument {
    case "--container": container = arguments.next()
    case "--mode": mode = arguments.next() ?? mode
    case "--root": rootId = arguments.next()
    case "--timeout": timeoutSeconds = arguments.next().flatMap(Double.init) ?? timeoutSeconds
    default: fail("unknown argument \(argument)")
    }
}

if mode == "signal" {
    ChangeSignal.post()
    print("SIGNALED")
    exit(0)
}

guard let container else {
    fail("--container is required for mode \(mode)")
}
let dataRoot = AppGroup.dataRootURL(containerURL: URL(fileURLWithPath: container))

do {
    let store = try SharedState.open(dataRoot: dataRoot, role: .provider)

    switch mode {
    case "read":
        guard let rootId else { fail("--root is required for mode read") }
        print("schema_version=\(try store.schemaVersion())")
        try printTree(store: store, rootId: rootId)

    case "watch":
        guard let rootId else { fail("--root is required for mode watch") }
        let initialVersion = try store.dataVersion()

        // Sendable flag flipped from the doorbell's dispatch queue.
        final class Flag: @unchecked Sendable {
            private let lock = NSLock()
            private var value = false
            func raise() {
                lock.lock()
                value = true
                lock.unlock()
            }
            func isRaised() -> Bool {
                lock.lock()
                defer { lock.unlock() }
                return value
            }
        }
        let rung = Flag()
        let observation = try ChangeSignal.observe { rung.raise() }
        defer { observation.cancel() }

        print("WATCH-READY data_version=\(initialVersion)")

        // Wait for both proofs: the doorbell (cross-process notification
        // path) and a moved data version (cross-process commit
        // visibility). Polling is the fallback the design prescribes for
        // missed doorbells, so a slow ring never deadlocks the wait.
        let deadline = Date(timeIntervalSinceNow: timeoutSeconds)
        var versionMoved = false
        while Date() < deadline {
            if !versionMoved {
                versionMoved = try store.dataVersion() != initialVersion
            }
            if versionMoved && rung.isRaised() {
                guard let file = try store.item(id: rootId) else {
                    fail("watched item \(rootId) disappeared")
                }
                let content = file.contentVersion ?? "-"
                let size = file.logicalSize.map(String.init) ?? "-"
                print("CHANGED signaled=true content=\(content) size=\(size)")
                exit(0)
            }
            Thread.sleep(forTimeInterval: 0.05)
        }
        fail("TIMEOUT signaled=\(rung.isRaised()) version_moved=\(versionMoved)")

    default:
        fail("unknown mode \(mode)")
    }
} catch {
    fail("ERROR: \(error)")
}
