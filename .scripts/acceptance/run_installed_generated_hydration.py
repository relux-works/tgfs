#!/usr/bin/env python3
"""Bounded, privacy-safe installed generated-document hydration acceptance.

The probe selects distinct dataless ``Messages.md``, ``Messages.ndjson`` and
``.chat.json`` placeholders backed by verified local cache entries. Each Finder
read runs in a killable child with a fixed deadline, so an operating-system
hydration stall is evidence rather than a hung acceptance process. Public
output contains aggregates only; identifiers, paths and digests stay in the
caller-owned private state file.
"""

from __future__ import annotations

import argparse
from dataclasses import asdict, dataclass
import hashlib
import json
import math
import multiprocessing
import os
from pathlib import Path
import socket
import sqlite3
import struct
import subprocess
import sys
import time
from collections.abc import Iterable, Sequence


DEFAULT_DATA_ROOT = (
    Path.home()
    / "Library/Group Containers/262RZ595FP.com.reluxworks.gramdrive"
    / "Library/Application Support/GramDrive"
)
DEFAULT_CLOUD_ROOT = Path.home() / "Library/CloudStorage/GramDrive-GramDrive"
MIME_TARGETS = {
    "text/markdown": 7,
    "application/x-ndjson": 7,
    "application/json": 6,
}
READ_TIMEOUT_SECONDS = 10.0
TURN_WAIT_SECONDS = 30.0


class AcceptanceFailure(RuntimeError):
    pass


@dataclass(frozen=True)
class Candidate:
    item_id: bytes
    account_id: int
    namespace_version: int
    chat_id: int
    mime_type: str
    logical_size: int
    content_version: str
    finder_path: Path
    cache_path: Path
    history_complete: bool
    backfill_active: bool


@dataclass(frozen=True)
class ReadResult:
    latency_ms: float
    byte_count: int
    digest: str | None
    errno: int | None
    timed_out: bool


