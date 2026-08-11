#!/usr/bin/env python3
"""Privacy-safe installed acceptance for account-wide history convergence.

Chat identities and cursor bounds are written only to ``--state``. Public
evidence is a flat allow-list of aggregate numbers and booleans: it is safe to
attach to a task without exposing account, chat, path, title, or message data.
"""

from __future__ import annotations

import argparse
from collections import defaultdict
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import sqlite3
import struct
import subprocess
import sys
import time
from collections.abc import Sequence
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError


DEFAULT_DATA_ROOT = (
    Path.home()
    / "Library/Group Containers/262RZ595FP.com.reluxworks.gramdrive"
    / "Library/Application Support/GramDrive"
)
DEFAULT_CLOUD_ROOT = Path.home() / "Library/CloudStorage/GramDrive-GramDrive"
DEFAULT_STATE = Path(".temp/installed-history-convergence-private-state.json")
DEFAULT_EVIDENCE = Path(".temp/installed-history-convergence-evidence.json")
MIN_NEW_CHATS = 3
MAX_AGENT_CPU_PERCENT = 80.0
MAX_FINDER_ENUMERATION_MS = 5_000
CPU_OBSERVATION_SECONDS = 5.0

PUBLIC_FIELDS = {
    "agent_cpu_bounded",
    "agent_cpu_average_percent",
    "agent_cpu_max_percent",
    "anchored_cursor_count",
    "cursor_missing_count",
    "cursor_progress_preserved",
    "cursor_progressed_count",
    "cursor_regressed_count",
    "eligible_chat_count",
    "finder_enumeration_ms",
    "finder_enumeration_responsive",
    "full_coverage_chat_count",
    "full_coverage_chat_gain_count",
    "generated_finder_open_attempt_count",
    "generated_finder_open_chat_count",
    "generated_finder_open_stable",
    "gained_truthful_chat_count",
    "incomplete_cursor_count",
    "media_blob_count",
    "media_hydration_unchanged",
    "old_projected_chat_count",
    "phase",
    "prior_truthful_chats_preserved",
    "privacy_safe",
    "projected_chat_gain_count",
    "schema_version",
    "source_month_count",
    "source_history_chat_count",
    "source_precurrent_chat_count",
    "published_source_month_count",
    "terminal_cursor_count",
    "truthful_generated_chat_count",
    "visible_chat_appearance_count",
}
PUBLIC_PHASES = {"before", "after", "relaunch"}
PUBLIC_BOOLEAN_FIELDS = {
    "agent_cpu_bounded",
    "cursor_progress_preserved",
    "finder_enumeration_responsive",
    "generated_finder_open_stable",
    "media_hydration_unchanged",
    "prior_truthful_chats_preserved",
    "privacy_safe",
}
FINDER_SAMPLE_CHATS = 3
FINDER_SAMPLE_REPEATS = 2
FINDER_OPEN_TIMEOUT_SECONDS = 15


class AcceptanceFailure(RuntimeError):
    """A fixed-label failure that is safe to print."""


def connection(database: Path) -> sqlite3.Connection:
    return sqlite3.connect(f"file:{database}?mode=ro", uri=True)


def _private_key(account: int, namespace: int, chat: int) -> str:
    return f"{account}:{namespace}:{chat}"


def decode_month_appearance(item_id: bytes) -> tuple[str, int, int] | None:
    """Decode only the frozen v1 month-appearance prefix needed by this probe."""
    data = memoryview(item_id)
    if len(data) < 4 or data[0] != 1 or data[1] != 0x10:
        return None
    offset = 3
    if data[2] == 3:
        offset += 4
    elif data[2] not in (1, 2):
        return None
    if len(data) != offset + 1 + 8 + 4 + 8 + 2 + 1 or data[offset] != 0x0C:
        return None
    offset += 1
    account, namespace, chat, year, month = struct.unpack_from(">qIqHB", data, offset)
    if not 1 <= month <= 12:
        return None
    return _private_key(account, namespace, chat), year, month


def _month_bounds_ms(year: int, month: int, zone_name: str) -> tuple[int, int]:
    try:
        zone = ZoneInfo(zone_name)
    except ZoneInfoNotFoundError as error:
        raise AcceptanceFailure("display-timezone-unavailable") from error
    start = datetime(year, month, 1, tzinfo=zone)
    end = (
        datetime(year + 1, 1, 1, tzinfo=zone)
        if month == 12
        else datetime(year, month + 1, 1, tzinfo=zone)
    )
    return round(start.timestamp() * 1000), round(end.timestamp() * 1000)


