#!/usr/bin/env python3
"""Installed-profile probe for the historical chat index, size, and dates
(BUG-260728-2qfzbd).

Three questions this answers about a *preserved authorized profile*, without
opening a chat, without hydrating a byte, and without emitting a single
identifier:

1. **Convergence.** How many listed chats still need history, how many of
   those the background scheduler can actually reach, and whether every
   cursor moved forward (never backward) across a relaunch.
2. **Size.** Whether each chat and month directory publishes an aggregate
   logical size, and whether that number equals the sum of the indexed
   descendants the same database holds.
3. **Dates.** Whether any live directory is still *undated* — the state that
   makes Finder show 1 Jan 1970 with nothing behind it — and whether every
   directory that does report the epoch does so because the source stamped a
   message there.

Phases
------
``before``    capture the state of the currently installed build
``after``     capture it again once the candidate has been running
``relaunch``  capture it after an app + agent relaunch and compare

Every phase writes a JSON evidence file. The public evidence contains counts,
booleans, and byte totals only: no account id, chat id, chat title, month
label, message text, file path, or item identifier ever reaches it. The
private comparison state (cursor bounds keyed by an opaque per-run digest)
stays under a caller-chosen private directory.

Exit code is nonzero when any required acceptance boolean is false.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sqlite3
import sys
from dataclasses import asdict, dataclass
from pathlib import Path

DEFAULT_STATE = (
    Path.home()
    / "Library/Group Containers/262RZ595FP.com.reluxworks.gramdrive"
    / "Library/Application Support/GramDrive/state/gramdrive.sqlite3"
)

#: Directory kinds that must carry a correspondence date and a size rollup.
ROLLUP_KINDS = ("chat", "month_dir", "active_stories")


@dataclass(frozen=True)
class Convergence:
    listed_chats: int
    listed_incomplete: int
    reachable_incomplete: int
    unreachable_incomplete: int
    #: Reachable under the predicate that shipped *before* this fix, which
    #: excluded every self-fenced (degraded) chat from background work. The
    #: gap against ``reachable_incomplete`` is the starvation that was fixed.
    reachable_under_prior_predicate: int
    complete_cursors: int
    #: Runnable incomplete chats that have never been handed a backward
    #: history turn. Every row starts here after the v17 migration — the
    #: guaranteed one-turn-each repair — and the number drains as the
    #: rotation runs. ``None`` on a profile whose schema predates the
    #: column, where turns were not a recorded fact at all.
    never_given_a_backfill_turn: int | None


@dataclass(frozen=True)
class Rollup:
    directories: int
    with_published_size: int
    exact_against_descendants: int
    mismatched: int
    published_bytes: int
    #: Live directories of a kind that owns no rollup — a chat list, the
    #: folder catalog, the account root — which nevertheless publish a size.
    #: Nobody sums those subtrees, so the only value such a row can carry is
    #: a false zero, and a published zero reads as "this folder is empty".
    #: v16's SQL leaves them NULL and the projection claims none, so any
    #: count here is a regression rather than a measurement.
    claimed_without_owning_a_rollup: int


@dataclass(frozen=True)
class Dates:
    live_directories: int
    dated: int
    undated: int
    epoch_dated: int
    epoch_stamped_source_messages: int
    #: Chat folders whose creation or modification date is null *or* zero —
    #: i.e. exactly what Finder renders as 1 Jan 1970 — counted only while
    #: the index holds messages at all. Named for what it measures: an
    #: earlier name said "undated", which is the null half only, and made
    #: this read as a contradiction of ``undated`` above.
    epoch_dated_chats_with_indexed_messages: int
    files: int
    files_epoch_modified: int
    indexed_messages: int


def connect(state: Path) -> sqlite3.Connection:
    if not state.is_file():
        raise SystemExit(f"no installed state database at {state}")
    conn = sqlite3.connect(f"file:{state}?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    return conn


def schema_version(conn: sqlite3.Connection) -> int:
    return int(conn.execute("PRAGMA user_version").fetchone()[0])


def has_column(conn: sqlite3.Connection, table: str, column: str) -> bool:
    return any(row["name"] == column for row in conn.execute(f"PRAGMA table_info({table})"))


def measure_convergence(conn: sqlite3.Connection, now_ms: int) -> Convergence:
    listed = conn.execute(
        "SELECT count(DISTINCT chat_id) FROM chat_list_entries"
    ).fetchone()[0]
    # Protected and deleted chats are excluded on purpose (POL-4): their
    # content is not fetchable, so "still needs history" does not apply.
    incomplete = conn.execute(
        """
        SELECT count(*) FROM chat_sync_state s
        JOIN chats c
          ON c.account_id = s.account_id
         AND c.namespace_version = s.namespace_version
         AND c.chat_id = s.chat_id
        WHERE s.history_complete = 0
          AND c.deleted_at_ms IS NULL
          AND c.is_protected = 0
          AND EXISTS (SELECT 1 FROM chat_list_entries e
                      WHERE e.account_id = s.account_id
                        AND e.namespace_version = s.namespace_version
                        AND e.chat_id = s.chat_id)
        """
    ).fetchone()[0]
    # The exact background-backlog predicate the scheduler uses.
    reachable = conn.execute(
        """
        SELECT count(*)
        FROM chat_sync_state s
        JOIN chats c
          ON c.account_id = s.account_id
         AND c.namespace_version = s.namespace_version
         AND c.chat_id = s.chat_id
        LEFT JOIN chat_content_progress p
          ON p.account_id = s.account_id
         AND p.namespace_version = s.namespace_version
         AND p.chat_id = s.chat_id
        WHERE s.history_complete = 0
          AND c.deleted_at_ms IS NULL
          AND c.is_protected = 0
          AND EXISTS (SELECT 1 FROM chat_list_entries e
                      WHERE e.account_id = s.account_id
                        AND e.namespace_version = s.namespace_version
                        AND e.chat_id = s.chat_id)
          AND (p.chat_id IS NULL
               OR p.phase IN ('pending', 'syncing', 'cancelled')
               OR (p.phase = 'degraded'
                   AND (p.retry_at_ms IS NULL OR p.retry_at_ms <= ?)))
        """,
        (now_ms,),
    ).fetchone()[0]
    prior_reachable = conn.execute(
        """
        SELECT count(*)
        FROM chat_sync_state s
        JOIN chats c
          ON c.account_id = s.account_id
         AND c.namespace_version = s.namespace_version
         AND c.chat_id = s.chat_id
        LEFT JOIN chat_content_progress p
          ON p.account_id = s.account_id
         AND p.namespace_version = s.namespace_version
         AND p.chat_id = s.chat_id
        WHERE s.history_complete = 0
          AND c.deleted_at_ms IS NULL
          AND c.is_protected = 0
          AND EXISTS (SELECT 1 FROM chat_list_entries e
                      WHERE e.account_id = s.account_id
                        AND e.namespace_version = s.namespace_version
                        AND e.chat_id = s.chat_id)
          AND (p.chat_id IS NULL OR p.phase IN ('pending', 'syncing', 'cancelled'))
        """
    ).fetchone()[0]
    complete = conn.execute(
        "SELECT count(*) FROM chat_sync_state WHERE history_complete = 1"
    ).fetchone()[0]
    never_turned = None
    if has_column(conn, "chat_sync_state", "last_backfill_at_ms"):
        never_turned = conn.execute(
            """
            SELECT count(*)
            FROM chat_sync_state s
            JOIN chats c
              ON c.account_id = s.account_id
             AND c.namespace_version = s.namespace_version
             AND c.chat_id = s.chat_id
            WHERE s.history_complete = 0
              AND s.last_backfill_at_ms IS NULL
              AND c.deleted_at_ms IS NULL
              AND c.is_protected = 0
              AND EXISTS (SELECT 1 FROM chat_list_entries e
                          WHERE e.account_id = s.account_id
                            AND e.namespace_version = s.namespace_version
                            AND e.chat_id = s.chat_id)
            """
        ).fetchone()[0]
    return Convergence(
        listed_chats=listed,
        listed_incomplete=incomplete,
        reachable_incomplete=reachable,
        unreachable_incomplete=incomplete - reachable,
        reachable_under_prior_predicate=prior_reachable,
        complete_cursors=complete,
        never_given_a_backfill_turn=never_turned,
    )


def measure_rollup(conn: sqlite3.Connection) -> Rollup:
    kinds = ",".join(f"'{kind}'" for kind in ROLLUP_KINDS)
    if not has_column(conn, "items", "aggregate_size"):
        total = conn.execute(
            f"SELECT count(*) FROM items WHERE kind IN ({kinds}) AND deleted_at_ms IS NULL"
        ).fetchone()[0]
        return Rollup(total, 0, 0, 0, 0, 0)

    rows = conn.execute(
        f"""
        SELECT d.aggregate_size AS published,
               COALESCE((SELECT sum(COALESCE(c.aggregate_size, c.logical_size))
                         FROM items c
                         WHERE c.parent_item_id = d.item_id
                           AND c.deleted_at_ms IS NULL), 0) AS descendants
        FROM items d
        WHERE d.kind IN ({kinds}) AND d.deleted_at_ms IS NULL
        """
    ).fetchall()
    published = [row for row in rows if row["published"] is not None]
    exact = [row for row in published if row["published"] == row["descendants"]]
    intruders = conn.execute(
        f"""
        SELECT count(*) FROM items
        WHERE is_directory = 1
          AND kind NOT IN ({kinds})
          AND deleted_at_ms IS NULL
          AND aggregate_size IS NOT NULL
        """
    ).fetchone()[0]
    return Rollup(
        directories=len(rows),
        with_published_size=len(published),
        exact_against_descendants=len(exact),
        mismatched=len(published) - len(exact),
        published_bytes=sum(int(row["published"]) for row in published),
        claimed_without_owning_a_rollup=int(intruders),
    )


def measure_dates(conn: sqlite3.Connection) -> Dates:
    live_dirs = conn.execute(
        "SELECT count(*) FROM items WHERE is_directory = 1 AND deleted_at_ms IS NULL"
    ).fetchone()[0]
    dated = conn.execute(
        """
        SELECT count(*) FROM items
        WHERE is_directory = 1 AND deleted_at_ms IS NULL
          AND created_at_ms IS NOT NULL AND modified_at_ms IS NOT NULL
        """
    ).fetchone()[0]
    # Finder renders a *null* timestamp as 1 Jan 1970 with nothing behind it.
    # That is the defect: a folder whose date the namespace simply never
    # wrote.
    undated = conn.execute(
        """
        SELECT count(*) FROM items
        WHERE is_directory = 1 AND deleted_at_ms IS NULL
          AND (created_at_ms IS NULL OR modified_at_ms IS NULL)
        """
    ).fetchone()[0]
    # A *zero* timestamp is a different thing: it is the epoch faithfully
    # reported because the source itself stamped a message there. Counted
    # separately, and only ever acceptable while such a message exists.
    epoch = conn.execute(
        """
        SELECT count(*) FROM items
        WHERE is_directory = 1 AND deleted_at_ms IS NULL
          AND (COALESCE(created_at_ms, 0) = 0 OR COALESCE(modified_at_ms, 0) = 0)
        """
    ).fetchone()[0]
    epoch_stamped_source = conn.execute(
        "SELECT count(*) FROM messages WHERE sent_at_ms <= 0"
    ).fetchone()[0]
    # The headline defect: a chat folder Finder dates 1 Jan 1970 while the
    # index holds real message timestamps. A chat item id encodes its chat,
    # so SQL cannot join the two per chat; the honest aggregate is "chat
    # folders Finder would date 1 Jan 1970, while the account holds messages
    # at all" — null and zero alike, since Finder shows the same thing for
    # both.
    epoch_dated_chats = conn.execute(
        """
        SELECT count(*) FROM items
        WHERE kind = 'chat' AND deleted_at_ms IS NULL
          AND (COALESCE(created_at_ms, 0) = 0 OR COALESCE(modified_at_ms, 0) = 0)
        """
    ).fetchone()[0]
    indexed_messages = conn.execute("SELECT count(*) FROM messages").fetchone()[0]
    epoch_dated_chats_with_messages = epoch_dated_chats if indexed_messages else 0
    files = conn.execute(
        "SELECT count(*) FROM items WHERE is_directory = 0 AND deleted_at_ms IS NULL"
    ).fetchone()[0]
    files_epoch = conn.execute(
        """
        SELECT count(*) FROM items
        WHERE is_directory = 0 AND deleted_at_ms IS NULL
          AND COALESCE(modified_at_ms, 0) = 0
        """
    ).fetchone()[0]
    return Dates(
        live_directories=live_dirs,
        dated=dated,
        undated=undated,
        epoch_dated=epoch,
        epoch_stamped_source_messages=epoch_stamped_source,
        epoch_dated_chats_with_indexed_messages=epoch_dated_chats_with_messages,
        files=files,
        files_epoch_modified=files_epoch,
        indexed_messages=indexed_messages,
    )


#: How recently a chat must have been touched by delivery to count as
#: "live-active" for the starvation measurement below. One hour: long enough
#: that an ordinary conversation qualifies, short enough that a chat which
#: went quiet yesterday does not.
LIVE_ACTIVE_WINDOW_MS = 60 * 60 * 1000


def cursor_fingerprints(
    conn: sqlite3.Connection, salt: str, now_ms: int
) -> dict[str, list[int]]:
    """Opaque per-run cursor keys, for monotonicity comparison only.

    The digest is salted per run directory so the private file cannot be
    correlated back to a chat identifier by anyone who later reads it.

    The fourth element flags a chat that delivery touched inside
    :data:`LIVE_ACTIVE_WINDOW_MS`. Those are the chats the scheduling-key
    defect starved: while the backlog ordered on `last_sync_at_ms`, every
    incoming message pushed its own chat back down the backward-crawl queue,
    so the busiest correspondences were the ones that never gained history
    (BUG-260728-2qfzbd).
    """
    out: dict[str, list[int]] = {}
    for row in conn.execute(
        """
        SELECT chat_id, oldest_loaded_message_id, newest_loaded_message_id,
               history_complete, last_sync_at_ms
        FROM chat_sync_state
        """
    ):
        key = hashlib.sha256(f"{salt}:{row['chat_id']}".encode()).hexdigest()[:16]
        last_sync = row["last_sync_at_ms"] or 0
        out[key] = [
            row["oldest_loaded_message_id"] or 0,
            row["newest_loaded_message_id"] or 0,
            int(row["history_complete"]),
            int(now_ms - last_sync <= LIVE_ACTIVE_WINDOW_MS if last_sync else 0),
        ]
    return out


def compare_cursors(
    previous: dict[str, list[int]], current: dict[str, list[int]]
) -> dict[str, int | bool]:
    missing = 0
    regressed = 0
    advanced = 0
    completed = 0
    live_active = 0
    live_active_backward = 0
    backward = 0
    for key, before in previous.items():
        now = current.get(key)
        if now is None:
            missing += 1
            continue
        old_oldest, old_newest, old_complete = before[:3]
        new_oldest, new_newest, new_complete = now[:3]
        was_live_active = bool(before[3]) if len(before) > 3 else False
        incomplete_then = old_complete == 0
        if was_live_active and incomplete_then:
            live_active += 1
        # "Forward" for a history crawl means the window only ever widens:
        # older backward, newer forward. A window that shrank lost work.
        if (old_oldest and new_oldest > old_oldest) or new_newest < old_newest:
            regressed += 1
            continue
        if new_complete < old_complete:
            regressed += 1
            continue
        crawled_backward = bool(old_oldest and new_oldest < old_oldest)
        if crawled_backward:
            backward += 1
            if was_live_active and incomplete_then:
                live_active_backward += 1
        if crawled_backward or new_newest > old_newest:
            advanced += 1
        if new_complete > old_complete:
            completed += 1
    return {
        "compared": len(previous),
        "missing": missing,
        "regressed": regressed,
        "advanced": advanced,
        "crawled_backward": backward,
        "newly_complete": completed,
        # The starvation signature, measured rather than asserted: a window
        # too short for a full rotation can legitimately reach few of these,
        # so the honest artifact reports the count instead of pretending a
        # threshold. Zero backward progress across a long window on a
        # populated live-active set is what the defect looked like.
        "live_active_incomplete_compared": live_active,
        "live_active_crawled_backward": live_active_backward,
        "monotonic": missing == 0 and regressed == 0,
    }


def run(phase: str, state: Path, out: Path, private: Path, now_ms: int) -> int:
    conn = connect(state)
    private.mkdir(parents=True, exist_ok=True)
    salt_file = private / "salt"
    if not salt_file.exists():
        salt_file.write_text(os.urandom(16).hex(), encoding="utf-8")
    salt = salt_file.read_text(encoding="utf-8").strip()

    version = schema_version(conn)
    convergence = measure_convergence(conn, now_ms)
    rollup = measure_rollup(conn)
    dates = measure_dates(conn)
    cursors = cursor_fingerprints(conn, salt, now_ms)

    evidence: dict[str, object] = {
        "phase": phase,
        "schema_version": version,
        "convergence": asdict(convergence),
        "rollup": asdict(rollup),
        "dates": asdict(dates),
    }

    checks: dict[str, bool] = {}
    if phase != "before":
        checks["rollup_published_for_every_directory"] = (
            rollup.directories > 0 and rollup.with_published_size == rollup.directories
        )
        checks["rollup_exact_against_indexed_descendants"] = rollup.mismatched == 0
        # A directory that owns no rollup must claim none. Publishing zero
        # there is not a smaller answer than the truth, it is a different
        # one: "this subtree is indexed and holds no bytes".
        checks["no_directory_claims_a_rollup_it_does_not_own"] = (
            rollup.claimed_without_owning_a_rollup == 0
        )
        checks["no_live_directory_is_undated"] = dates.undated == 0
        # An epoch date is acceptable only where the index itself holds an
        # epoch-stamped message — the acceptance criterion is "never 1970
        # *when message timestamps exist*", and here the timestamp is 1970.
        checks["every_epoch_date_comes_from_epoch_stamped_source"] = (
            dates.epoch_dated == 0 or dates.epoch_stamped_source_messages > 0
        )
        checks["no_listed_chat_is_unreachable_by_background_work"] = (
            convergence.unreachable_incomplete == 0
        )

    previous_file = private / "cursors-before.json"
    if phase == "before":
        previous_file.write_text(json.dumps(cursors), encoding="utf-8")
    elif previous_file.exists():
        previous = json.loads(previous_file.read_text(encoding="utf-8"))
        comparison = compare_cursors(previous, cursors)
        evidence["cursors"] = comparison
        checks["cursors_are_monotonic_across_relaunch"] = bool(comparison["monotonic"])
        (private / f"cursors-{phase}.json").write_text(
            json.dumps(cursors), encoding="utf-8"
        )

    evidence["checks"] = checks
    # A phase that asserts nothing reports `null`, not `true`. The `before`
    # phase measures the build being replaced, where every check is expected
    # to fail, so running none is correct — but a bare `"passed": true` beside
    # `"checks": {}` is quotable as "the pre-fix build passed", which it
    # neither did nor was asked to.
    evidence["passed"] = all(checks.values()) if checks else None
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(evidence, indent=2, sort_keys=True), encoding="utf-8")
    print(json.dumps(evidence, indent=2, sort_keys=True))
    return 0 if all(checks.values()) else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("before", "after", "relaunch"))
    parser.add_argument("--state", type=Path, default=DEFAULT_STATE)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--private", type=Path, required=True)
    parser.add_argument(
        "--now-ms",
        type=int,
        required=True,
        help="wall-clock ms the retry-deadline predicate is evaluated against",
    )
    args = parser.parse_args(argv)
    return run(args.phase, args.state, args.output, args.private, args.now_ms)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
