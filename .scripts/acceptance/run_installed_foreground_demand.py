#!/usr/bin/env python3
"""Does using a chat in Finder actually buy that chat a history turn on the
installed profile? (BUG-260728-2qfzbd)

Two gestures, because they reach the extension very differently.

`read` — the default, and the one the acceptance boolean is read off. Reading
a file inside a chat always reaches the provider: dataless content can only be
produced by `fetchContents`, and the fetch raises `requested` demand for the
enclosing chat while it runs, releasing it again when it settles. That release
routinely wins the race to the agent's next scheduler boundary, which is why
the agent admits the hint into a ledger and owes the chat a turn regardless.
The document read is a generated one — rendered from the index the agent
already holds, so nothing is downloaded from Telegram to measure this.

`open` — the platform-truth record. On a replicated domain macOS answers a
read of an already-materialized folder out of its own copy of the namespace
and never calls the extension's enumerator, so the `visible` hint a folder
open would emit is simply never sent. Measured on the preserved profile: a
plain `readdir` and a real Finder window held open for 90 s both delivered
zero hints to the agent. A folder-open-only interaction is served by the fair
background rotation, and this gesture is kept so that claim stays measured
rather than asserted.

Either gesture runs the same shape, with no socket hint anywhere:

  control  watch the chosen chat for `window` seconds, touching nothing
  gesture  read the folder (`open`) or one generated document inside it
           (`read`) through the mounted domain, then watch it for the same
           `window`

A chat is only chosen if the background rotation demonstrably is not about to
reach it anyway: it must be incomplete, listed, reachable, and must already
have been given a turn recently (deep in the `last_backfill_at_ms` rotation).
That is what makes an advance during the gesture window attributable to the
gesture rather than to the rotation coming round.

The hints the agent actually received are read from its status endpoint before
and after the gesture, so "the chat did not advance" can be attributed to the
provider not sending or the agent not honoring, rather than guessed at.

Public evidence carries counts, byte-free booleans, seconds and message
*deltas* only: no account id, chat id, chat title, safe name, folder path, or
item identifier reaches it. The chosen chat's identity stays in the private
file, next to the raw cursor readings.

Exit code is nonzero when the acceptance boolean is false.
"""

from __future__ import annotations

import argparse
import json
import os
import socket as socketlib
import sqlite3
import struct
import sys
import time
from dataclasses import asdict, dataclass
from pathlib import Path

DEFAULT_STATE = (
    Path.home()
    / "Library/Group Containers/262RZ595FP.com.reluxworks.gramdrive"
    / "Library/Application Support/GramDrive/state/gramdrive.sqlite3"
)
DEFAULT_MOUNT = Path.home() / "Library/CloudStorage/GramDrive-GramDrive"

#: Most bytes read out of the chosen document. The read only has to reach
#: `fetchContents`; the file is materialized whole by the system either way,
#: and a generated document is small.
READ_LIMIT = 64 * 1024

#: How recently a chat may have been given a turn and still be considered
#: idle. The scheduler stamps a turn when it hands the work out, then keeps the
#: crawl across several ticks, so the chat with the newest stamp is very often
#: the one being crawled *right now* — and it advances during any window, which
#: would read as an open the probe caused. Anything stamped inside this window
#: is excluded for that reason.
ACTIVE_CRAWL_GRACE_MS = 180_000

