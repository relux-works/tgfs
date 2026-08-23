import Foundation
import GramDriveCore
import GramDriveSupport
import Network

/// Privacy-safe state emitted by one account's owned content namespace.
public enum AgentNamespaceProgress: Equatable, Sendable {
    case preparing
    case authorized
    case folderCatalog
    case snapshotList
    case projectionSlice(processedChatCount: UInt64)
    case ready(canonicalChatCount: UInt64, appearanceCount: UInt64)
    case degraded(category: String, retryable: Bool)
    case failed(category: String, retryable: Bool)
    case stopped
}

/// Foreground history demand accepted by the lifecycle-owned session.
public enum AgentChatHistoryPriority: Equatable, Sendable {
    case background
    case requested
    case visible
}

/// Owned handle for one long-lived account namespace.
public protocol AgentNamespaceSessionHosting: AnyObject, Sendable {
    func setChatHistoryPriority(chatId: Int64, priority: AgentChatHistoryPriority) throws
    func close()
}

/// Testable construction seam for the Rust TDLib namespace owner.
public protocol AgentNamespaceBootstrapping: Sendable {
    func start(
        accountId: Int64,
        onProgress: @escaping @Sendable (AgentNamespaceProgress) -> Void
    ) throws -> any AgentNamespaceSessionHosting
}

/// Production bridge to the FFI namespace session. The Rust worker owns
/// TDLib, normalized persistence, bounded paging, and live updates; the
/// native lifecycle owns its lifetime and File Provider doorbell.
public struct CoreNamespaceBootstrapper: AgentNamespaceBootstrapping {
    private let configuration: CoreAuthConfiguration
    private let vault: any SecretVault

    public init(configuration: CoreAuthConfiguration, vault: any SecretVault) {
        self.configuration = configuration
        self.vault = vault
    }

    public func start(
        accountId: Int64,
        onProgress: @escaping @Sendable (AgentNamespaceProgress) -> Void
    ) throws -> any AgentNamespaceSessionHosting {
        let relay = CoreNamespaceProgressRelay(onProgress: onProgress)
        let session = try NamespaceSession.start(
            config: configuration.sessionConfig(),
            accountId: accountId,
            vault: vault,
            listener: relay)
        return CoreNamespaceSessionHost(
            session: session,
            relay: relay,
            dataRoot: configuration.dataRoot)
    }
}

private final class CoreNamespaceSessionHost: AgentNamespaceSessionHosting,
    @unchecked Sendable
{
    private let session: NamespaceSession
    // UniFFI retains callback interfaces for the Rust handle, but retaining
    // the relay here also makes its lifetime explicit at the native boundary.
    private let relay: CoreNamespaceProgressRelay
    private let archiveConditions: ArchiveHostConditionsMonitor

    init(session: NamespaceSession, relay: CoreNamespaceProgressRelay, dataRoot: URL) {
        self.session = session
        self.relay = relay
        archiveConditions = ArchiveHostConditionsMonitor(dataRoot: dataRoot) { conditions in
            session.setArchiveHostConditions(conditions: conditions)
        }
        archiveConditions.start()
    }

    func setChatHistoryPriority(chatId: Int64, priority: AgentChatHistoryPriority) throws {
        let corePriority: ChatHistoryPriority = switch priority {
        case .background: .background
        case .requested: .requested
        case .visible: .visible
        }
        try session.setChatHistoryPriority(chatId: chatId, priority: corePriority)
    }

    func close() {
        archiveConditions.cancel()
        session.shutdown()
    }
}

/// Supplies real host policy inputs to the fail-closed Rust Archive scheduler.
///
/// Network callbacks and the periodic power/disk sample share one private
/// queue, so a session never observes a torn host snapshot. Unknown disk
/// capacity is reported as low and therefore cannot accidentally admit an
/// eager download.
private final class ArchiveHostConditionsMonitor: @unchecked Sendable {
    private static let lowDiskBytes: Int64 = 2 * 1_024 * 1_024 * 1_024
    private static let criticalDiskBytes: Int64 = 1 * 1_024 * 1_024 * 1_024

    private let dataRoot: URL
    private let report: @Sendable (ArchiveHostConditions) -> Void
    private let queue = DispatchQueue(label: "works.relux.gramdrive.archive-host-conditions")
    private let pathMonitor = NWPathMonitor()
    private var timer: DispatchSourceTimer?
    private var latestPath: NWPath?

    init(
        dataRoot: URL,
        report: @escaping @Sendable (ArchiveHostConditions) -> Void
    ) {
        self.dataRoot = dataRoot
        self.report = report
    }

    func start() {
        pathMonitor.pathUpdateHandler = { [weak self] path in
            guard let self else { return }
            latestPath = path
            publish()
        }
        pathMonitor.start(queue: queue)
        queue.async { [weak self] in
            guard let self else { return }
            let timer = DispatchSource.makeTimerSource(queue: queue)
            timer.schedule(deadline: .now(), repeating: .seconds(5), leeway: .seconds(1))
            timer.setEventHandler { [weak self] in self?.publish() }
            self.timer = timer
            timer.resume()
        }
    }

    func cancel() {
        pathMonitor.cancel()
        queue.async { [weak self] in
            self?.timer?.cancel()
            self?.timer = nil
        }
    }

    private func publish() {
        let network: ArchiveNetworkCondition
        switch latestPath?.status {
        case .satisfied:
            if latestPath?.isExpensive == true || latestPath?.isConstrained == true {
                network = .metered
            } else {
                network = .online
            }
        case .requiresConnection, .unsatisfied, .none:
            network = .offline
        @unknown default:
            network = .offline
        }

        let power: ArchivePowerCondition =
            ProcessInfo.processInfo.isLowPowerModeEnabled ? .saving : .unconstrained
        let disk: ArchiveDiskCondition
        let values = try? dataRoot.resourceValues(
            forKeys: [.volumeAvailableCapacityForImportantUsageKey])
        if let available = values?.volumeAvailableCapacityForImportantUsage {
            if available < Self.criticalDiskBytes {
                disk = .critical
            } else if available < Self.lowDiskBytes {
                disk = .low
            } else {
                disk = .ample
            }
        } else {
            disk = .low
        }
        report(ArchiveHostConditions(network: network, power: power, disk: disk))
    }
}

private final class CoreNamespaceProgressRelay: NamespaceProgressListener,
    @unchecked Sendable
{
    private let onProgress: @Sendable (AgentNamespaceProgress) -> Void

    init(onProgress: @escaping @Sendable (AgentNamespaceProgress) -> Void) {
        self.onProgress = onProgress
    }

    func onProgress(progress: NamespaceProgress) {
        switch progress {
        case .preparing:
            onProgress(.preparing)
        case .authorized:
            onProgress(.authorized)
        case .folderCatalog:
            onProgress(.folderCatalog)
        case .snapshotList:
            onProgress(.snapshotList)
        case let .projectionSlice(processedChatCount):
            onProgress(.projectionSlice(processedChatCount: processedChatCount))
        case .ready(let canonicalChatCount, let appearanceCount):
            onProgress(
                .ready(
                    canonicalChatCount: canonicalChatCount,
                    appearanceCount: appearanceCount))
        case .degraded(let category, let retryable):
            onProgress(.degraded(category: category, retryable: retryable))
        case .failed(let category, let retryable):
            onProgress(.failed(category: category, retryable: retryable))
        case .stopped:
            onProgress(.stopped)
        }
    }
}
