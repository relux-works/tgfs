#!/usr/bin/env python3
"""Installed QA-only Open/Preview fault preservation and recovery matrix.

The runner is deliberately useless with an ordinary GramDrive build: only the
separately packaged QA agent recognizes its authenticated App-Group-local
record. It operates on dedicated synthetic image fixtures, never opens or logs
their bytes itself, and publishes only fault labels, counters, and booleans.
"""

from __future__ import annotations

import argparse
import base64
from dataclasses import dataclass
import hashlib
import hmac
import json
import os
from pathlib import Path
import secrets
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Sequence


DEFAULT_DATA_ROOT = (
    Path.home()
    / "Library/Group Containers/262RZ595FP.com.reluxworks.gramdrive"
    / "Library/Application Support/GramDrive"
)
DEFAULT_CLOUD_ROOT = Path.home() / "Library/CloudStorage/GramDrive-GramDrive"
DEFAULT_EVIDENCE = Path(".temp/BUG-260729-3uclm3_installed-fault-recovery.json")
CONTROL_SCHEMA = "gramdrive.qa-fault-control.v1"
CONTROL_RELATIVE_PATH = Path("qa/qa-fault-control-v1.json")
FAULTS = (
    "timeout",
    "transport",
    "renderer_source_not_found",
    "source_not_found",
    "unavailable_content",
)
PURPOSES = ("content", "thumbnail")
POLL_ATTEMPTS = 120
POLL_SECONDS = 0.25


class AcceptanceFailure(RuntimeError):
    """Fixed-label failure safe for public task evidence."""


@dataclass(frozen=True)
class Fixture:
    account_id: int
    item_id: bytes
    item_text: str
    parent_item_id: bytes
    content_version: str | None
    path: Path


@dataclass(frozen=True)
class Health:
    callbacks: int
    succeeded: int
    engine_failures: int
    provider_mappings: int
    no_such_item: int
    retryable: int


def read_secret(path: Path) -> bytes:
    facts = path.stat()
    if (
        not stat.S_ISREG(facts.st_mode)
        or facts.st_uid != os.getuid()
        or facts.st_mode & 0o777 != 0o600
    ):
        raise AcceptanceFailure("qa-secret-permissions-invalid")
    text = path.read_text(encoding="ascii").strip()
    try:
        secret = bytes.fromhex(text)
    except ValueError as error:
        raise AcceptanceFailure("qa-secret-format-invalid") from error
    if len(secret) != 32 or text != secret.hex():
        raise AcceptanceFailure("qa-secret-format-invalid")
    return secret


def item_text(item_id: bytes) -> str:
    return "gd" + base64.b32encode(item_id).decode("ascii").lower().rstrip("=")


def authenticated_fields(
    *,
    account_id: int,
    item_id: str,
    purpose: str,
    fault: str,
    nonce: str,
    expires_at_ms: int,
) -> dict:
    return {
        "account_id": account_id,
        "expires_at_ms": expires_at_ms,
        "fault": fault,
        "item_id": item_id,
        "nonce": nonce,
        "purpose": purpose,
        "schema": CONTROL_SCHEMA,
    }


def canonical_bytes(fields: dict) -> bytes:
    return json.dumps(
        fields, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def signed_record(fields: dict, secret: bytes) -> dict:
    return {
        **fields,
        "mac": hmac.new(secret, canonical_bytes(fields), hashlib.sha256).hexdigest(),
    }


def arm_fault(control: Path, record: dict) -> None:
    control.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(control.parent, 0o700)
    descriptor, temporary = tempfile.mkstemp(prefix=".qa-fault-", dir=control.parent)
    try:
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(record, output, sort_keys=True, separators=(",", ":"))
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, control)
    finally:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass


def clear_fault(control: Path) -> None:
    try:
        control.unlink()
    except FileNotFoundError:
        pass