def _current_month_start_ms() -> int:
    now = datetime.now(timezone.utc)
    return round(datetime(now.year, now.month, 1, tzinfo=timezone.utc).timestamp() * 1000)


def _eligible_keys(db: sqlite3.Connection) -> set[str]:
    return {
        _private_key(*row)
        for row in db.execute(
            """
            SELECT DISTINCT e.account_id, e.namespace_version, e.chat_id
            FROM chat_list_entries e
            JOIN accounts a ON a.account_id=e.account_id
            JOIN chats c
              ON c.account_id=e.account_id
             AND c.namespace_version=e.namespace_version
             AND c.chat_id=e.chat_id
            WHERE a.auth_state='authorized'
              AND a.namespace_version=e.namespace_version
              AND c.deleted_at_ms IS NULL
            """
        )
    }


def _source_chat_sets(
    db: sqlite3.Connection, eligible: set[str]
) -> tuple[set[str], set[str]]:
    cutoff = _current_month_start_ms()
    history: set[str] = set()
    old: set[str] = set()
    for account, namespace, chat, oldest in db.execute(
        """
        SELECT m.account_id, m.namespace_version, m.chat_id, min(m.sent_at_ms)
        FROM messages m
        JOIN accounts a ON a.account_id=m.account_id
        WHERE a.auth_state='authorized'
          AND a.namespace_version=m.namespace_version
        GROUP BY m.account_id, m.namespace_version, m.chat_id
        """
    ):
        key = _private_key(account, namespace, chat)
        if key in eligible:
            history.add(key)
            if oldest < cutoff:
                old.add(key)
    return history, old


def _source_months(
    db: sqlite3.Connection, eligible: set[str]
) -> dict[str, set[tuple[int, int]]]:
    zones = {
        (account, namespace): ZoneInfo(zone)
        for account, namespace, zone in db.execute(
            "SELECT account_id, namespace_version, display_timezone "
            "FROM accounts WHERE auth_state='authorized'"
        )
    }
    months: dict[str, set[tuple[int, int]]] = defaultdict(set)
    for account, namespace, chat, sent_at_ms in db.execute(
        """
        SELECT m.account_id, m.namespace_version, m.chat_id, m.sent_at_ms
        FROM messages m
        JOIN accounts a ON a.account_id=m.account_id
        WHERE a.auth_state='authorized'
          AND a.namespace_version=m.namespace_version
        """
    ):
        key = _private_key(account, namespace, chat)
        if key not in eligible:
            continue
        zone = zones[(account, namespace)]
        instant = datetime.fromtimestamp(sent_at_ms / 1_000, tz=timezone.utc)
        local = instant.astimezone(zone)
        months[key].add((local.year, local.month))
    return months


