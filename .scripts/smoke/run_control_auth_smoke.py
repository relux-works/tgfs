#!/usr/bin/env python3
"""Control-channel auth smoke: real sign-in against Telegram's test DC.

Owned by BUG-260720-3i74u1. Proves the acceptance criteria's end-to-end
path through the *packaged* agent binary (by default the one inside the
assembled GramDrive.app, so the proven bytes are the shipped bytes):

  1. The agent starts over a fresh substitute container with the test-DC
     flag; its control socket comes up.
  2. An auth session is opened over the control channel (the same NDJSON
     wire the companion speaks) and driven end to end: phone number ->
     login code -> ready. Telegram's test DC issues deterministic codes
     for its shared test numbers (+99966XYYYY confirms with X repeated
     five times); numbers that turn out unregistered (registration is
     outside the v1 sign-in scope, DEC/POL) or throttled are skipped and
     the next suffix is tried.
  3. After `ready`, the one-shot `status` command must report the account
     row (identity + display name + authorized) — the durable half.
  4. The agent is SIGTERMed and a fresh agent started over the same
     container: `status` must still report the account, and the `repair`
     command must complete — repair probes the stored session by opening
     the account's client and observing TDLib reach Ready with no user
     input, which is exactly "the session persists across restart".

Requires: the packaged app bundle (`make package-app` or
`--agent <path>` for a bare binary), Telegram api credentials in the
keychain (service `gramdrive-telegram`) *readable by the packaged agent
without a consent prompt* — provision them with
.scripts/keychain/provision_telegram_credentials.py, since items created
by the `security` CLI are partition-locked and hang the unattended agent
on an interactive prompt — and network access to Telegram's test data
centers. Never run against a production DC: the flag stays on
`--telegram-test-dc true` unconditionally.

Since mid-2025 Telegram no longer honors the documented auto-code for
the shared 99966XYYYY numbers with third-party api ids (tdlib/td#3361:
"The test phone numbers don't work anymore for regular users" — the
account must exist, created via an official app, and the code is
delivered for real). The default pattern mode therefore cannot complete
unattended; `--phone` is the supported path: a human's dedicated test-DC
account, the login code read from stdin (it arrives in the official app
session that created the account). The signed-in session persists in the
kept container, so the restart/repair legs — and future re-runs over
`--container` — stay unattended after that one interactive bootstrap.

Usage: python3 .scripts/smoke/run_control_auth_smoke.py
           [--agent PATH] [--dc N] [--attempts N] [--keep]
           [--phone +NUMBER]   # interactive: code prompted on stdin
Artifacts and logs: .temp/control-auth-smoke/
"""

from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
OUT_DIR = REPO_ROOT / ".temp" / "control-auth-smoke"
DEFAULT_AGENT = (
    REPO_ROOT / ".temp" / "app-packaging" / "GramDrive.app" / "Contents" / "MacOS"
    / "gramdrive-agent"
)
CONTAINER = OUT_DIR / "container"
DATA_ROOT = CONTAINER / "Library" / "Application Support" / "GramDrive"
CONTROL_SOCKET = DATA_ROOT / "agent" / "control.sock"
READY_TIMEOUT = 30
EVENT_TIMEOUT = 90


def log(message: str) -> None:
    print(message, flush=True)


def mask_phone(number: str) -> str:
    """A log-safe phone number: keep the leading country/DC digits so runs
    stay correlatable, hide the subscriber tail (the operator's --phone is a
    real account)."""
    keep = 4
    if len(number) <= keep + 2:
        return "*" * len(number)
    return number[:keep] + "*" * (len(number) - keep - 2) + number[-2:]


def mask_account(account: dict) -> dict:
    """A log-safe account row: the identity truncated, the display name
    dropped. Enough to correlate a run, not enough to expose the operator."""
    if not isinstance(account, dict):
        return account
    masked = dict(account)
    account_id = masked.get("accountId")
    if account_id is not None:
        text = str(account_id)
        masked["accountId"] = ("…" + text[-3:]) if len(text) > 3 else "***"
    if "displayName" in masked:
        masked["displayName"] = "<redacted>"
    return masked


class ControlConnection:
    """One NDJSON control connection (the wire the companion speaks)."""

    def __init__(self, path: Path, timeout: float = EVENT_TIMEOUT):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(timeout)
        # The container path exceeds sun_path; connect the leaf from the
        # socket's own directory (the same technique the Swift side uses).
        target = str(path)
        if len(target.encode()) > 100:
            previous = os.getcwd()
            os.chdir(path.parent)
            try:
                self.sock.connect(path.name)
            finally:
                os.chdir(previous)
        else:
            self.sock.connect(target)
        self.buffer = b""

    def send(self, obj: dict) -> None:
        self.sock.sendall(json.dumps(obj).encode() + b"\n")

    def next_event(self) -> dict | None:
        while b"\n" not in self.buffer:
            chunk = self.sock.recv(65536)
            if not chunk:
                return None
            self.buffer += chunk
        line, self.buffer = self.buffer.split(b"\n", 1)
        return json.loads(line)

    def close(self) -> None:
        try:
            self.sock.close()
        except OSError:
            pass