#: Chats that still need history and are reachable by the scheduler. Ordered by
#: the rotation key *descending*, so the first row is the chat the background
#: scheduler will reach last — excluding any chat recent enough to still be the
#: active crawl.
CANDIDATES = """
SELECT s.chat_id           AS chat_id,
       s.account_id        AS account_id,
       s.namespace_version AS namespace_version,
       s.oldest_loaded_message_id AS oldest,
       s.newest_loaded_message_id AS newest,
       s.last_backfill_at_ms      AS last_backfill_at_ms,
       s.last_sync_at_ms          AS last_sync_at_ms
FROM chat_sync_state s
JOIN chats c ON c.chat_id = s.chat_id AND c.account_id = s.account_id
            AND c.namespace_version = s.namespace_version
LEFT JOIN chat_content_progress p ON p.chat_id = s.chat_id
            AND p.account_id = s.account_id
            AND p.namespace_version = s.namespace_version
WHERE s.history_complete = 0 AND c.deleted_at_ms IS NULL AND c.is_protected = 0
  AND s.oldest_loaded_message_id IS NOT NULL
  AND s.last_backfill_at_ms IS NOT NULL
  AND EXISTS (SELECT 1 FROM chat_list_entries e WHERE e.chat_id = s.chat_id
              AND e.account_id = s.account_id
              AND e.namespace_version = s.namespace_version)
  AND (p.chat_id IS NULL OR p.phase IN ('pending', 'syncing', 'cancelled')
       OR (p.phase = 'degraded' AND (p.retry_at_ms IS NULL OR p.retry_at_ms <= ?1)))
  AND s.last_backfill_at_ms <= ?2
ORDER BY s.last_backfill_at_ms DESC, s.chat_id
"""

BACKLOG_DEPTH = """
SELECT count(*)
FROM chat_sync_state s
JOIN chats c ON c.chat_id = s.chat_id AND c.account_id = s.account_id
            AND c.namespace_version = s.namespace_version
LEFT JOIN chat_content_progress p ON p.chat_id = s.chat_id
            AND p.account_id = s.account_id
            AND p.namespace_version = s.namespace_version
WHERE s.history_complete = 0 AND c.deleted_at_ms IS NULL AND c.is_protected = 0
  AND EXISTS (SELECT 1 FROM chat_list_entries e WHERE e.chat_id = s.chat_id
              AND e.account_id = s.account_id
              AND e.namespace_version = s.namespace_version)
  AND (p.chat_id IS NULL OR p.phase IN ('pending', 'syncing', 'cancelled')
       OR (p.phase = 'degraded' AND (p.retry_at_ms IS NULL OR p.retry_at_ms <= ?1)))
"""

#: Messages actually indexed inside a frontier move, counted through the
#: `messages` primary key. The raw `oldest` delta is in Telegram server-id
#: units, which are not messages; this is what the turn really fetched.
CRAWLED = """
SELECT count(*) FROM messages
WHERE account_id = ?1 AND namespace_version = ?2 AND chat_id = ?3
  AND message_id >= ?4 AND message_id <= ?5
"""

CURSOR = """
SELECT oldest_loaded_message_id AS oldest,
       newest_loaded_message_id AS newest,
       history_complete         AS history_complete,
       last_backfill_at_ms      AS last_backfill_at_ms,
       last_sync_at_ms          AS last_sync_at_ms
FROM chat_sync_state WHERE chat_id = ?1
"""


@dataclass(frozen=True)
class Reading:
    """One chat's scheduling-relevant cursor at one instant."""

    oldest: int | None
    newest: int | None
    history_complete: int
    last_backfill_at_ms: int | None
    last_sync_at_ms: int | None


@dataclass(frozen=True)
class Window:
    """What changed for the chat across one observation window."""

    seconds: float
    took_a_turn: bool
    frontier_moved_back_by: int
    messages_crawled_backward: int
    reached_history_complete: bool