def _month_sets(
    db: sqlite3.Connection, eligible: set[str]
) -> tuple[set[str], set[str], set[tuple[str, int, int]]]:
    """Return old projections, truthful chats, and fully current source months."""
    zones = {
        (account, namespace): zone
        for account, namespace, zone in db.execute(
            "SELECT account_id, namespace_version, display_timezone "
            "FROM accounts WHERE auth_state='authorized'"
        )
    }
    now = datetime.now(timezone.utc)
    projected: set[str] = set()
    truthful: set[str] = set()
    rows = db.execute(
        """
        SELECT month.item_id, month.safe_name,
               sum(CASE WHEN doc.safe_name IN ('Messages.md', 'Messages.ndjson')
                             AND doc.logical_size > 0
                             AND cache.verification='verified'
                             AND cache.kind='generated_doc'
                             AND cache.size=doc.logical_size
                             AND cache.content_version=doc.content_version
                             AND render.dirty=0
                        THEN 1 ELSE 0 END)
        FROM items month
        LEFT JOIN items doc
          ON doc.parent_item_id=month.item_id
         AND doc.kind='generated_doc'
         AND doc.deleted_at_ms IS NULL
        LEFT JOIN cache_entries cache ON cache.item_id=doc.item_id
        LEFT JOIN render_state render ON render.item_id=doc.item_id
        WHERE month.kind='month_dir' AND month.deleted_at_ms IS NULL
        GROUP BY month.item_id, month.safe_name
        """
    )
    appearance_totals: dict[tuple[str, int, int], int] = defaultdict(int)
    appearance_ready: dict[tuple[str, int, int], int] = defaultdict(int)
    for item_id, safe_name, verified_docs in rows:
        decoded = decode_month_appearance(item_id)
        if decoded is None:
            continue
        key, year, month = decoded
        if (
            key not in eligible
            or safe_name != f"{year:04}-{month:02}"
            or (year, month) >= (now.year, now.month)
        ):
            continue
        projected.add(key)
        month_key = (key, year, month)
        appearance_totals[month_key] += 1
        appearance_ready[month_key] += verified_docs == 2
        if verified_docs != 2:
            continue
        account, namespace, chat = map(int, key.split(":"))
        zone_name = zones.get((account, namespace))
        if zone_name is None:
            continue
        start_ms, end_ms = _month_bounds_ms(year, month, zone_name)
        has_source = db.execute(
            """
            SELECT EXISTS (
                SELECT 1 FROM messages
                WHERE account_id=?1 AND namespace_version=?2 AND chat_id=?3
                  AND sent_at_ms>=?4 AND sent_at_ms<?5
            )
            """,
            (account, namespace, chat, start_ms, end_ms),
        ).fetchone()[0]
        if has_source:
            truthful.add(key)
    fully_published = {
        month
        for month, total in appearance_totals.items()
        if total > 0 and appearance_ready[month] == total
    }
    return projected, truthful, fully_published


def snapshot(db: sqlite3.Connection) -> dict:
    eligible = _eligible_keys(db)
    history, source_old = _source_chat_sets(db, eligible)
    source_months = _source_months(db, eligible)
    projected, truthful, fully_published = _month_sets(db, eligible)
    source_month_keys = {
        (key, year, month)
        for key, months in source_months.items()
        for year, month in months
    }
    published_source_months = fully_published & source_month_keys
    cursors: dict[str, list[int | None]] = {}
    terminal = anchored = incomplete = 0
    for account, namespace, chat, oldest, newest, complete in db.execute(
        """
        SELECT s.account_id, s.namespace_version, s.chat_id,
               s.oldest_loaded_message_id, s.newest_loaded_message_id,
               s.history_complete
        FROM chat_sync_state s
        JOIN accounts a ON a.account_id=s.account_id
        WHERE a.auth_state='authorized'
          AND a.namespace_version=s.namespace_version
        """
    ):
        key = _private_key(account, namespace, chat)
        cursors[key] = [oldest, newest, complete]
        if key in eligible:
            terminal += bool(complete)
            incomplete += not bool(complete)
            anchored += oldest is not None and newest is not None
    terminal_keys = {
        key for key, cursor in cursors.items() if key in eligible and bool(cursor[2])
    }
    full_coverage = {
        key
        for key in terminal_keys
        if source_months.get(key)
        and all(
            (key, year, month) in published_source_months
            for year, month in source_months[key]
        )
    }
    return {
        "eligible_keys": sorted(eligible),
        "source_history_keys": sorted(history),
        "source_precurrent_keys": sorted(source_old),
        "old_projected_keys": sorted(projected),
        "truthful_generated_keys": sorted(truthful),
        "source_month_keys": {
            key: sorted(months) for key, months in source_months.items()
        },
        "published_source_month_keys": sorted(published_source_months),
        "full_coverage_keys": sorted(full_coverage),
        "cursors": cursors,
        "terminal_cursor_count": terminal,
        "incomplete_cursor_count": incomplete,
        "anchored_cursor_count": anchored,
        "visible_chat_appearance_count": db.execute(
            """
            SELECT count(*) FROM items i JOIN accounts a ON a.account_id=i.account_id
            WHERE a.auth_state='authorized'
              AND a.namespace_version=i.namespace_version
              AND i.kind='chat' AND i.deleted_at_ms IS NULL
            """
        ).fetchone()[0],
        "media_blob_count": db.execute(
            "SELECT count(*) FROM cache_entries "
            "WHERE kind='blob' AND verification='verified'"
        ).fetchone()[0],
        "schema_version": db.execute("PRAGMA user_version").fetchone()[0],
    }


