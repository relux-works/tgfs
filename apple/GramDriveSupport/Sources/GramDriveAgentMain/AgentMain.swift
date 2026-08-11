import Darwin
import Dispatch
import Foundation
import GramDriveAgentCore
import GramDriveCore
import GramDriveSupport

/// The GramDrive companion background agent (launch agent): the
/// engine-hosting coordinator process (PLAT-MAC-002, TASK-260715-1yx9ly).
///
///     gramdrive-agent run [options]      start the agent (default command)
///     gramdrive-agent health [options]   query a running agent's health
///
/// Options:
///     --container PATH        substitute container root (tests, smoke);
///                             the data root is derived by the same rule
///                             as for the real App Group container
///     --data-root PATH        explicit data root (overrides --container)
///     --drain-grace-ms N      drain grace period (run; default 10000)
///     --drain-cancel-wait-ms N  wait after cancellation (run; default 5000)
///     --probe-transfer-ms N   host a synthetic in-flight transfer of
///                             roughly N ms (the contract's boundary
///                             probe), so shutdown-drain behavior is
///                             observable end to end (smoke, diagnostics)
///     --timeout-ms N          health fetch timeout (health; default 5000)
///
/// Exit codes: 0 success/clean shutdown; 2 another agent already runs over
/// this container; 3 startup failure; 4 agent unavailable (health); 64
/// usage error.
@main
enum AgentMain {
    static func main() {
        var arguments = Array(CommandLine.arguments.dropFirst())
        var command = "run"
        if let first = arguments.first, !first.hasPrefix("--") {
            command = first
            arguments.removeFirst()
        }
        let options: [String: String]
        do {
            options = try parseOptions(arguments)
        } catch {
            fail("usage: \(error)", code: 64)
        }
        let dataRoot: URL
        do {
            dataRoot = try resolveDataRoot(options)
        } catch {
            fail("cannot resolve data root: \(error)", code: 3)
        }
        switch command {
        case "run":
            runAgent(dataRoot: dataRoot, options: options)
        case "health":
            fetchHealth(dataRoot: dataRoot, options: options)
        default:
            fail("unknown command '\(command)'", code: 64)
        }
    }

    // MARK: - run

