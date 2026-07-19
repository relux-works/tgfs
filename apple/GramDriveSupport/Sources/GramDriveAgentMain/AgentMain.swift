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
        let configuration = AgentConfiguration(
            dataRoot: dataRoot,
            drainGracePeriod: .milliseconds(integerOption(options, "drain-grace-ms") ?? 10_000),
            drainCancelWait: .milliseconds(
                integerOption(options, "drain-cancel-wait-ms") ?? 5_000),
            powerEvents: WorkspacePowerEventSource())
        let lifecycle = AgentLifecycle(configuration: configuration)
        do {
            try lifecycle.start()
        } catch AgentStartError.alreadyRunning {
            fail("another agent already coordinates this container", code: 2)
        } catch {
            fail("startup failed: \(error)", code: 3)
        }

        emit(
            "agent: state=running pid=\(ProcessInfo.processInfo.processIdentifier) "
                + "socket=\(lifecycle.runtimeLayout.healthSocket.path)")

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
                    token: token)
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
                            + "cancelled=\(outcome.cancelled) abandoned=\(outcome.abandoned)")
                    emit("agent: state=stopped")
                    exit(0)
                }
            }
            source.resume()
            retainedSources.append(source)
        }
    }

    // Signal sources must outlive main()'s scope; dispatchMain() never
    // returns, so a static retain list is the containing scope.
    nonisolated(unsafe) private static var retainedSources: [any DispatchSourceProtocol] = []

    // MARK: - health

    private static func fetchHealth(dataRoot: URL, options: [String: String]) {
        let layout = AgentRuntimeLayout(dataRoot: dataRoot)
        let timeoutMs = integerOption(options, "timeout-ms") ?? 5_000
        do {
            let snapshot = try AgentHealthClient.fetch(
                socketURL: layout.healthSocket,
                timeout: .milliseconds(timeoutMs))
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            let data = try encoder.encode(snapshot)
            emit(String(decoding: data, as: UTF8.self))
        } catch AgentHealthClientError.agentUnavailable(let path) {
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
                containerURL: URL(fileURLWithPath: container, isDirectory: true))
        }
        return AppGroup.dataRootURL(containerURL: try AppGroup.containerURL())
    }

    private static func integerOption(_ options: [String: String], _ key: String) -> Int? {
        options[key].flatMap(Int.init)
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
            case .unexpected(let argument): return "unexpected argument '\(argument)'"
            case .missingValue(let option): return "option '\(option)' needs a value"
            }
        }
    }
}

/// Progress sink for the hosted boundary probe; the probe's purpose here
/// is being drainable, not being watched.
private final class SilentProgressListener: ProgressListener {
    func onProgress(progress: TransferProgress) {}
}