def item_path(db: sqlite3.Connection, cloud_root: Path, item_id: bytes) -> Path:
    names: list[str] = []
    current: bytes | None = item_id
    while current is not None:
        row = db.execute(
            "SELECT parent_item_id, safe_name, kind FROM items WHERE item_id=?",
            (current,),
        ).fetchone()
        if row is None:
            raise AcceptanceFailure("fixture-ancestor-missing")
        parent, name, kind = row
        if kind != "account":
            names.append(name)
        current = parent
    return cloud_root.joinpath(*reversed(names))


def fixture(
    db: sqlite3.Connection,
    cloud_root: Path,
    *,
    fixture_prefix: str,
    purpose: str,
    fault: str,
) -> Fixture:
    safe_name = f"{fixture_prefix}-{purpose}-{fault}.png"
    row = db.execute(
        """
        SELECT item_id, account_id, parent_item_id, content_version
        FROM items
        WHERE safe_name=? AND kind='attachment' AND deleted_at_ms IS NULL
          AND availability='fetchable' AND mime_type LIKE 'image/%'
        """,
        (safe_name,),
    ).fetchone()
    if row is None:
        raise AcceptanceFailure("synthetic-fixture-missing")
    raw, account_id, parent, version = row
    return Fixture(
        account_id=account_id,
        item_id=raw,
        item_text=item_text(raw),
        parent_item_id=parent,
        content_version=version,
        path=item_path(db, cloud_root, raw),
    )


def live_identity(db: sqlite3.Connection, expected: Fixture) -> bool:
    row = db.execute(
        "SELECT parent_item_id, content_version, deleted_at_ms FROM items WHERE item_id=?",
        (expected.item_id,),
    ).fetchone()
    return row == (expected.parent_item_id, expected.content_version, None)


def finder_dataless(path: Path) -> bool:
    result = subprocess.run(
        ("stat", "-f", "%Sf", str(path)),
        check=False,
        capture_output=True,
        text=True,
        timeout=10,
    )
    return result.returncode == 0 and "dataless" in result.stdout.lower()


def health(db: sqlite3.Connection) -> Health:
    row = db.execute(
        """
        SELECT callback_count, success_count, engine_failure_count,
               provider_mapping_count, no_such_item_count, retryable_count
        FROM provider_fetch_health WHERE singleton=1
        """
    ).fetchone()
    if row is None:
        raise AcceptanceFailure("provider-health-unavailable")
    return Health(*row)


def wait_for_health(
    database: Path,
    predicate: Callable[[Health], bool],
    *,
    attempts: int = POLL_ATTEMPTS,
    delay: float = POLL_SECONDS,
) -> Health:
    for _ in range(attempts):
        with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as db:
            current = health(db)
        if predicate(current):
            return current
        time.sleep(delay)
    raise AcceptanceFailure("provider-callback-timeout")


def trigger_open(path: Path, _scratch: Path) -> int:
    return subprocess.run(
        ("open", "-g", str(path)),
        check=False,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=15,
    ).returncode


def trigger_preview(path: Path, scratch: Path) -> int:
    # Quick Look owns any content-derived thumbnail bytes. The private 0700
    # directory is destroyed immediately and is never attached as evidence.
    return subprocess.run(
        ("qlmanage", "-t", "-s", "256", "-o", str(scratch), str(path)),
        check=False,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=30,
    ).returncode


TRIGGERS: dict[str, Callable[[Path, Path], int]] = {
    "content": trigger_open,
    "thumbnail": trigger_preview,
}