    private static func runAgent(dataRoot: URL, options: [String: String]) {
        // The agent's own IPC channels set SO_NOSIGPIPE per socket, but the
        // engine's network sockets live inside libtdjson and carry the
        // process default: a peer resetting mid-write would kill the whole
        // agent. Ignored process-wide before any engine work starts, so a
        // dead peer surfaces as EPIPE to the writer that hit it.
        signal(SIGPIPE, SIG_IGN)
        let commitExitWatchdog = CommitExitWatchdog()
        guard commitExitWatchdog.install() else {
            fail("cannot install committed-exit watchdog", code: 3)
        }
        #if DEBUG
            let testTerminationCommitLease = integerOption(options, "test-termination-commit-lease-ms")
                .map { Duration.milliseconds($0) }
            let testCommittedExitDelay = integerOption(options, "test-committed-exit-delay-ms")
                .map { Duration.milliseconds($0) }
            let testTerminationHardExitWatchdog = integerOption(
                options, "test-termination-hard-exit-watchdog-ms"
            ).map { Duration.milliseconds($0) }
            let testFinderHierarchyReady =
                boolOption(options, "test-finder-hierarchy-ready") ?? false
            if testFinderHierarchyReady {
                AgentRuntimeTestOverrides.installFinderHierarchyReady()
            }
            if let testReportedBundleVersion = options["test-reported-bundle-version"],
               !AgentBuildVersion.installTestReportedBuild(testReportedBundleVersion)
            {
                fail("test-reported-bundle-version must be numeric", code: 64)
            }
        #else
            let testTerminationCommitLease: Duration? = nil
            let testCommittedExitDelay: Duration? = nil
            let testTerminationHardExitWatchdog: Duration? = nil
        #endif
        // The engine-backed control seams (BUG-260720-3i74u1): sign-in,
        // removal, and repair over the FFI's authorization surface, with
        // secrets from the OS keychain. Test-DC selection exists for the
        // acceptance smoke only (`--telegram-test-dc true` or the
        // GRAMDRIVE_TELEGRAM_TEST_DC env), never in a user-facing launch.
        let useTestDc =
            boolOption(options, "telegram-test-dc")
                ?? (ProcessInfo.processInfo.environment["GRAMDRIVE_TELEGRAM_TEST_DC"] == "1")
        let vault = KeychainSecretVault()
        let authConfiguration = CoreAuthConfiguration(dataRoot: dataRoot, useTestDc: useTestDc)
        let lifecycleRef = LifecycleRef()
        let terminationExit = TerminationExitGate(
            watchdog: commitExitWatchdog,
            testCommittedExitDelay: testCommittedExitDelay,
            hardExitWatchdogDelay: testTerminationHardExitWatchdog
                ?? CommitExitWatchdog.committedExitDeadline
        )
        let contentPolicy: CoreContentPolicyController
        do {
            contentPolicy = try CoreContentPolicyController(dataRoot: dataRoot)
        } catch {
            fail("cannot open content policy controller: \(error)", code: 3)
        }
        let seams = AgentControlSeams(
            authorizer: CoreAuthorizer(
                configuration: authConfiguration,
                vault: vault,
                beforeSession: { lifecycleRef.lifecycle?.stopAllNamespaces() },
                afterSession: { lifecycleRef.lifecycle?.restartNamespaces() }
            ),
            remover: CoreAccountRemover(
                configuration: authConfiguration,
                vault: vault,
                beforeRemoval: { lifecycleRef.lifecycle?.stopNamespace(accountId: $0) },
                afterFailure: { lifecycleRef.lifecycle?.restartNamespaces() }
            ),
            repairer: CoreRepairRunner(
                configuration: authConfiguration,
                vault: vault,
                accounts: {
                    guard let accounts = lifecycleRef.lifecycle?.healthSnapshot().accounts
                    else {
                        throw AgentStartError.storage(detail: "durable state is not open")
                    }
                    return accounts
                },
                beforeRepair: { lifecycleRef.lifecycle?.stopAllNamespaces() },
                afterRepair: { lifecycleRef.lifecycle?.restartNamespaces() },
                onSignedOutProbe: {
                    lifecycleRef.lifecycle?.recordAuthDiagnostic(.probeSignedOut)
                }
            ),
            contentPolicy: contentPolicy
        )
        let configuration = AgentConfiguration(
            dataRoot: dataRoot,
            drainGracePeriod: .milliseconds(integerOption(options, "drain-grace-ms") ?? 10000),
            drainCancelWait: .milliseconds(
                integerOption(options, "drain-cancel-wait-ms") ?? 5000
            ),
            powerEvents: WorkspacePowerEventSource(),
            controlSeams: seams,
            namespaceBootstrapper: CoreNamespaceBootstrapper(
                configuration: authConfiguration, vault: vault
            ),
            onTerminationAccepted: { request in
                terminationExit.request(request)
            },
            onTerminationCommitAccepted: { request in
                terminationExit.acceptCommit(request)
            },
            onTerminationCommitAcknowledged: { request in
                terminationExit.finishAcceptedCommit(request)
            },
            terminationCommitLease: testTerminationCommitLease ?? .seconds(5)
        )
        let lifecycle = AgentLifecycle(configuration: configuration)
        lifecycleRef.lifecycle = lifecycle
        terminationExit.bind(lifecycle)
        do {
            try lifecycle.start()
        } catch AgentStartError.alreadyRunning {
            fail("another agent already coordinates this container", code: 2)
        } catch {
            fail("startup failed: \(error)", code: 3)
        }

        emit(
            "agent: state=running pid=\(ProcessInfo.processInfo.processIdentifier) "
                + "socket=\(lifecycle.runtimeLayout.healthSocket.path)"
        )

        if let probeMs = integerOption(options, "probe-transfer-ms") {
            hostProbeTransfer(on: lifecycle, milliseconds: probeMs)
        }

        // launchd delivers shutdown as SIGTERM (unload, logout, update);
        // SIGINT covers interactive runs. Either drains, then exits 0.
        installShutdownSignals(for: lifecycle)
        dispatchMain()
    }