def command(operation: str, extra: dict | None = None, timeout: float = 150.0) -> dict:
    """One-shot control command; returns the terminal event."""
    conn = ControlConnection(CONTROL_SOCKET, timeout=timeout)
    try:
        request = {"protocolVersion": 1, "operation": operation}
        if extra:
            request.update(extra)
        conn.send(request)
        event = conn.next_event()
        if event is None:
            raise RuntimeError(f"{operation}: connection closed without an answer")
        return event
    finally:
        conn.close()


def start_agent(agent: Path, log_name: str) -> subprocess.Popen:
    log_file = (OUT_DIR / f"{log_name}.log").open("w")
    process = subprocess.Popen(
        [
            str(agent),
            "run",
            "--container",
            str(CONTAINER),
            "--telegram-test-dc",
            "true",
        ],
        stdout=log_file,
        stderr=subprocess.STDOUT,
    )
    deadline = time.monotonic() + READY_TIMEOUT
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(
                f"agent exited early with {process.returncode}; see {log_name}.log"
            )
        if CONTROL_SOCKET.exists():
            try:
                probe = ControlConnection(CONTROL_SOCKET, timeout=5)
                probe.close()
                log(f"--- agent up (pid {process.pid})")
                return process
            except OSError:
                pass
        time.sleep(0.25)
    raise RuntimeError("the agent's control socket never came up")


def stop_agent(process: subprocess.Popen) -> None:
    if process.poll() is None:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=10)
    log(f"--- agent stopped (exit {process.returncode})")


def drive_sign_in(dc: int, attempts: int, phone: str | None) -> dict:
    """Drives phone -> code -> ready. Returns the `ready` state's account
    identity.

    With `phone`, one interactive attempt: the operator's dedicated
    test-DC account, codes typed on stdin. Otherwise the historical
    pattern mode: random shared test numbers with derived codes, until
    one lands. Numbers that reach a state outside the v1 scope
    (registration), or whose submission is rejected, are cancelled and
    the next suffix tried.
    """
    if phone:
        log(f"--- interactive sign-in: {mask_phone(phone)}")
        outcome = drive_one_number(phone, operator_codes)
        if isinstance(outcome, dict):
            return outcome
        raise RuntimeError(f"interactive sign-in did not complete: {outcome}")
    suffixes = random.sample(range(10_000), attempts)
    last_detail = "no attempt ran"
    for attempt, suffix in enumerate(suffixes, start=1):
        number = f"+99966{dc}{suffix:04d}"
        log(f"--- sign-in attempt {attempt}/{attempts}: {mask_phone(number)}")
        outcome = drive_one_number(number, lambda info: pattern_codes(dc, info))
        if isinstance(outcome, dict):
            return outcome
        last_detail = outcome
        log(f"    {outcome}; trying the next test number")
        time.sleep(2)
    raise RuntimeError(
        "no test number completed sign-in (Telegram no longer honors the "
        f"shared-test-number auto-code for regular api ids — see --phone): {last_detail}"
    )


def pattern_codes(dc: int, code_info: dict) -> list[str]:
    """The historical auto-codes: the DC digit repeated. The reported
    length first, then the known lengths (tdlib/td#1524)."""
    candidates: list[str] = []
    for length in (code_info.get("codeLength"), 5, 6):
        if isinstance(length, int) and 0 < length and length not in (
            len(c) for c in candidates
        ):
            candidates.append(str(dc) * length)
    return candidates


def operator_codes(code_info: dict):
    """Interactive mode: the operator reads the code from the official
    app session that owns the account and types it in (3 tries; an empty
    line gives up)."""
    stated = code_info.get("codeLength")
    hint = f" ({stated} digits)" if stated else ""
    for _ in range(3):
        code = input(f"login code{hint}: ").strip()
        if not code:
            return
        yield code