def connect(state: Path) -> sqlite3.Connection:
    conn = sqlite3.connect(f"file:{state}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def read_cursor(conn: sqlite3.Connection, chat_id: int) -> Reading:
    row = conn.execute(CURSOR, (chat_id,)).fetchone()
    if row is None:
        raise SystemExit("the chosen chat lost its sync row mid-run")
    return Reading(
        oldest=row["oldest"],
        newest=row["newest"],
        history_complete=int(row["history_complete"] or 0),
        last_backfill_at_ms=row["last_backfill_at_ms"],
        last_sync_at_ms=row["last_sync_at_ms"],
    )


def compare(
    before: Reading, after: Reading, seconds: float, messages_crawled_backward: int = 0
) -> Window:
    """A turn shows up as a fresh `last_backfill_at_ms`; real backward progress
    shows up as a lower `oldest`. Report both — a turn that crawled nothing is
    not the thing the fix claims."""
    took_a_turn = (
        after.last_backfill_at_ms is not None
        and before.last_backfill_at_ms is not None
        and after.last_backfill_at_ms > before.last_backfill_at_ms
    )
    moved = 0
    if before.oldest is not None and after.oldest is not None:
        moved = max(0, before.oldest - after.oldest)
    return Window(
        seconds=round(seconds, 1),
        took_a_turn=took_a_turn,
        frontier_moved_back_by=moved,
        messages_crawled_backward=messages_crawled_backward,
        reached_history_complete=bool(
            after.history_complete and not before.history_complete
        ),
    )


#: The frozen v1 main-view chat-appearance identifier, which is the only part of
#: the item-id encoding this probe decodes: format version, appearance tag, list
#: kind, canonical chat tag, then account/namespace/chat.
CHAT_APPEARANCE_PREFIX = bytes([0x01, 0x10, 0x01, 0x03])
CHAT_APPEARANCE_LEN = len(CHAT_APPEARANCE_PREFIX) + 8 + 4 + 8


def chat_appearance_item_id(account_id: int, namespace_version: int, chat_id: int) -> bytes:
    """Rebuild one chat's main-view item identifier rather than searching for
    it. The encoding is frozen at v1 and pinned by the core's golden tests, so
    building it here cannot drift silently: a changed encoding stops matching
    any row and the probe reports no candidate rather than a wrong one."""
    return CHAT_APPEARANCE_PREFIX + struct.pack(
        ">qIq", account_id, namespace_version, chat_id
    )


def folder_path(conn: sqlite3.Connection, item_id: bytes, mount: Path) -> Path | None:
    """Walk the item's parents up to the account root and join their safe names,
    which is exactly how the domain lays the tree out on disk."""
    names: list[str] = []
    current: bytes | None = item_id
    while current is not None:
        row = conn.execute(
            "SELECT parent_item_id, safe_name, kind, deleted_at_ms FROM items WHERE item_id = ?",
            (current,),
        ).fetchone()
        if row is None or row["deleted_at_ms"] is not None:
            return None
        current = row["parent_item_id"]
        if row["kind"] != "account":
            names.append(row["safe_name"])
    return mount.joinpath(*reversed(names))


def choose_chat(conn: sqlite3.Connection, mount: Path, now_ms: int):
    """The reachable incomplete chat the rotation will get to *last* and whose
    folder is actually present on the mounted domain — never one that could
    still be the crawl in flight."""
    idle_before_ms = now_ms - ACTIVE_CRAWL_GRACE_MS
    for row in conn.execute(CANDIDATES, (now_ms, idle_before_ms)):
        item_id = chat_appearance_item_id(
            int(row["account_id"]), int(row["namespace_version"]), int(row["chat_id"])
        )
        folder = folder_path(conn, item_id, mount)
        if folder is not None and folder.is_dir():
            scope = (
                int(row["account_id"]),
                int(row["namespace_version"]),
                int(row["chat_id"]),
            )
            return scope, folder
    return None, None


def enumerate_folder(folder: Path) -> int:
    """Read the folder the way Finder does. `os.scandir` on a File Provider
    domain drives the extension's `enumerateItems`, which is the callback that
    signals the visible hint — and, on invalidation, releases it again. It
    materializes placeholders, never content."""
    with os.scandir(folder) as entries:
        return sum(1 for _ in entries)


#: The smallest live generated document inside one chat, preferring one that
#: hangs directly off the chat directory (`.chat.json`) over one nested in a
#: month. Generated documents are rendered from the index the agent already
#: holds, so reading one downloads no Telegram payload — which is what keeps
#: this probe inside the bug's "without downloading payload bytes" scope.
CHAT_DOCUMENT = """
SELECT d.item_id AS item_id, COALESCE(d.logical_size, 0) AS size_rank, 0 AS depth
FROM items d
WHERE d.parent_item_id = ?1 AND d.kind = 'generated_doc' AND d.deleted_at_ms IS NULL
UNION ALL
SELECT d.item_id, COALESCE(d.logical_size, 0), 1
FROM items d
JOIN items m ON m.item_id = d.parent_item_id
WHERE m.parent_item_id = ?1 AND m.deleted_at_ms IS NULL
  AND d.kind = 'generated_doc' AND d.deleted_at_ms IS NULL
ORDER BY depth, size_rank, item_id
LIMIT 1
"""


def choose_document(conn: sqlite3.Connection, chat_item_id: bytes, mount: Path) -> Path | None:
    """The file whose read stands in for "the user is in this chat"."""
    row = conn.execute(CHAT_DOCUMENT, (chat_item_id,)).fetchone()
    if row is None:
        return None
    path = folder_path(conn, row["item_id"], mount)
    if path is None or not path.is_file():
        return None
    return path


def read_document(path: Path) -> dict:
    """Read the document through the mounted domain. On a dataless file this is
    what drives the extension's `fetchContents`, which is the callback that
    raises the enclosing chat's `requested` demand.

    A read that fails is still a read that happened: the demand is raised as
    soon as the fetch resolves its item, before any byte is transferred, so a
    fetch the system gives up on (`ETIMEDOUT` while the agent is saturated) has
    still delivered the hint. The failure is reported rather than raised, so
    the windows either side of it stay measurable — and so the artifact says
    plainly that no bytes arrived."""
    try:
        with open(path, "rb") as handle:
            return {"document_bytes_read": len(handle.read(READ_LIMIT))}
    except OSError as error:
        return {
            "document_bytes_read": 0,
            "document_read_failed_errno": error.errno,
        }


def connect_unix(client: socketlib.socket, socket_path: Path) -> None:
    """`sun_path` holds 104 bytes and the group container's path is longer than
    that, so a long path is connected the same way the agent's own clients do
    it: from inside the socket's directory, by leaf name."""
    if len(str(socket_path).encode("utf-8")) <= 100:
        client.connect(str(socket_path))
        return
    previous = os.getcwd()
    os.chdir(socket_path.parent)
    try:
        client.connect(socket_path.name)
    finally:
        os.chdir(previous)


def agent_hint_counts(socket_path: Path, timeout: float = 2.0) -> dict | None:
    """The hint counters at the receiving end, from the agent's status
    endpoint. Counts only — the payload carries no chat identity by design.

    Best effort: an agent that is not running, a socket that is not there, or
    a payload without the counters answers `None`, and the probe reports that
    it could not observe them rather than failing the acceptance boolean on an
    instrument that was unavailable."""
    request = json.dumps({"protocolVersion": 1, "operation": "status"}) + "\n"
    try:
        with socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM) as client:
            client.settimeout(timeout)
            connect_unix(client, socket_path)
            client.sendall(request.encode("utf-8"))
            chunks: list[bytes] = []
            while b"\n" not in b"".join(chunks):
                chunk = client.recv(65536)
                if not chunk:
                    break
                chunks.append(chunk)
        line = b"".join(chunks).split(b"\n", 1)[0]
        payload = json.loads(line.decode("utf-8"))
    except (OSError, ValueError):
        return None
    status = payload.get("status")
    if not isinstance(status, dict):
        return None
    hints = status.get("historyPriorityHints")
    return hints if isinstance(hints, dict) else None