    /// Hosts one synthetic in-flight transfer through the real contract
    /// path — `DriveCore.probeTransfer` registered in the drain ledger with
    /// its cancellation token — so a drain has something real to drain.
    private static func hostProbeTransfer(on lifecycle: AgentLifecycle, milliseconds: Int) {
        guard let core = lifecycle.core else { return }
        let token = CancellationToken()
        let ticket: TransferTicket
        do {
            ticket = try lifecycle.transfers.begin(token: token)
        } catch {
            emit("probe-transfer: refused (draining)")
            return
        }
        let chunkDelayMs: UInt64 = 100
        let totalChunks = UInt64(max(1, milliseconds / Int(chunkDelayMs)))
        emit("probe-transfer: started total_ms=\(totalChunks * chunkDelayMs)")
        Task {
            defer { lifecycle.transfers.end(ticket) }
            do {
                _ = try await core.probeTransfer(
                    totalBytes: totalChunks,
                    chunkBytes: 1,
                    chunkDelayMs: chunkDelayMs,
                    listener: SilentProgressListener(),
                    token: token
                )
                emit("probe-transfer: completed")
            } catch {
                emit("probe-transfer: cancelled")
            }
        }
    }

    private static func installShutdownSignals(for lifecycle: AgentLifecycle) {
        for sig in [SIGTERM, SIGINT] {
            signal(sig, SIG_IGN)
            let source = DispatchSource.makeSignalSource(signal: sig, queue: .main)
            source.setEventHandler {
                Task {
                    let outcome = await lifecycle.shutdown(reason: .terminate)
                    emit(
                        "agent: drained completed=\(outcome.completed) "
                            + "cancelled=\(outcome.cancelled) abandoned=\(outcome.abandoned)"
                    )
                    guard outcome.abandoned == 0 else {
                        emit("agent: termination-cancelled")
                        return
                    }
                    emit("agent: state=stopped")
                    exit(0)
                }
            }
            source.resume()
            retainedSources.append(source)
        }
    }

    /// Signal sources must outlive main()'s scope; dispatchMain() never
    /// returns, so a static retain list is the containing scope.
    private nonisolated(unsafe) static var retainedSources: [any DispatchSourceProtocol] = []

    /// Coordinates the one process exit following a control acknowledgement.
    /// It deliberately starts only after the server writes `commandDone`, so the
    /// companion never mistakes a disappearing socket for accepted work.
    private final class TerminationExitGate: @unchecked Sendable {
        private let lock = NSLock()
        private let watchdog: CommitExitWatchdog
        private let testCommittedExitDelay: Duration?
        private let hardExitWatchdogDelay: Duration
        private weak var lifecycle: AgentLifecycle?
        private var requested = false

        init(
            watchdog: CommitExitWatchdog,
            testCommittedExitDelay: Duration? = nil,
            hardExitWatchdogDelay: Duration = CommitExitWatchdog.committedExitDeadline
        ) {
            self.watchdog = watchdog
            self.testCommittedExitDelay = testCommittedExitDelay
            self.hardExitWatchdogDelay = hardExitWatchdogDelay
        }

        func bind(_ lifecycle: AgentLifecycle) {
            lock.lock()
            self.lifecycle = lifecycle
            lock.unlock()
        }

        func request(_ request: ControlTerminationRequest) {
            if request.action == .cancel {
                lock.lock()
                let lifecycle = self.lifecycle
                lock.unlock()
                lifecycle?.cancelTermination(request)
                return
            }
            guard request.action == .prepare || request.action == .cancel else { return }
            lock.lock()
            guard
                let lifecycle,
                !requested || lifecycle.currentState == .terminationCancelled
            else {
                lock.unlock()
                return
            }
            requested = true
            lock.unlock()
            lifecycle.beginTermination(request)
            Task {
                let outcome = await lifecycle.shutdown(
                    reason: request.reason == .update ? .update : .terminate
                )
                emit(
                    "agent: drained completed=\(outcome.completed) "
                        + "cancelled=\(outcome.cancelled) abandoned=\(outcome.abandoned)"
                )
                guard outcome.abandoned == 0, lifecycle.currentState != .terminationCancelled else {
                    emit("agent: termination-cancelled")
                    self.clearRequest()
                    return
                }
                // A completed drain is still reversible. The companion must observe
                // request-correlated readiness and explicitly commit before this
                // process tears down its endpoints or exits. The lifecycle lease
                // cancels and restores serving state if no commit arrives.
                guard lifecycle.currentState == .terminationReady else {
                    emit("agent: termination-cancelled")
                    self.clearRequest()
                    return
                }
                emit("agent: termination-ready")
            }
        }