def drive_one_number(number: str, codes_for) -> dict | str:
    """One sign-in attempt. Returns the account dict on ready, else a
    human-readable reason to try another number.

    `codes_for(code_info)` yields the candidate codes for the reached
    code prompt. A wrong code leaves TDLib in `wait-code`, so the next
    candidate goes over the same session."""
    conn = ControlConnection(CONTROL_SOCKET)
    seq = 0

    def submit(input_obj: dict) -> dict | str:
        nonlocal seq
        seq += 1
        conn.send({"seq": seq, "input": input_obj})
        while True:
            event = conn.next_event()
            if event is None:
                return "connection closed mid-flow"
            if event.get("event") == "auth-submit-result":
                result = event["result"]
                if result["seq"] != seq:
                    continue
                return result
            if event.get("event") == "auth-state":
                pending_states.append(event["state"])
            # A `failed` event during the submit wait ends the flow; return a
            # clean reason instead of blocking until the socket times out.
            if event.get("event") == "failed":
                return f"refused: {event.get('failure')}"

    def next_state() -> dict | str:
        while pending_states:
            return pending_states.pop(0)
        while True:
            event = conn.next_event()
            if event is None:
                return "connection closed mid-flow"
            if event.get("event") == "auth-state":
                return event["state"]
            if event.get("event") == "failed":
                return f"refused: {event.get('failure')}"

    pending_states: list[dict] = []
    try:
        conn.send({"protocolVersion": 1, "operation": "authStart"})
        # Walk to the phone prompt.
        while True:
            state = next_state()
            if isinstance(state, str):
                return state
            kind = state.get("kind")
            if kind == "wait-phone-number":
                break
            if kind in ("closed", "failed", "unsupported"):
                return f"flow ended before the phone prompt: {state}"
        result = submit({"kind": "submit-phone-number", "value": number})
        if isinstance(result, str) or result.get("outcome") != "accepted":
            return f"phone rejected: {result}"
        # Walk to the code prompt.
        while True:
            state = next_state()
            if isinstance(state, str):
                return state
            kind = state.get("kind")
            if kind == "wait-code":
                code_info = state.get("codeInfo") or {}
                break
            if kind in ("closed", "failed", "unsupported"):
                return f"flow ended before the code prompt: {state}"
        result: dict | str = "no code candidate ran"
        for code in codes_for(code_info):
            result = submit({"kind": "submit-code", "value": code})
            if isinstance(result, str):
                return f"code rejected: {result}"
            if result.get("outcome") == "accepted":
                break
            rejection = (result.get("rejection") or {}).get("kind")
            if rejection != "invalid-code":
                break
            log(f"    code {code} rejected as invalid; trying the next candidate")
        if isinstance(result, str) or result.get("outcome") != "accepted":
            return f"code rejected: {result}"
        # Walk to ready (through finalizing); registration-gated numbers
        # surface as unsupported here and are skipped.
        while True:
            state = next_state()
            if isinstance(state, str):
                return state
            kind = state.get("kind")
            if kind == "ready":
                account = state.get("account") or {}
                log(f"    ready: account {mask_account(account)}")
                return account
            if kind in ("unsupported", "closed", "failed"):
                return f"flow ended after the code: {state}"
    finally:
        conn.close()


def assert_account_visible(step: str) -> dict:
    event = command("status")
    if event.get("event") != "status":
        raise RuntimeError(f"{step}: status answered {event}")
    accounts = event["status"].get("accounts") or []
    if not accounts:
        raise RuntimeError(f"{step}: no account row in status")
    account = accounts[0]
    if account.get("authState") != "authorized":
        raise RuntimeError(f"{step}: account not authorized: {mask_account(account)}")
    log(f"--- {step}: account visible: {mask_account(account)}")
    return account


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agent", type=Path, default=DEFAULT_AGENT)
    parser.add_argument("--dc", type=int, default=2, choices=(1, 2, 3))
    parser.add_argument("--attempts", type=int, default=6)
    parser.add_argument("--keep", action="store_true")
    parser.add_argument(
        "--phone",
        help="dedicated test-DC account for one interactive sign-in "
        "(login code prompted on stdin); implies --keep so the "
        "authorized session survives for unattended re-runs",
    )
    args = parser.parse_args()
    if args.phone:
        args.keep = True

    if not args.agent.is_file():
        log(f"agent binary not found at {args.agent}; run `make package-app` first")
        return 2

    if OUT_DIR.exists():
        shutil.rmtree(OUT_DIR)
    OUT_DIR.mkdir(parents=True)
    CONTAINER.mkdir(parents=True)

    agent_process = start_agent(args.agent, "agent-first")
    try:
        account = drive_sign_in(args.dc, args.attempts, args.phone)
        if not account.get("accountId"):
            raise RuntimeError(f"ready reported no account identity: {account}")
        first_status = assert_account_visible("after sign-in")
        if first_status.get("accountId") != account.get("accountId"):
            raise RuntimeError(
                f"status reports {first_status}, sign-in reported {account}"
            )
    finally:
        stop_agent(agent_process)

    # Restart: the durable row and the TDLib session must both survive.
    agent_process = start_agent(args.agent, "agent-second")
    try:
        assert_account_visible("after restart")
        repair = command("repair")
        if repair.get("event") != "done":
            raise RuntimeError(
                "repair (the stored-session probe) did not complete after "
                f"restart: {repair}"
            )
        log("--- repair after restart: completed (stored session authorizes)")
    finally:
        stop_agent(agent_process)
        if not args.keep:
            # The container holds a live test-DC session; keep the default
            # run tidy. --keep retains it for inspection.
            shutil.rmtree(CONTAINER, ignore_errors=True)

    log("")
    log("control-auth smoke: PASS")
    log(f"  account: {mask_account(account)}")
    log(f"  logs:    {OUT_DIR}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
