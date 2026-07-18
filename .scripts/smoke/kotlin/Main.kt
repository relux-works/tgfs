// GramDrive bindings smoke consumer (Kotlin/JVM).
//
// Compiled against the generated gramdrive.kt by
// .scripts/smoke/run_bindings_smoke.py; proves the acceptance criteria of
// TASK-260715-265gqq executable-style: the contract surface compiles from
// Kotlin, suspend operations and progress callbacks work, structured errors
// round-trip as DriveException subclasses, and CancellationToken
// cancellation round-trips as DriveException.Cancelled with no callbacks
// afterwards. Also exercises the binding-level bonus: cancelling the
// coroutine itself drops the Rust future and stops progress.

import com.reluxworks.gramdrive.core.CancellationToken
import com.reluxworks.gramdrive.core.CoreConfig
import com.reluxworks.gramdrive.core.DriveCore
import com.reluxworks.gramdrive.core.DriveException
import com.reluxworks.gramdrive.core.ProgressListener
import com.reluxworks.gramdrive.core.TransferProgress
import com.reluxworks.gramdrive.core.contractVersion
import java.util.concurrent.CopyOnWriteArrayList
import kotlin.system.exitProcess
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking

var failures = 0

fun check(condition: Boolean, name: String) {
    if (condition) {
        println("ok - $name")
    } else {
        failures += 1
        println("FAIL - $name")
    }
}

class RecordingListener : ProgressListener {
    val events = CopyOnWriteArrayList<TransferProgress>()
    override fun onProgress(progress: TransferProgress) {
        events.add(progress)
    }
}

fun main(): Unit = runBlocking {
    // -- Contract version -------------------------------------------------
    val version = contractVersion()
    check(
        version.major == 0u && version.minor == 2u && version.patch == 0u,
        "contract version is 0.2.0",
    )

    // -- Constructor validation error round-trips --------------------------
    try {
        DriveCore(CoreConfig(dataDir = ""))
        check(false, "empty dataDir must throw")
    } catch (e: DriveException.InvalidArgument) {
        check(true, "constructor error round-trips as DriveException.InvalidArgument")
    }

    val core = DriveCore(CoreConfig(dataDir = System.getProperty("java.io.tmpdir")))

    // -- Suspend success path with progress callbacks -----------------------
    val listener = RecordingListener()
    val transferred = core.probeTransfer(
        totalBytes = 100u,
        chunkBytes = 40u,
        chunkDelayMs = 10u,
        listener = listener,
        token = CancellationToken(),
    )
    check(transferred == 100uL, "probe returns total bytes")
    check(
        listener.events.map { it.bytesTransferred } == listOf(40uL, 80uL, 100uL),
        "progress reports each chunk",
    )
    check(
        listener.events.all { it.bytesTotal == 100uL },
        "progress carries the known total",
    )

    // -- Suspend structured error round-trips -------------------------------
    try {
        core.probeTransfer(100u, 0u, 1u, RecordingListener(), CancellationToken())
        check(false, "zero chunkBytes must throw")
    } catch (e: DriveException.InvalidArgument) {
        check(true, "suspend error round-trips as DriveException.InvalidArgument")
    }

    // -- Cancellation round-trips --------------------------------------------
    // Without cancellation this probe would take ~28 hours (1M chunks x
    // 100 ms), so the test completing at all proves the token interrupted it.
    val cancelListener = RecordingListener()
    val token = CancellationToken()
    // runCatching inside the async block: a failed `async` child cancels its
    // parent scope even when await() is caught, so the expected failure must
    // be contained where it happens.
    val probe = async(Dispatchers.Default) {
        runCatching { core.probeTransfer(1_000_000u, 1u, 100u, cancelListener, token) }
    }
    delay(350)
    token.cancel()
    check(token.isCancelled(), "token reports cancelled state")
    check(
        probe.await().exceptionOrNull() is DriveException.Cancelled,
        "cancellation round-trips as DriveException.Cancelled",
    )
    val seenAtCancel = cancelListener.events.size
    check(seenAtCancel < 20, "cancellation interrupted the probe early")
    delay(500)
    check(
        cancelListener.events.size == seenAtCancel,
        "no progress callbacks after cancellation",
    )

    // -- Binding-level bonus: coroutine cancellation drops the Rust future ---
    val dropListener = RecordingListener()
    val job = launch(Dispatchers.Default) {
        core.probeTransfer(1_000_000u, 1u, 100u, dropListener, CancellationToken())
    }
    delay(350)
    job.cancelAndJoin()
    val seenAtDrop = dropListener.events.size
    delay(500)
    check(
        dropListener.events.size == seenAtDrop,
        "no progress callbacks after coroutine cancellation",
    )

    println(if (failures == 0) "KOTLIN SMOKE PASSED" else "KOTLIN SMOKE FAILED ($failures)")
    if (failures != 0) exitProcess(1)
}