        func acceptCommit(_ request: ControlTerminationRequest) -> Bool {
            lock.lock()
            let lifecycle = self.lifecycle
            lock.unlock()
            // The lifecycle holds its commit lock across this arm + claim, so a
            // cancellation cannot land between the two halves of the permit.
            return lifecycle?.acceptTerminationCommit(request, armWatchdog: {
                self.watchdog.arm(after: self.hardExitWatchdogDelay)
            }) ?? false
        }

        func finishAcceptedCommit(_ request: ControlTerminationRequest) {
            lock.lock()
            let lifecycle = self.lifecycle
            lock.unlock()
            guard let lifecycle, lifecycle.finishAcceptedTerminationCommit(request) else { return }
            // The watchdog stays armed. No async teardown is allowed after commit:
            // normal exit and watchdog fallback both rely on kernel cleanup.
            #if DEBUG
                if let testCommittedExitDelay {
                    let components = testCommittedExitDelay.components
                    let microseconds = max(
                        0,
                        components.seconds * 1_000_000
                            + Int64(components.attoseconds / 1_000_000_000_000)
                    )
                    _ = Darwin.usleep(useconds_t(min(microseconds, Int64(UInt32.max))))
                }
            #endif
            Darwin._exit(0)
        }

        private func clearRequest() {
            lock.lock()
            requested = false
            lock.unlock()
        }
    }

    // MARK: - health

    private static func fetchHealth(dataRoot: URL, options: [String: String]) {
        let layout = AgentRuntimeLayout(dataRoot: dataRoot)
        let timeoutMs = integerOption(options, "timeout-ms") ?? 5000
        do {
            let snapshot = try AgentHealthClient.fetch(
                socketURL: layout.healthSocket,
                timeout: .milliseconds(timeoutMs)
            )
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            let data = try encoder.encode(snapshot)
            emit(String(decoding: data, as: UTF8.self))
        } catch let AgentHealthClientError.agentUnavailable(path) {
            fail("agent unavailable at \(path)", code: 4)
        } catch {
            fail("health fetch failed: \(error)", code: 4)
        }
    }

    // MARK: - plumbing

    private static func parseOptions(_ arguments: [String]) throws -> [String: String] {
        var options: [String: String] = [:]
        var index = 0
        while index < arguments.count {
            let argument = arguments[index]
            guard argument.hasPrefix("--") else {
                throw OptionError.unexpected(argument)
            }
            let key = String(argument.dropFirst(2))
            guard index + 1 < arguments.count else {
                throw OptionError.missingValue(argument)
            }
            options[key] = arguments[index + 1]
            index += 2
        }
        return options
    }

    private static func resolveDataRoot(_ options: [String: String]) throws -> URL {
        if let explicit = options["data-root"] {
            return URL(fileURLWithPath: explicit, isDirectory: true)
        }
        if let container = options["container"] {
            return AppGroup.dataRootURL(
                containerURL: URL(fileURLWithPath: container, isDirectory: true)
            )
        }
        return try AppGroup.dataRootURL(containerURL: AppGroup.containerURL())
    }

    private static func integerOption(_ options: [String: String], _ key: String) -> Int? {
        options[key].flatMap(Int.init)
    }

    private static func boolOption(_ options: [String: String], _ key: String) -> Bool? {
        options[key].map { value in
            value == "1" || value.lowercased() == "true"
        }
    }

    private static func emit(_ line: String) {
        print(line)
        fflush(stdout)
    }

    private static func fail(_ message: String, code: Int32) -> Never {
        FileHandle.standardError.write(Data(("gramdrive-agent: " + message + "\n").utf8))
        exit(code)
    }

    private enum OptionError: Error, CustomStringConvertible {
        case unexpected(String)
        case missingValue(String)

        var description: String {
            switch self {
            case let .unexpected(argument): return "unexpected argument '\(argument)'"
            case let .missingValue(option): return "option '\(option)' needs a value"
            }
        }
    }
}

/// Progress sink for the hosted boundary probe; the probe's purpose here
/// is being drainable, not being watched.
private final class SilentProgressListener: ProgressListener {
    func onProgress(progress _: TransferProgress) {}
}

/// Breaks the seam/lifecycle construction cycle: the repair seam needs the
/// lifecycle's account projection, and the lifecycle needs the seams at
/// construction. Filled once, right after the lifecycle exists.
private final class LifecycleRef: @unchecked Sendable {
    weak var lifecycle: AgentLifecycle?
}