def connect(database: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(f"file:{database}?mode=ro", uri=True, timeout=2)
    connection.row_factory = sqlite3.Row
    return connection


def finder_dataless(path: Path) -> bool:
    result = subprocess.run(
        ("stat", "-f", "%Sf", str(path)),
        check=True,
        capture_output=True,
        text=True,
        timeout=5,
    )
    return "dataless" in result.stdout.lower()


def item_path(db: sqlite3.Connection, cloud_root: Path, item_id: bytes) -> Path:
    names: list[str] = []
    current: bytes | None = item_id
    while current is not None:
        row = db.execute(
            "SELECT parent_item_id,safe_name,kind,deleted_at_ms FROM items WHERE item_id=?",
            (current,),
        ).fetchone()
        if row is None or row["deleted_at_ms"] is not None:
            raise AcceptanceFailure("generated-item-path-missing")
        current = row["parent_item_id"]
        if row["kind"] != "account":
            names.append(row["safe_name"])
    return cloud_root.joinpath(*reversed(names))


def chat_id_from_item(item_id: bytes) -> int:
    if len(item_id) < 8:
        raise AcceptanceFailure("chat-identity-shape-invalid")
    return struct.unpack(">q", item_id[-8:])[0]


def candidate_rows(
    db: sqlite3.Connection, mime_type: str, excluded: set[bytes]
) -> Iterable[sqlite3.Row]:
    rows = db.execute(
        """
        SELECT d.item_id,d.account_id,d.namespace_version,d.mime_type,
               d.logical_size,d.content_version,c.materialization_ref,
               CASE WHEN p.kind='chat' THEN p.item_id ELSE gp.item_id END AS chat_item_id
        FROM cache_entries c
        JOIN items d ON d.item_id=c.item_id
        JOIN items p ON p.item_id=d.parent_item_id
        LEFT JOIN items gp ON gp.item_id=p.parent_item_id
        WHERE c.kind='generated_doc' AND c.verification='verified'
          AND d.deleted_at_ms IS NULL AND d.mime_type=?
          AND c.materialization_ref IS NOT NULL
        ORDER BY d.item_id
        LIMIT 4096
        """,
        (mime_type,),
    )
    return (row for row in rows if row["item_id"] not in excluded)


def select_candidates(
    db: sqlite3.Connection,
    cloud_root: Path,
    targets: dict[str, int],
    excluded: set[bytes],
) -> list[Candidate]:
    selected: list[Candidate] = []
    for mime_type, target in targets.items():
        buckets: dict[bool, list[Candidate]] = {False: [], True: []}
        for row in candidate_rows(db, mime_type, excluded):
            chat_item = row["chat_item_id"]
            if chat_item is None:
                continue
            chat_id = chat_id_from_item(chat_item)
            sync = db.execute(
                """SELECT history_complete FROM chat_sync_state
                   WHERE account_id=? AND namespace_version=? AND chat_id=?""",
                (row["account_id"], row["namespace_version"], chat_id),
            ).fetchone()
            if sync is None:
                continue
            complete = bool(sync["history_complete"])
            progress = db.execute(
                """SELECT phase FROM chat_content_progress
                   WHERE account_id=? AND namespace_version=? AND chat_id=?""",
                (row["account_id"], row["namespace_version"], chat_id),
            ).fetchone()
            backfill_active = bool(
                progress is not None and progress["phase"] in ("pending", "syncing")
            )
            if not complete and not backfill_active:
                continue
            if len(buckets[complete]) >= target:
                continue
            path = item_path(db, cloud_root, row["item_id"])
            try:
                # Never call Path.stat/is_file on the provider placeholder in
                # this process: macOS may synchronously drive fetchContents
                # from metadata lookup. The subprocess below is the bounded
                # provider interaction and a missing path simply returns false.
                if not finder_dataless(path):
                    continue
            except (OSError, subprocess.SubprocessError):
                continue
            cache_path = Path(row["materialization_ref"])
            if not cache_path.is_file():
                continue
            buckets[complete].append(
                Candidate(
                    item_id=row["item_id"],
                    account_id=int(row["account_id"]),
                    namespace_version=int(row["namespace_version"]),
                    chat_id=chat_id,
                    mime_type=mime_type,
                    logical_size=int(row["logical_size"]),
                    content_version=row["content_version"],
                    finder_path=path,
                    cache_path=cache_path,
                    history_complete=complete,
                    backfill_active=backfill_active,
                )
            )
            if all(len(bucket) >= target for bucket in buckets.values()):
                break
        # Alternate states when possible, then fill the remaining format quota.
        combined: list[Candidate] = []
        for index in range(target):
            for complete in (False, True):
                if index < len(buckets[complete]):
                    combined.append(buckets[complete][index])
        selected.extend(combined[:target])
    if len(selected) != sum(targets.values()):
        raise AcceptanceFailure("insufficient-distinct-dataless-generated-documents")
    if not any(candidate.history_complete for candidate in selected):
        raise AcceptanceFailure("no-complete-chat-generated-document")
    if not any(not candidate.history_complete for candidate in selected):
        raise AcceptanceFailure("no-crawling-chat-generated-document")
    return selected


def digest_file(path: Path) -> tuple[str, int]:
    digest = hashlib.sha256()
    byte_count = 0
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
            byte_count += len(chunk)
    return digest.hexdigest(), byte_count


def _read_child(path: str, sender) -> None:
    try:
        digest, byte_count = digest_file(Path(path))
        sender.send((digest, byte_count, None))
    except OSError as error:
        sender.send((None, 0, error.errno))
    finally:
        sender.close()


def bounded_read(path: Path, timeout: float = READ_TIMEOUT_SECONDS) -> ReadResult:
    context = multiprocessing.get_context("fork")
    receiver, sender = context.Pipe(duplex=False)
    started = time.monotonic()
    process = context.Process(target=_read_child, args=(str(path), sender))
    process.start()
    sender.close()
    process.join(timeout)
    latency_ms = (time.monotonic() - started) * 1000
    if process.is_alive():
        process.terminate()
        process.join(2)
        if process.is_alive():
            process.kill()
            process.join(2)
        receiver.close()
        return ReadResult(latency_ms, 0, None, None, True)
    digest, byte_count, error_number = receiver.recv() if receiver.poll() else (None, 0, None)
    receiver.close()
    return ReadResult(latency_ms, byte_count, digest, error_number, False)


def percentile(values: Sequence[float], quantile: float) -> float:
    if not values:
        raise ValueError("percentile requires values")
    ordered = sorted(values)
    return ordered[max(0, math.ceil(quantile * len(ordered)) - 1)]


def control_socket(database: Path) -> Path:
    return database.parent.parent / "agent/control.sock"


def connect_unix(client: socket.socket, path: Path) -> None:
    if len(str(path).encode()) <= 100:
        client.connect(str(path))
        return
    previous = os.getcwd()
    os.chdir(path.parent)
    try:
        client.connect(path.name)
    finally:
        os.chdir(previous)


def hint_counts(path: Path) -> dict[str, int] | None:
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(2)
            connect_unix(client, path)
            client.sendall(b'{"protocolVersion":1,"operation":"status"}\n')
            line = b""
            while b"\n" not in line:
                chunk = client.recv(65536)
                if not chunk:
                    break
                line += chunk
        payload = json.loads(line.split(b"\n", 1)[0])
        counters = payload["status"]["historyPriorityHints"]
        return {
            key: int(counters[key])
            for key in ("requested", "visible", "background", "accepted", "unroutable")
        }
    except (OSError, ValueError, KeyError, TypeError):
        return None


def hint_delta(before: dict[str, int] | None, after: dict[str, int] | None) -> dict[str, int] | None:
    if before is None or after is None:
        return None
    return {key: after[key] - before[key] for key in before}


def cursor_snapshot(db: sqlite3.Connection) -> dict[str, list[int | None]]:
    return {
        f"{row['account_id']}:{row['namespace_version']}:{row['chat_id']}": [
            row["oldest_loaded_message_id"],
            row["newest_loaded_message_id"],
            int(row["history_complete"]),
            row["last_backfill_at_ms"],
        ]
        for row in db.execute(
            """SELECT account_id,namespace_version,chat_id,
                      oldest_loaded_message_id,newest_loaded_message_id,
                      history_complete,last_backfill_at_ms
               FROM chat_sync_state"""
        )
    }


def cursors_monotonic(before: dict[str, list], after: dict[str, list]) -> bool:
    for key, old in before.items():
        new = after.get(key)
        if new is None:
            return False
        old_oldest, old_newest, old_complete, old_turn = old
        new_oldest, new_newest, new_complete, new_turn = new
        if old_oldest is not None and new_oldest is not None and new_oldest > old_oldest:
            return False
        if old_newest is not None and new_newest is not None and new_newest < old_newest:
            return False
        if new_complete < old_complete:
            return False
        if old_turn is not None and new_turn is not None and new_turn < old_turn:
            return False
    return True


def selected_turns(db: sqlite3.Connection, candidates: Sequence[Candidate]) -> dict[str, int | None]:
    result: dict[str, int | None] = {}
    for candidate in candidates:
        if candidate.history_complete:
            continue
        key = f"{candidate.account_id}:{candidate.namespace_version}:{candidate.chat_id}"
        row = db.execute(
            """SELECT last_backfill_at_ms FROM chat_sync_state
               WHERE account_id=? AND namespace_version=? AND chat_id=?""",
            (candidate.account_id, candidate.namespace_version, candidate.chat_id),
        ).fetchone()
        result[key] = None if row is None else row[0]
    return result


def turn_advanced(before: dict[str, int | None], after: dict[str, int | None]) -> bool:
    return any(
        after.get(key) is not None and (value is None or after[key] > value)
        for key, value in before.items()
    )


def profile_identity(db: sqlite3.Connection) -> list[list[int]]:
    return [
        [int(row[0]), int(row[1])]
        for row in db.execute(
            "SELECT account_id,namespace_version FROM accounts ORDER BY account_id,namespace_version"
        )
    ]


def run(args: argparse.Namespace) -> tuple[dict, dict]:
    database = args.data_root / "state/gramdrive.sqlite3"
    db = connect(database)
    prior = json.loads(args.private.read_text()) if args.phase == "after" else None
    excluded = {
        bytes.fromhex(item) for item in (prior or {}).get("selected_item_ids", [])
    }
    baseline_cursors = cursor_snapshot(db)
    baseline_identity = profile_identity(db)
    active_backfill = int(
        db.execute(
            "SELECT count(*) FROM chat_content_progress WHERE phase IN ('pending','syncing')"
        ).fetchone()[0]
    )
    incomplete = int(
        db.execute("SELECT count(*) FROM chat_sync_state WHERE history_complete=0").fetchone()[0]
    )
    candidates = select_candidates(db, args.cloud_root, MIME_TARGETS, excluded)
    turns_before = selected_turns(db, candidates)
    db.close()

    before_hints = hint_counts(control_socket(database))
    records: list[dict] = []
    exact = True
    for candidate in candidates:
        expected_digest, expected_size = digest_file(candidate.cache_path)
        result = bounded_read(candidate.finder_path, args.read_timeout)
        exact_match = (
            not result.timed_out
            and result.errno is None
            and result.digest == expected_digest
            and result.byte_count == expected_size == candidate.logical_size
        )
        exact = exact and exact_match
        records.append(
            {**asdict(result), "mime_type": candidate.mime_type, "exact_match": exact_match}
        )
    after_hints = hint_counts(control_socket(database))

    deadline = time.monotonic() + args.turn_wait
    turns_after: dict[str, int | None] = {}
    while time.monotonic() < deadline:
        check = connect(database)
        turns_after = selected_turns(check, candidates)
        check.close()
        if turn_advanced(turns_before, turns_after):
            break
        time.sleep(0.25)

    current = connect(database)
    after_cursors = cursor_snapshot(current)
    current_identity = profile_identity(current)
    current.close()
    latencies = [record["latency_ms"] for record in records]
    delta = hint_delta(before_hints, after_hints)
    hint_balanced = bool(
        delta is not None
        and delta["requested"] == delta["background"]
        and delta["requested"] > 0
        and delta["unroutable"] == 0
    )
    p95 = percentile(latencies, 0.95)
    p99 = percentile(latencies, 0.99)
    public = {
        "phase": args.phase,
        "read_count": len(records),
        "format_counts": {
            mime: sum(record["mime_type"] == mime for record in records)
            for mime in MIME_TARGETS
        },
        "complete_chat_read_count": sum(candidate.history_complete for candidate in candidates),
        "active_backfill_chat_read_count": sum(
            candidate.backfill_active for candidate in candidates
        ),
        "timeout_count": sum(record["timed_out"] for record in records),
        "completed_read_count": sum(
            not record["timed_out"] and record["errno"] is None for record in records
        ),
        "exact_match_count": sum(record["exact_match"] for record in records),
        "errno_counts": {
            str(number): sum(record["errno"] == number for record in records)
            for number in sorted({record["errno"] for record in records if record["errno"] is not None})
        },
        "exact_bytes_verified": exact,
        "latency_p95_ms": round(p95, 1),
        "latency_p99_ms": round(p99, 1),
        "max_latency_ms": round(max(latencies), 1),
        "active_backfill_count": active_backfill,
        "incomplete_chat_count": incomplete,
        "backfill_saturated": active_backfill >= 2 and incomplete >= 2,
        "hint_counter_delta": delta,
        "hint_counters_balanced": hint_balanced,
        "target_chat_got_turn": turn_advanced(turns_before, turns_after),
        "selected_cursors_monotonic": cursors_monotonic(baseline_cursors, after_cursors),
        "profile_identity_preserved": baseline_identity == current_identity,
        "domain_root_preserved": args.cloud_root.is_dir(),
    }
    if prior is not None:
        public["relaunch_cursors_monotonic"] = cursors_monotonic(
            prior["all_cursors"], after_cursors
        )
        public["relaunch_profile_identity_preserved"] = (
            prior["profile_identity"] == current_identity
        )
        old_items = prior["selected_item_ids"]
        verify = connect(database)
        public["relaunch_prior_item_identity_preserved"] = all(
            verify.execute(
                "SELECT count(*) FROM items WHERE item_id=? AND deleted_at_ms IS NULL",
                (bytes.fromhex(item),),
            ).fetchone()[0]
            == 1
            for item in old_items
        )
        verify.close()
    passed = (
        public["read_count"] == sum(MIME_TARGETS.values())
        and public["timeout_count"] == 0
        and not public["errno_counts"]
        and exact
        and p95 < 1_000
        and p99 < 3_000
        and public["backfill_saturated"]
        and hint_balanced
        and public["target_chat_got_turn"]
        and public["selected_cursors_monotonic"]
        and public["profile_identity_preserved"]
        and public["domain_root_preserved"]
        and all(
            public.get(field, True)
            for field in (
                "relaunch_cursors_monotonic",
                "relaunch_profile_identity_preserved",
                "relaunch_prior_item_identity_preserved",
            )
        )
    )
    public["passed"] = passed
    private = {
        "selected_item_ids": sorted(
            excluded | {candidate.item_id for candidate in candidates}, key=bytes.hex
        ),
        "all_cursors": after_cursors,
        "profile_identity": current_identity,
        "reads": records,
    }
    private["selected_item_ids"] = [item.hex() for item in private["selected_item_ids"]]
    return public, private


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("before", "after"))
    parser.add_argument("--data-root", type=Path, default=DEFAULT_DATA_ROOT)
    parser.add_argument("--cloud-root", type=Path, default=DEFAULT_CLOUD_ROOT)
    parser.add_argument("--private", type=Path, required=True)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--read-timeout", type=float, default=READ_TIMEOUT_SECONDS)
    parser.add_argument("--turn-wait", type=float, default=TURN_WAIT_SECONDS)
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        public, private = run(args)
    except (AcceptanceFailure, OSError, sqlite3.Error, ValueError) as error:
        print(f"installed generated hydration acceptance failed: {error}", file=sys.stderr)
        return 1
    args.private.parent.mkdir(parents=True, exist_ok=True)
    args.private.write_text(json.dumps(private, sort_keys=True) + "\n")
    args.evidence.parent.mkdir(parents=True, exist_ok=True)
    args.evidence.write_text(json.dumps(public, indent=2, sort_keys=True) + "\n")
    print(json.dumps(public, sort_keys=True))
    return 0 if public["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