def run_matrix(
    *,
    database: Path,
    data_root: Path,
    cloud_root: Path,
    secret: bytes,
    fixture_prefix: str,
    evidence_path: Path,
    trigger: dict[str, Callable[[Path, Path], int]] = TRIGGERS,
    dataless_probe: Callable[[Path], bool] = finder_dataless,
) -> dict:
    control = data_root / CONTROL_RELATIVE_PATH
    cases: list[dict] = []
    started = time.monotonic()
    clear_fault(control)
    try:
        for purpose in PURPOSES:
            for fault in FAULTS:
                with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as db:
                    selected = fixture(
                        db,
                        cloud_root,
                        fixture_prefix=fixture_prefix,
                        purpose=purpose,
                        fault=fault,
                    )
                    before = health(db)
                    identity_before = live_identity(db, selected) and selected.path.exists()
                if purpose == "content" and not dataless_probe(selected.path):
                    raise AcceptanceFailure("synthetic-content-fixture-not-dataless")
                fields = authenticated_fields(
                    account_id=selected.account_id,
                    item_id=selected.item_text,
                    purpose=purpose,
                    fault=fault,
                    nonce=secrets.token_hex(16),
                    expires_at_ms=int(time.time() * 1_000) + 5 * 60 * 1_000,
                )
                arm_fault(control, signed_record(fields, secret))
                with tempfile.TemporaryDirectory(prefix="gramdrive-qa-preview-") as temporary:
                    os.chmod(temporary, 0o700)
                    trigger[purpose](selected.path, Path(temporary))
                failed = wait_for_health(
                    database,
                    lambda value: value.callbacks > before.callbacks
                    and value.retryable > before.retryable,
                )
                with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as db:
                    preserved = live_identity(db, selected) and selected.path.exists()

                clear_fault(control)
                recovery_before = failed
                with tempfile.TemporaryDirectory(prefix="gramdrive-qa-preview-") as temporary:
                    os.chmod(temporary, 0o700)
                    trigger[purpose](selected.path, Path(temporary))
                recovered = wait_for_health(
                    database,
                    lambda value: value.callbacks > recovery_before.callbacks
                    and value.succeeded > recovery_before.succeeded,
                )
                with sqlite3.connect(f"file:{database}?mode=ro", uri=True) as db:
                    identity_after = live_identity(db, selected) and selected.path.exists()
                cases.append(
                    {
                        "purpose": purpose,
                        "fault": fault,
                        "retryable_typed_error_observed": (
                            failed.retryable > before.retryable
                            and failed.engine_failures > before.engine_failures
                            and failed.provider_mappings > before.provider_mappings
                        ),
                        "no_such_item_unchanged": (
                            failed.no_such_item == before.no_such_item
                        ),
                        "stable_identity_preserved": (
                            identity_before and preserved and identity_after
                        ),
                        "recovery_succeeded": recovered.succeeded > failed.succeeded,
                    }
                )
    finally:
        clear_fault(control)

    passed = all(
        case["retryable_typed_error_observed"]
        and case["no_such_item_unchanged"]
        and case["stable_identity_preserved"]
        and case["recovery_succeeded"]
        for case in cases
    ) and len(cases) == len(PURPOSES) * len(FAULTS)
    evidence = {
        "schema": "BUG-260729-3uclm3.installed-fault-recovery.v1",
        "privacy_safe": True,
        "qa_only": True,
        "case_count": len(cases),
        "passed": passed,
        "elapsed_ms": round((time.monotonic() - started) * 1_000),
        "cases": cases,
    }
    evidence_path.parent.mkdir(parents=True, exist_ok=True)
    evidence_path.write_text(json.dumps(evidence, indent=2, sort_keys=True) + "\n")
    return evidence


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--secret-file", type=Path, required=True)
    parser.add_argument("--data-root", type=Path, default=DEFAULT_DATA_ROOT)
    parser.add_argument("--cloud-root", type=Path, default=DEFAULT_CLOUD_ROOT)
    parser.add_argument("--fixture-prefix", default="gramdrive-qa-fault")
    parser.add_argument("--evidence", type=Path, default=DEFAULT_EVIDENCE)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    database = args.data_root / "state/gramdrive.sqlite3"
    try:
        evidence = run_matrix(
            database=database,
            data_root=args.data_root,
            cloud_root=args.cloud_root,
            secret=read_secret(args.secret_file),
            fixture_prefix=args.fixture_prefix,
            evidence_path=args.evidence,
        )
    except (AcceptanceFailure, OSError, sqlite3.Error, subprocess.SubprocessError) as error:
        label = str(error) if isinstance(error, AcceptanceFailure) else "acceptance-io-failed"
        print(f"installed fault recovery failed: {label}", file=sys.stderr)
        return 1
    print(json.dumps(evidence, sort_keys=True))
    return 0 if evidence["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