def hint_delta(before: dict | None, after: dict | None) -> dict:
    """What the agent received across the gesture. `observed` is false when the
    counters could not be read at either end — an unobserved instrument is
    reported as unobserved, never as a zero."""
    if before is None or after is None:
        return {"observed": False}
    delta = {
        f"{kind}_delta": int(after.get(kind, 0)) - int(before.get(kind, 0))
        for kind in ("accepted", "visible", "requested", "background", "unroutable")
    }
    delta["observed"] = True
    return delta


def crawled_backward(
    conn: sqlite3.Connection, scope: tuple[int, int, int], before: Reading, after: Reading
) -> int:
    if before.oldest is None or after.oldest is None or after.oldest >= before.oldest:
        return 0
    account_id, namespace_version, chat_id = scope
    return int(
        conn.execute(
            CRAWLED,
            (account_id, namespace_version, chat_id, after.oldest, before.oldest),
        ).fetchone()[0]
    )


def watch(
    conn: sqlite3.Connection,
    scope: tuple[int, int, int],
    seconds: float,
    pause=time.sleep,
) -> Window:
    chat_id = scope[2]
    before = read_cursor(conn, chat_id)
    started = time.monotonic()
    pause(seconds)
    elapsed = time.monotonic() - started
    after = read_cursor(conn, chat_id)
    return compare(before, after, elapsed, crawled_backward(conn, scope, before, after))


