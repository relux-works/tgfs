// Shared-state smoke process (TASK-260715-gnsa2s), driven by
// .scripts/smoke/run_shared_state_smoke.py. One binary, four modes:
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
//   domains — the File Provider domain chain (TASK-260715-3s44pc): read
//             the seeded accounts, derive the desired domain set, then
//             construct the real extension type against its domain and
//             resolve the account context through shared state — the
//             cross-process proof that a provider extension process maps
//             its domain back to the account the Rust coordinator wrote.
//
// The container path arrives via --container; the data root is *derived*
// here through AppGroup.dataRootURL, deliberately — if the Swift-side
// derivation ever disagrees with the layout the seeder wrote under, every
// read comes back empty and the smoke fails.

import FileProvider
import Foundation
import GramDriveCore
import GramDriveFileProvider
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

/// The reserved-value-aware label of a File Provider identifier, so the
/// smoke can assert the account root folds onto `.rootContainer`.
func identifierLabel(_ identifier: NSFileProviderItemIdentifier) -> String {
    identifier == .rootContainer ? "rootContainer" : identifier.rawValue
}

/// Whether an item advertises any mutating capability — the read-only
/// invariant (DEC-007 / SYNC-060) the domains mode proves cross-process.
func hasMutatingCapability(_ capabilities: NSFileProviderItemCapabilities) -> Bool {
    let mutating: NSFileProviderItemCapabilities = [
        .allowsWriting, .allowsRenaming, .allowsReparenting,
        .allowsTrashing, .allowsDeleting, .allowsAddingSubItems,
    ]
    return !capabilities.intersection(mutating).isEmpty
}

/// The item's non-mutating access, by kind. On macOS
/// `AllowsContentEnumerating` and `AllowsReading` are the *same* bit, so the
/// meaning is read from the kind: a directory enumerates, a fetchable file
/// reads, and a restricted/unavailable file grants nothing.
func accessLabel(_ item: GramDriveFileProviderItem) -> String {
    if item.metadata.isDirectory {
        return item.capabilities.contains(.allowsContentEnumerating) ? "enumerate" : "none"
    }
    return item.capabilities.contains(.allowsReading) ? "read" : "none"
}

/// One mapped item as a deterministic line: identity folding, the on-disk
/// name and content type, size, and the read-only capability surface — the
/// item mapping (TASK-260715-i3mp9x) proven over metadata a separate
/// coordinator process durably wrote.
func describeMapped(_ item: GramDriveFileProviderItem) -> String {
    let size = item.documentSize.map(String.init) ?? "-"
    return "map id=\(item.metadata.id) "
        + "identifier=\(identifierLabel(item.itemIdentifier)) "
        + "parent=\(identifierLabel(item.parentItemIdentifier)) "
        + "name=\(item.filename) type=\(item.contentType.identifier) size=\(size) "
        + "readonly=\(!hasMutatingCapability(item.capabilities)) "
        + "access=\(accessLabel(item))"
}

/// Maps every live item under the account root to `NSFileProviderItem` and
/// prints its mapping line, in the same paged depth-first order as
/// ``printTree``.
func printMappedTree(store: SharedStateStore, accountRootId: String) throws {
    guard let root = try store.item(id: accountRootId) else {
        fail("root item \(accountRootId) not found")
    }
    print(describeMapped(GramDriveFileProviderItem(metadata: root, accountRootId: accountRootId)))
    var stack: [String] = [root.id]
    while let parent = stack.popLast() {
        var after: String? = nil
        while true {
            let page = try store.children(parent: parent, after: after, limit: 2)
            for child in page {
                print(
                    describeMapped(
                        GramDriveFileProviderItem(metadata: child, accountRootId: accountRootId)))
                if child.isDirectory {
                    stack.append(child.id)
                }
            }
            guard let last = page.last, page.count == 2 else { break }
            after = last.id
        }
    }
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

    case "domains":
        // The account list a provider host maps domains from.
        let accounts = try store.accounts()
        print("accounts_count=\(accounts.count)")
        guard let account = accounts.first, accounts.count == 1 else {
            fail("expected exactly one seeded account")
        }
        print("account_id=\(account.accountId)")
        print("account_name=\(account.displayName)")
        print("account_root=\(account.rootItemId)")

        let desired = DomainIdentity.desiredDomains(for: accounts)
        guard let domain = desired.first, desired.count == 1 else {
            fail("expected exactly one desired domain")
        }
        print("domain_id=\(domain.identifier)")
        print("domain_name=\(domain.displayName)")

        // The real extension type, over the same substitute container:
        // domain identifier -> account -> root item, all through shared
        // state, in this separate process.
        let ext = GramDriveFileProviderExtension(
            domain: NSFileProviderDomain(
                identifier: NSFileProviderDomainIdentifier(rawValue: domain.identifier),
                displayName: domain.displayName
            ),
            dataRoot: { dataRoot }
        )
        let context = try ext.accountContext()
        guard let rootItem = try context.store.item(id: context.account.rootItemId) else {
            fail("extension found no root item for \(context.account.rootItemId)")
        }
        guard rootItem.kind == .account, rootItem.parent == nil else {
            fail("extension root item is not an account root: \(describe(rootItem))")
        }
        print("context_root=\(rootItem.id)")
        print("context_root_name=\(rootItem.safeName)")

        // The item mapping (TASK-260715-i3mp9x): map the seeded tree to
        // NSFileProviderItem and prove — cross-process, over metadata the
        // Rust coordinator durably wrote — that the account root folds onto
        // .rootContainer, a direct child reparents onto it, and no item at
        // any depth advertises a mutating capability.
        try printMappedTree(store: context.store, accountRootId: context.account.rootItemId)

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