def _item_path(db: sqlite3.Connection, cloud_root: Path, item_id: bytes) -> Path:
    names: list[str] = []
    current: bytes | None = item_id
    while current is not None:
        row = db.execute(
            "SELECT parent_item_id, safe_name, kind FROM items WHERE item_id=?",
            (current,),
        ).fetchone()
        if row is None:
            raise AcceptanceFailure("projection-item-missing")
        current, name, kind = row
        if kind != "account":
            names.append(name)
    return cloud_root.joinpath(*reversed(names))


def verify_repeated_generated_finder_opens(
    db: sqlite3.Connection, cloud_root: Path, current: dict
) -> tuple[int, int]:
    """Open paired exports repeatedly in independent fully covered chats."""
    now = datetime.now(timezone.utc)
    candidates = [
        key
        for key in current["full_coverage_keys"]
        if any(
            (year, month) < (now.year, now.month)
            for year, month in current["source_month_keys"].get(key, [])
        )
    ][:FINDER_SAMPLE_CHATS]
    if len(candidates) < FINDER_SAMPLE_CHATS:
        raise AcceptanceFailure("full-coverage-chat-sample-insufficient")

    month_items: dict[tuple[str, int, int], list[bytes]] = defaultdict(list)
    for (item_id,) in db.execute(
        "SELECT item_id FROM items "
        "WHERE kind='month_dir' AND deleted_at_ms IS NULL ORDER BY item_id"
    ):
        decoded = decode_month_appearance(item_id)
        if decoded is not None:
            key, year, month = decoded
            if key in candidates:
                month_items[(key, year, month)].append(item_id)

    attempts = 0
    for key in candidates:
        months = current["source_month_keys"][key]
        selected = sorted({months[0], months[-1]})
        for year, month in selected:
            appearances = month_items.get((key, year, month), [])
            if not appearances:
                raise AcceptanceFailure("generated-finder-month-missing")
            month_item = appearances[0]
            docs = db.execute(
                """
                SELECT doc.item_id, cache.materialization_ref
                FROM items doc
                JOIN cache_entries cache ON cache.item_id=doc.item_id
                JOIN render_state render ON render.item_id=doc.item_id
                WHERE doc.parent_item_id=? AND doc.deleted_at_ms IS NULL
                  AND doc.safe_name IN ('Messages.md', 'Messages.ndjson')
                  AND cache.kind='generated_doc'
                  AND cache.verification='verified'
                  AND cache.content_version=doc.content_version
                  AND cache.size=doc.logical_size
                  AND cache.materialization_ref IS NOT NULL
                  AND render.dirty=0
                ORDER BY doc.safe_name
                """,
                (month_item,),
            ).fetchall()
            if len(docs) != 2:
                raise AcceptanceFailure("generated-finder-document-set-incomplete")
            for item_id, materialization_ref in docs:
                finder_path = _item_path(db, cloud_root, item_id)
                for _ in range(FINDER_SAMPLE_REPEATS):
                    attempts += 1
                    try:
                        result = subprocess.run(
                            ("cmp", "-s", materialization_ref, str(finder_path)),
                            timeout=FINDER_OPEN_TIMEOUT_SECONDS,
                            check=False,
                        )
                    except subprocess.TimeoutExpired as error:
                        raise AcceptanceFailure(
                            "generated-finder-open-timeout"
                        ) from error
                    if result.returncode != 0:
                        raise AcceptanceFailure("generated-finder-open-mismatch")
    return len(candidates), attempts


def compare_cursors(before: dict, after: dict) -> tuple[int, int, int]:
    missing = regressed = progressed = 0
    for key, old in before.items():
        new = after.get(key)
        if new is None:
            missing += 1
            continue
        old_oldest, old_newest, old_complete = old
        new_oldest, new_newest, new_complete = new
        preserved = (
            (old_oldest is None or (new_oldest is not None and new_oldest <= old_oldest))
            and (old_newest is None or (new_newest is not None and new_newest >= old_newest))
            and (not old_complete or bool(new_complete))
        )
        if not preserved:
            regressed += 1
        elif new != old:
            progressed += 1
    return missing, regressed, progressed