def control_socket(state: Path) -> Path:
    """The agent's control endpoint beside the state file: both hang off the
    same data root."""
    return state.parent.parent / "agent" / "control.sock"


def run(
    state: Path,
    mount: Path,
    window: float,
    out: Path,
    private: Path,
    pause=time.sleep,
    gesture: str = "read",
    socket_path: Path | None = None,
    hints=agent_hint_counts,
) -> int:
    conn = connect(state)
    now_ms = int(time.time() * 1000)
    backlog = int(conn.execute(BACKLOG_DEPTH, (now_ms,)).fetchone()[0])
    scope, folder = choose_chat(conn, mount, now_ms)
    if scope is None:
        evidence = {
            "measured": False,
            "gesture": gesture,
            "reason": "no reachable incomplete chat has a folder on the domain",
            "backlog_depth": backlog,
        }
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(json.dumps(evidence, indent=2) + "\n")
        print(json.dumps(evidence, indent=2))
        return 0

    chat_id = scope[2]
    document: Path | None = None
    if gesture == "read":
        document = choose_document(
            conn, chat_appearance_item_id(scope[0], scope[1], chat_id), mount
        )
        if document is None:
            evidence = {
                "measured": False,
                "gesture": gesture,
                "reason": "the chosen chat has no generated document on the domain",
                "backlog_depth": backlog,
            }
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_text(json.dumps(evidence, indent=2) + "\n")
            print(json.dumps(evidence, indent=2))
            return 0

    socket_path = socket_path or control_socket(state)
    chosen_at = read_cursor(conn, chat_id)
    control = watch(conn, scope, window, pause)

    before_hints = hints(socket_path)
    performed: dict[str, int] = {}
    if gesture == "read":
        assert document is not None
        performed.update(read_document(document))
    else:
        performed["folder_entries_enumerated"] = enumerate_folder(folder)
    after_hints = hints(socket_path)

    acted = watch(conn, scope, window, pause)

    granted = acted.took_a_turn and not control.took_a_turn
    boolean = (
        "content_read_granted_a_turn"
        if gesture == "read"
        else "foreground_open_granted_a_turn"
    )
    evidence = {
        "measured": True,
        "gesture": gesture,
        "backlog_depth": backlog,
        "window_seconds": round(window, 1),
        **performed,
        "hints_delivered_to_the_agent": hint_delta(before_hints, after_hints),
        "control": asdict(control),
        "after_gesture": asdict(acted),
        boolean: granted,
    }
    private.parent.mkdir(parents=True, exist_ok=True)
    private.write_text(
        json.dumps(
            {
                "chat_id": chat_id,
                "folder": str(folder),
                "document": str(document) if document is not None else None,
                "at_choice": asdict(chosen_at),
                "after_control": asdict(read_cursor(conn, chat_id)),
            },
            indent=2,
        )
        + "\n"
    )
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))
    return 0 if granted else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", type=Path, default=DEFAULT_STATE)
    parser.add_argument("--mount", type=Path, default=DEFAULT_MOUNT)
    parser.add_argument(
        "--window",
        type=float,
        default=45.0,
        help="seconds to watch the chat in each of the control and gesture phases",
    )
    parser.add_argument(
        "--gesture",
        choices=("read", "open"),
        default="read",
        help="read one generated document inside the chat, or only open its folder",
    )
    parser.add_argument(
        "--socket",
        type=Path,
        default=None,
        help="agent control endpoint (defaults to the one beside --state)",
    )
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--private", type=Path, required=True)
    args = parser.parse_args(argv)
    return run(
        args.state,
        args.mount,
        args.window,
        args.output,
        args.private,
        gesture=args.gesture,
        socket_path=args.socket,
    )


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
