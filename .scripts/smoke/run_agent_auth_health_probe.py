#!/usr/bin/env python3
"""Native process-boundary probe for live authorization health.

Builds and starts the real ``gramdrive-agent`` executable, seeds one durable
authorized account, and verifies authorized, authorizationRequired, and
unavailable observations through the public health CLI/socket boundary. The
fixture namespace bridge is compiled only in DEBUG; release packaging always
uses the real CoreNamespaceBootstrapper.
"""

from __future__ import annotations

import json
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT_DIR = REPO_ROOT / ".temp" / "agent-auth-health-probe"
CORE_PACKAGE = REPO_ROOT / ".temp" / "packaging" / "GramDriveCore"
SUPPORT_PACKAGE = REPO_ROOT / "apple" / "GramDriveSupport"
CONTAINER = OUT_DIR / "container"
DATA_ROOT = CONTAINER / "Library" / "Application Support" / "GramDrive"
DATABASE = DATA_ROOT / "state" / "gramdrive.sqlite3"
READY_TIMEOUT_SECONDS = 10


def run_checked(argv: list[str]) -> str:
    result = subprocess.run(
        argv, cwd=REPO_ROOT, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
        raise SystemExit(result.returncode)
    return result.stdout


def start_agent(agent: Path, state: str | None = None) -> subprocess.Popen[str]:
    argv = [str(agent), "run", "--container", str(CONTAINER)]
    if state is not None:
        argv.extend(["--test-observed-authorization", state])
    return subprocess.Popen(
        argv,
        cwd=REPO_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )


def fetch_health(agent: Path) -> dict | None:
    result = subprocess.run(
        [
            str(agent),
            "health",
            "--container",
            str(CONTAINER),
            "--timeout-ms",
            "500",
        ],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode == 4:
        return None
    if result.returncode != 0:
        sys.stdout.write(result.stdout)
        sys.stderr.write(result.stderr)
        raise SystemExit(result.returncode)
    return json.loads(result.stdout)


def await_health(agent: Path, state: str | None = None) -> dict:
    deadline = time.monotonic() + READY_TIMEOUT_SECONDS
    last: dict | None = None
    while time.monotonic() < deadline:
        last = fetch_health(agent)
        if last is not None and last.get("state") == "running":
            if state is None:
                return last
            accounts = last.get("accounts") or []
            if accounts and accounts[0].get("observedAuthorization") == state:
                return last
        time.sleep(0.05)
    raise RuntimeError(f"agent health never published {state!r}; last={last!r}")


def stop_agent(process: subprocess.Popen[str]) -> None:
    process.send_signal(signal.SIGTERM)
    stdout, stderr = process.communicate(timeout=READY_TIMEOUT_SECONDS)
    if process.returncode != 0:
        sys.stdout.write(stdout)
        sys.stderr.write(stderr)
        raise RuntimeError(f"agent exited {process.returncode}, expected 0")


def seed_durable_account() -> None:
    with sqlite3.connect(DATABASE) as database:
        database.execute(
            """
            INSERT INTO accounts (
                account_id, source_kind, display_name, auth_state,
                namespace_version, retention_mode, archive_mode,
                created_at_ms, updated_at_ms, display_timezone
            ) VALUES (?, 'local_tdlib', 'Private', 'authorized', 0,
                      'mirror', 0, 1, 1, 'UTC')
            """,
            (9,),
        )


def durable_auth_state() -> str:
    with sqlite3.connect(DATABASE) as database:
        row = database.execute(
            "SELECT auth_state FROM accounts WHERE account_id = ?", (9,)
        ).fetchone()
    if row is None:
        raise RuntimeError("durable account disappeared")
    return str(row[0])


def main() -> None:
    shutil.rmtree(OUT_DIR, ignore_errors=True)
    OUT_DIR.mkdir(parents=True)
    CONTAINER.mkdir(parents=True)

    if not (CORE_PACKAGE / "Package.swift").exists():
        run_checked(
            [sys.executable, ".scripts/packaging/build_core_artifacts.py"]
        )
    run_checked(
        [
            "swift",
            "build",
            "--package-path",
            str(SUPPORT_PACKAGE),
            "--product",
            "gramdrive-agent",
        ]
    )
    bin_dir = Path(
        run_checked(
            ["swift", "build", "--package-path", str(SUPPORT_PACKAGE), "--show-bin-path"]
        ).strip()
    )
    agent = bin_dir / "gramdrive-agent"
    if not agent.is_file():
        raise RuntimeError(f"agent executable missing at {agent}")

    bootstrap = start_agent(agent)
    await_health(agent)
    stop_agent(bootstrap)
    seed_durable_account()

    observations: list[dict[str, object]] = []
    for state in ("authorized", "authorizationRequired", "unavailable"):
        process = start_agent(agent, state)
        started = time.monotonic()
        health = await_health(agent, state)
        elapsed_ms = round((time.monotonic() - started) * 1000)
        account = health["accounts"][0]
        if account["authState"] != "authorized":
            raise RuntimeError(f"durable health state changed for {state}: {account!r}")
        if durable_auth_state() != "authorized":
            raise RuntimeError(f"database auth_state changed for {state}")
        observations.append(
            {
                "observedAuthorization": state,
                "durableAuthState": account["authState"],
                "databaseAuthState": durable_auth_state(),
                "pid": health["pid"],
                "observedAfterMs": elapsed_ms,
            }
        )
        stop_agent(process)
        if fetch_health(agent) is not None:
            raise RuntimeError(f"health endpoint remained available after {state}")

    result = {
        "executable": str(agent),
        "transport": "gramdrive-agent health over bounded Unix socket IPC",
        "observations": observations,
    }
    result_path = OUT_DIR / "results.json"
    result_path.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result, indent=2))
    print(f"PASSED: native authorization health probe ({result_path})")


if __name__ == "__main__":
    main()