def finder_enumeration_ms(cloud_root: Path) -> int:
    started = time.monotonic()
    roots = [entry.path for entry in os.scandir(cloud_root) if entry.is_dir()]
    for root in roots:
        tuple(os.scandir(root))
    return round((time.monotonic() - started) * 1000)


def _cpu_time_seconds(value: str) -> float:
    fields = value.strip().split(":")
    if not fields or len(fields) > 3:
        raise AcceptanceFailure("installed-agent-cpu-time-invalid")
    seconds = float(fields[-1])
    if len(fields) >= 2:
        seconds += int(fields[-2]) * 60
    if len(fields) == 3:
        seconds += int(fields[0]) * 3_600
    return seconds


def agent_cpu_average_percent() -> float:
    result = subprocess.run(
        ("pgrep", "-x", "gramdrive-agent"),
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    pids = [line for line in result.stdout.splitlines() if line.isdigit()]
    if not pids:
        raise AcceptanceFailure("installed-agent-not-running")

    def total_cpu_seconds() -> float:
        sample = subprocess.run(
            ("ps", "-o", "time=", "-p", ",".join(pids)),
            capture_output=True,
            text=True,
            timeout=10,
            check=True,
        )
        return sum(_cpu_time_seconds(value) for value in sample.stdout.splitlines())

    started = time.monotonic()
    before = total_cpu_seconds()
    time.sleep(CPU_OBSERVATION_SECONDS)
    after = total_cpu_seconds()
    elapsed = time.monotonic() - started
    return round(max(0.0, after - before) * 100 / elapsed, 1)


def public_facts(phase: str, current: dict, cloud_root: Path) -> dict:
    latency = finder_enumeration_ms(cloud_root)
    cpu = agent_cpu_average_percent()
    return {
        "phase": phase,
        "privacy_safe": True,
        "schema_version": current["schema_version"],
        "eligible_chat_count": len(current["eligible_keys"]),
        "source_history_chat_count": len(current["source_history_keys"]),
        "source_precurrent_chat_count": len(current["source_precurrent_keys"]),
        "source_month_count": sum(
            len(months) for months in current["source_month_keys"].values()
        ),
        "published_source_month_count": len(
            current["published_source_month_keys"]
        ),
        "full_coverage_chat_count": len(current["full_coverage_keys"]),
        "full_coverage_chat_gain_count": 0,
        "old_projected_chat_count": len(current["old_projected_keys"]),
        "truthful_generated_chat_count": len(current["truthful_generated_keys"]),
        "visible_chat_appearance_count": current["visible_chat_appearance_count"],
        "terminal_cursor_count": current["terminal_cursor_count"],
        "incomplete_cursor_count": current["incomplete_cursor_count"],
        "anchored_cursor_count": current["anchored_cursor_count"],
        "media_blob_count": current["media_blob_count"],
        "finder_enumeration_ms": latency,
        "finder_enumeration_responsive": latency <= MAX_FINDER_ENUMERATION_MS,
        "agent_cpu_average_percent": cpu,
        # Retained for compatibility with the original aggregate evidence
        # schema; this is the bounded observation-window maximum average.
        "agent_cpu_max_percent": cpu,
        "agent_cpu_bounded": cpu <= MAX_AGENT_CPU_PERCENT,
        "gained_truthful_chat_count": 0,
        "projected_chat_gain_count": 0,
        "cursor_missing_count": 0,
        "cursor_regressed_count": 0,
        "cursor_progressed_count": 0,
        "cursor_progress_preserved": True,
        "media_hydration_unchanged": True,
        "prior_truthful_chats_preserved": True,
        "generated_finder_open_chat_count": 0,
        "generated_finder_open_attempt_count": 0,
        "generated_finder_open_stable": True,
    }


def validate_public(evidence: dict) -> None:
    if set(evidence) != PUBLIC_FIELDS:
        raise AcceptanceFailure("public-evidence-schema-invalid")
    if evidence.get("privacy_safe") is not True or evidence.get("phase") not in PUBLIC_PHASES:
        raise AcceptanceFailure("public-evidence-marker-invalid")
    if any(isinstance(value, (dict, list)) for value in evidence.values()):
        raise AcceptanceFailure("public-evidence-not-aggregate")
    for key, value in evidence.items():
        if key == "phase":
            continue
        if key in PUBLIC_BOOLEAN_FIELDS:
            if not isinstance(value, bool):
                raise AcceptanceFailure("public-evidence-type-invalid")
        elif isinstance(value, bool) or not isinstance(value, (int, float)):
            raise AcceptanceFailure("public-evidence-type-invalid")


def write_json(path: Path, value: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def run(args: argparse.Namespace) -> dict:
    database = args.data_root / "state/gramdrive.sqlite3"
    with connection(database) as db:
        current = snapshot(db)
    evidence = public_facts(args.phase, current, args.cloud_root)
    if args.phase == "before":
        write_json(args.state, {"before": current})
    else:
        with connection(database) as db:
            sample_chats, open_attempts = verify_repeated_generated_finder_opens(
                db, args.cloud_root, current
            )
        evidence.update(
            {
                "generated_finder_open_chat_count": sample_chats,
                "generated_finder_open_attempt_count": open_attempts,
                "generated_finder_open_stable": True,
            }
        )
        private = json.loads(args.state.read_text())
        prior = private["before"] if args.phase == "after" else private["after"]
        missing, regressed, progressed = compare_cursors(
            prior["cursors"], current["cursors"]
        )
        prior_truthful = set(prior["truthful_generated_keys"])
        current_truthful = set(current["truthful_generated_keys"])
        prior_full_coverage = set(prior.get("full_coverage_keys", []))
        current_full_coverage = set(current["full_coverage_keys"])
        evidence.update(
            {
                "gained_truthful_chat_count": len(current_truthful - prior_truthful),
                "projected_chat_gain_count": len(
                    set(current["old_projected_keys"])
                    - set(prior["old_projected_keys"])
                ),
                "cursor_missing_count": missing,
                "cursor_regressed_count": regressed,
                "cursor_progressed_count": progressed,
                "cursor_progress_preserved": missing == 0 and regressed == 0,
                "media_hydration_unchanged": (
                    current["media_blob_count"] == prior["media_blob_count"]
                ),
                "prior_truthful_chats_preserved": prior_truthful.issubset(
                    current_truthful
                ),
                "full_coverage_chat_gain_count": len(
                    current_full_coverage - prior_full_coverage
                ),
            }
        )
    validate_public(evidence)
    write_json(args.evidence, evidence)
    required: list[bool] = []
    if args.phase != "before":
        required.extend(
            [
                evidence["schema_version"] >= 13,
                evidence["finder_enumeration_responsive"],
                evidence["agent_cpu_bounded"],
                evidence["cursor_progress_preserved"],
                evidence["media_hydration_unchanged"],
                evidence["prior_truthful_chats_preserved"],
                evidence["full_coverage_chat_count"]
                >= max(
                    args.min_new_chats,
                    evidence["source_history_chat_count"] // 10,
                ),
                evidence["generated_finder_open_chat_count"]
                >= FINDER_SAMPLE_CHATS,
                evidence["generated_finder_open_stable"],
            ]
        )
    if args.phase == "after":
        required.extend(
            [
                evidence["gained_truthful_chat_count"] >= args.min_new_chats,
                evidence["projected_chat_gain_count"] >= args.min_new_chats,
                evidence["cursor_progressed_count"] > 0,
                evidence["full_coverage_chat_gain_count"] >= args.min_new_chats,
            ]
        )
    if not all(required):
        raise AcceptanceFailure("account-history-convergence-not-proven")
    if args.phase == "after":
        private["after"] = current
        write_json(args.state, private)
    return evidence


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=sorted(PUBLIC_PHASES))
    parser.add_argument("--data-root", type=Path, default=DEFAULT_DATA_ROOT)
    parser.add_argument("--cloud-root", type=Path, default=DEFAULT_CLOUD_ROOT)
    parser.add_argument("--state", type=Path, default=DEFAULT_STATE)
    parser.add_argument("--evidence", type=Path, default=DEFAULT_EVIDENCE)
    parser.add_argument("--min-new-chats", type=int, default=MIN_NEW_CHATS)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        evidence = run(args)
    except (
        AcceptanceFailure,
        OSError,
        sqlite3.Error,
        subprocess.SubprocessError,
        ValueError,
        KeyError,
    ) as error:
        label = str(error) if isinstance(error, AcceptanceFailure) else "acceptance-io-failed"
        print(f"installed history convergence failed: {label}", file=sys.stderr)
        return 1
    print(json.dumps(evidence, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
