#!/usr/bin/env python3
"""Agent-lifecycle smoke: the companion agent as real processes.

Owned by TASK-260715-1yx9ly. Proves the macOS background agent's lifecycle
contract end to end, in real separate processes, through the *packaged*
artifact (SwiftPM -> XCFramework -> staticlib):

  1. An agent process starts over a substitute container, hosting one
     synthetic in-flight transfer (the FFI boundary probe registered in the
     drain ledger), and its health endpoint answers over the bounded local
     IPC channel: state=running, the right pid, one pending transfer.
  2. A second agent process over the same container must exit with the
     single-instance code (2) while the first keeps serving — one
     coordinator per container.
  3. SIGTERM must drain cleanly: the hosted transfer is cancelled through
     its token, the drain outcome is reported, exit code is 0, and the
     socket is gone afterwards (health answers "unavailable").
  4. Crash recovery: an agent is SIGKILLed and a successor must start
     immediately (the kernel released the dead agent's flock) with healthy
     shared state — recovery leans on durable state, no stale-lock
     cleanup, no duplicate coordinator.

Requires: Xcode (swift), and a staged artifact at
.temp/packaging/GramDriveCore (`make package`; run automatically when
missing, or forced with --repackage).

Usage: python3 .scripts/smoke/run_agent_lifecycle_smoke.py [--repackage]
Artifacts and logs: .temp/agent-lifecycle-smoke/
"""

from __future__ import annotations

import argparse
import json
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT_DIR = REPO_ROOT / ".temp" / "agent-lifecycle-smoke"
CORE_PACKAGE = REPO_ROOT / ".temp" / "packaging" / "GramDriveCore"
SUPPORT_PACKAGE = REPO_ROOT / "apple" / "GramDriveSupport"
CONTAINER = OUT_DIR / "container"
READY_TIMEOUT = 30
EXIT_TIMEOUT = 20


def run(name: str, cmd: list[str], **kwargs) -> str:
    """Runs one step, teeing output to a log file; exits non-zero on failure."""
    log_path = OUT_DIR / f"{name}.log"
    print(f"--- {name}: {' '.join(str(c) for c in cmd)}")
    result = subprocess.run(
        cmd, cwd=REPO_ROOT, capture_output=True, text=True, **kwargs
    )
    log_path.write_text(result.stdout + result.stderr, encoding="utf-8")
    if result.returncode != 0:
        sys.stdout.write((result.stdout + result.stderr)[-4000:])
        print(f"FAILED: {name} (exit {result.returncode}); full log: {log_path}")
        sys.exit(result.returncode)
    return result.stdout


def fetch_health(agent_bin: Path) -> dict | None:
    """One health query; None while the agent is unavailable (exit 4)."""
    result = subprocess.run(
        [str(agent_bin), "health", "--container", str(CONTAINER),
         "--timeout-ms", "3000"],
        cwd=REPO_ROOT, capture_output=True, text=True,
    )
    if result.returncode == 4:
        return None
    if result.returncode != 0:
        print(f"FAILED: health query (exit {result.returncode})")
        sys.stdout.write(result.stdout + result.stderr)
        sys.exit(1)
    return json.loads(result.stdout)


def await_health(agent_bin: Path, predicate, what: str) -> dict:
    deadline = time.monotonic() + READY_TIMEOUT
    last = None
    while time.monotonic() < deadline:
        last = fetch_health(agent_bin)
        if last is not None and predicate(last):
            return last
        time.sleep(0.2)
    print(f"FAILED: agent never became '{what}'; last health: {last}")
    sys.exit(1)


def await_exit(name: str, process: subprocess.Popen, expected_code: int) -> str:
    try:
        stdout, stderr = process.communicate(timeout=EXIT_TIMEOUT)
    except subprocess.TimeoutExpired:
        process.kill()
        print(f"FAILED: {name} did not exit within {EXIT_TIMEOUT}s")
        sys.exit(1)
    (OUT_DIR / f"{name}.log").write_text(stdout + stderr, encoding="utf-8")
    if process.returncode != expected_code:
        sys.stdout.write(stdout + stderr)
        print(
            f"FAILED: {name} exited {process.returncode}, "
            f"expected {expected_code}"
        )
        sys.exit(1)
    return stdout


def start_agent(name: str, agent_bin: Path, extra: list[str]) -> subprocess.Popen:
    print(f"--- {name}: starting agent process")
    return subprocess.Popen(
        [str(agent_bin), "run", "--container", str(CONTAINER), *extra],
        cwd=REPO_ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )


def expect(condition: bool, what: str) -> None:
    if not condition:
        print(f"FAILED: {what}")
        sys.exit(1)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--repackage", action="store_true",
        help="re-stage the core artifact even if one is present",
    )
    args = parser.parse_args()

    shutil.rmtree(OUT_DIR, ignore_errors=True)
    OUT_DIR.mkdir(parents=True)
    CONTAINER.mkdir(parents=True)

    if args.repackage or not (CORE_PACKAGE / "Package.swift").exists():
        run("package", [sys.executable, ".scripts/packaging/build_core_artifacts.py"])

    run(
        "build-agent",
        ["swift", "build", "--package-path", str(SUPPORT_PACKAGE),
         "--product", "gramdrive-agent"],
    )
    bin_dir = run(
        "agent-bin-path",
        ["swift", "build", "--package-path", str(SUPPORT_PACKAGE),
         "--show-bin-path"],
    ).strip()
    agent_bin = Path(bin_dir) / "gramdrive-agent"
    expect(agent_bin.exists(), f"agent binary missing at {agent_bin}")

    # 1. First agent up, hosting one synthetic in-flight transfer; health
    # answers over the bounded IPC channel with the right identity.
    first = start_agent("agent-first", agent_bin, [
        "--probe-transfer-ms", "600000", "--drain-grace-ms", "200",
    ])
    health = await_health(
        agent_bin,
        lambda h: h["state"] == "running" and h["pendingTransferCount"] == 1,
        "running with one pending transfer",
    )
    expect(health["pid"] == first.pid, "health pid must match the agent process")
    expect("started" in health["recentEvents"], "health must record startup")
    expect(health["stateSchemaVersion"] > 0, "shared state must be open and healthy")
    print(f"--- health: running, pid={health['pid']}, pending=1")

    # 2. Single instance: a second agent over the same container exits 2
    # and the first keeps serving.
    second = start_agent("agent-second", agent_bin, [])
    await_exit("agent-second", second, 2)
    health = fetch_health(agent_bin)
    expect(
        health is not None and health["pid"] == first.pid,
        "first agent must survive the refused second instance",
    )
    print("--- single-instance: second agent refused (exit 2), first serving")

    # 2b. SIGPIPE immunity (BUG-260720-3i74u1): sockets inside libtdjson
    # carry the process's SIGPIPE disposition, so the agent must ignore it
    # process-wide — a dead peer mid-write must never kill the agent.
    first.send_signal(signal.SIGPIPE)
    time.sleep(0.5)
    expect(first.poll() is None, "the agent must survive SIGPIPE")
    health = fetch_health(agent_bin)
    expect(
        health is not None and health["pid"] == first.pid,
        "health must still answer after SIGPIPE",
    )
    print("--- sigpipe: ignored, agent alive and serving")

    # 3. Clean shutdown: SIGTERM drains the hosted transfer through its
    # cancellation token and exits 0; the endpoint is gone afterwards.
    first.send_signal(signal.SIGTERM)
    stdout = await_exit("agent-first", first, 0)
    expect("probe-transfer: cancelled" in stdout, "drain must cancel the hosted transfer")
    expect(
        "drained completed=0 cancelled=1 abandoned=0" in stdout,
        f"unexpected drain outcome in:\n{stdout}",
    )
    expect("agent: state=stopped" in stdout, "agent must report the stopped state")
    expect(fetch_health(agent_bin) is None, "health must be unavailable after shutdown")
    socket_path = (
        CONTAINER / "Library" / "Application Support" / "GramDrive"
        / "agent" / "health.sock"
    )
    expect(not socket_path.exists(), "the socket file must be removed on shutdown")
    print("--- clean shutdown: drained (cancelled=1), exit 0, endpoint gone")

    # 4. Crash recovery: SIGKILL an agent, then a successor must start
    # immediately over the same container with healthy durable state.
    third = start_agent("agent-third", agent_bin, [])
    await_health(agent_bin, lambda h: h["state"] == "running", "running (third)")
    third.send_signal(signal.SIGKILL)
    third.wait(timeout=EXIT_TIMEOUT)
    fourth = start_agent("agent-fourth", agent_bin, [])
    health = await_health(
        agent_bin,
        lambda h: h["state"] == "running" and h["pid"] == fourth.pid,
        "running (successor after SIGKILL)",
    )
    expect(
        health["stateSchemaVersion"] > 0,
        "durable state must be healthy after the crash",
    )
    fourth.send_signal(signal.SIGTERM)
    await_exit("agent-fourth", fourth, 0)
    print("--- crash recovery: successor took over instantly, state healthy")

    print("PASSED: agent lifecycle smoke")


if __name__ == "__main__":
    main()
