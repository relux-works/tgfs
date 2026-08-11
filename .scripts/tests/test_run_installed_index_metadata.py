#!/usr/bin/env python3
"""Tests for the installed chat-index metadata probe (BUG-260728-2qfzbd).

The probe is the instrument the acceptance evidence is read off, so the
properties worth pinning are the ones a wrong reading would hide: that it
separates *undated* from *epoch-dated*, that it scopes the rollup to the
kinds that own one, that a phase asserting nothing does not report a pass,
and that cursor comparison calls a shrinking window a regression.
"""

from __future__ import annotations

import importlib.util
import json
import sqlite3
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = REPO_ROOT / ".scripts" / "acceptance" / "run_installed_index_metadata.py"


def load_runner():
    spec = importlib.util.spec_from_file_location(
        "run_installed_index_metadata", RUNNER_PATH
    )
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


probe = load_runner()

#: The v16 shape, reduced to the columns this probe reads.
SCHEMA = """
CREATE TABLE items (
    item_id BLOB PRIMARY KEY, account_id INTEGER, namespace_version INTEGER,
    kind TEXT, parent_item_id BLOB, is_directory INTEGER,
    logical_size INTEGER, aggregate_size INTEGER,
    created_at_ms INTEGER, modified_at_ms INTEGER, deleted_at_ms INTEGER
);
CREATE TABLE messages (
    account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
    message_id INTEGER, sent_at_ms INTEGER
);
CREATE TABLE chats (
    account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
    deleted_at_ms INTEGER, is_protected INTEGER
);
CREATE TABLE chat_list_entries (
    account_id INTEGER, namespace_version INTEGER, chat_id INTEGER
);
CREATE TABLE chat_sync_state (
    account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
    oldest_loaded_message_id INTEGER, newest_loaded_message_id INTEGER,
    history_complete INTEGER, last_sync_at_ms INTEGER, last_backfill_at_ms INTEGER
);
CREATE TABLE chat_content_progress (
    account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
    phase TEXT, retry_at_ms INTEGER
);
"""


def memory_db() -> sqlite3.Connection:
    conn = sqlite3.connect(":memory:")
    conn.row_factory = sqlite3.Row
    conn.executescript(SCHEMA)
    return conn


def add_item(conn, item_id, kind, *, parent=None, directory=True, **columns):
    conn.execute(
        """
        INSERT INTO items (
            item_id, account_id, namespace_version, kind, parent_item_id,
            is_directory, logical_size, aggregate_size, created_at_ms,
            modified_at_ms, deleted_at_ms
        ) VALUES (?1, 7, 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        """,
        (
            item_id,
            kind,
            parent,
            1 if directory else 0,
            columns.get("logical_size"),
            columns.get("aggregate_size"),
            columns.get("created_at_ms"),
            columns.get("modified_at_ms"),
            columns.get("deleted_at_ms"),
        ),
    )


class DateMeasurementTests(unittest.TestCase):
    def test_separates_an_undated_directory_from_an_epoch_dated_one(self):
        # These are different defects and only the first one is this bug's:
        # a null date is a folder the namespace never wrote a date for, while
        # a zero date is the epoch reported faithfully because the source
        # stamped a message there.
        conn = memory_db()
        add_item(conn, b"\x01", "chat", created_at_ms=None, modified_at_ms=None)
        add_item(conn, b"\x02", "chat", created_at_ms=0, modified_at_ms=0)
        add_item(conn, b"\x03", "chat", created_at_ms=1700, modified_at_ms=1800)
        conn.execute("INSERT INTO messages VALUES (7, 1, -5, 1, 0)")

        dates = probe.measure_dates(conn)

        self.assertEqual(dates.live_directories, 3)
        self.assertEqual(dates.dated, 2, "a null-dated directory is not dated")
        self.assertEqual(dates.undated, 1, "exactly the null-dated one")
        self.assertEqual(
            dates.epoch_dated, 2, "null and zero both render as 1 Jan 1970"
        )
        self.assertEqual(dates.epoch_stamped_source_messages, 1)
        self.assertEqual(
            dates.epoch_dated_chats_with_indexed_messages,
            2,
            "the field counts epoch-dated chats, which is what its name says",
        )

    def test_reports_no_epoch_dated_chats_when_the_index_holds_no_messages(self):
        conn = memory_db()
        add_item(conn, b"\x01", "chat", created_at_ms=None, modified_at_ms=None)

        dates = probe.measure_dates(conn)

        self.assertEqual(dates.indexed_messages, 0)
        self.assertEqual(dates.epoch_dated_chats_with_indexed_messages, 0)

    def test_ignores_tombstoned_directories(self):
        conn = memory_db()
        add_item(
            conn,
            b"\x01",
            "chat",
            created_at_ms=None,
            modified_at_ms=None,
            deleted_at_ms=50,
        )

        self.assertEqual(probe.measure_dates(conn).live_directories, 0)


class RollupMeasurementTests(unittest.TestCase):
    def test_scopes_the_rollup_to_the_kinds_that_own_one(self):
        # A chat list holds chats, not correspondence. It is deliberately
        # left NULL by the v16 backfill and by the projection, so counting it
        # here would report a permanent, correct absence as a missing rollup.
        conn = memory_db()
        add_item(conn, b"\x01", "chat_list", aggregate_size=None)
        add_item(conn, b"\x02", "chat", aggregate_size=300)
        add_item(conn, b"\x03", "month_dir", parent=b"\x02", aggregate_size=300)
        add_item(
            conn,
            b"\x04",
            "attachment",
            parent=b"\x03",
            directory=False,
            logical_size=300,
        )

        rollup = probe.measure_rollup(conn)

        self.assertEqual(rollup.directories, 2, "the chat list is not counted")
        self.assertEqual(rollup.with_published_size, 2)
        self.assertEqual(rollup.mismatched, 0)
        self.assertEqual(rollup.published_bytes, 600)
        self.assertEqual(rollup.claimed_without_owning_a_rollup, 0)

    def test_counts_a_directory_that_claims_a_rollup_it_does_not_own(self):
        # The regression this pins: a chat list published `0` where the v16
        # SQL left NULL. Zero is not a smaller answer than "nothing is
        # claimed here", it is the claim that the subtree is empty — and the
        # subtree in question held terabytes.
        conn = memory_db()
        add_item(conn, b"\x01", "chat_list", aggregate_size=0)
        add_item(conn, b"\x02", "folder_catalog", aggregate_size=0)
        add_item(conn, b"\x03", "chat", aggregate_size=0)
        add_item(conn, b"\x04", "chat_list", aggregate_size=0, deleted_at_ms=50)
        add_item(conn, b"\x05", "attachment", directory=False, logical_size=7)

        rollup = probe.measure_rollup(conn)

        self.assertEqual(
            rollup.claimed_without_owning_a_rollup,
            2,
            "only live directories outside the rollup kinds count: a chat's"
            " own zero is a real sum, a tombstone is invisible, and a file's"
            " logical size is not a rollup at all",
        )

    def test_flags_a_rollup_that_disagrees_with_its_indexed_descendants(self):
        conn = memory_db()
        add_item(conn, b"\x02", "chat", aggregate_size=999)
        add_item(
            conn,
            b"\x03",
            "attachment",
            parent=b"\x02",
            directory=False,
            logical_size=300,
        )

        rollup = probe.measure_rollup(conn)

        self.assertEqual(rollup.mismatched, 1)
        self.assertEqual(rollup.exact_against_descendants, 0)

    def test_reports_nothing_published_before_the_column_exists(self):
        # A v15 profile — the pre-migration shape the `before` phase reads.
        conn = sqlite3.connect(":memory:")
        conn.row_factory = sqlite3.Row
        conn.executescript(SCHEMA.replace("aggregate_size INTEGER,", ""))
        conn.execute(
            "INSERT INTO items (item_id, account_id, namespace_version, kind,"
            " is_directory) VALUES (X'02', 7, 1, 'chat', 1)"
        )

        rollup = probe.measure_rollup(conn)

        self.assertEqual(rollup.directories, 1)
        self.assertEqual(rollup.with_published_size, 0)
        self.assertEqual(rollup.published_bytes, 0)


class ConvergenceTests(unittest.TestCase):
    def seed(self, phase, retry_at_ms=None, last_backfill_at_ms=None):
        conn = memory_db()
        conn.execute("INSERT INTO chats VALUES (7, 1, -5, NULL, 0)")
        conn.execute("INSERT INTO chat_list_entries VALUES (7, 1, -5)")
        conn.execute(
            "INSERT INTO chat_sync_state VALUES (7, 1, -5, 10, 20, 0, 900, ?1)",
            (last_backfill_at_ms,),
        )
        if phase is not None:
            conn.execute(
                "INSERT INTO chat_content_progress VALUES (7, 1, -5, ?1, ?2)",
                (phase, retry_at_ms),
            )
        return conn

    def test_a_self_fenced_chat_is_reachable_now_and_was_not_before(self):
        # This gap is the starvation the fix removed, and the probe exists to
        # measure it: `degraded` is the engine's own re-crawl fence, not a
        # source refusal.
        conn = self.seed("degraded")

        convergence = probe.measure_convergence(conn, now_ms=1_000)

        self.assertEqual(convergence.listed_incomplete, 1)
        self.assertEqual(convergence.reachable_incomplete, 1)
        self.assertEqual(convergence.unreachable_incomplete, 0)
        self.assertEqual(convergence.reachable_under_prior_predicate, 0)

    def test_an_unexpired_retry_deadline_keeps_a_degraded_chat_out(self):
        conn = self.seed("degraded", retry_at_ms=5_000)

        self.assertEqual(
            probe.measure_convergence(conn, now_ms=1_000).reachable_incomplete, 0
        )
        self.assertEqual(
            probe.measure_convergence(conn, now_ms=9_000).reachable_incomplete, 1
        )

    def test_a_genuine_source_refusal_stays_unreachable(self):
        for phase in ("unavailable", "failed", "protected"):
            with self.subTest(phase=phase):
                conn = self.seed(phase)
                convergence = probe.measure_convergence(conn, now_ms=1_000)
                self.assertEqual(convergence.reachable_incomplete, 0)
                self.assertEqual(convergence.unreachable_incomplete, 1)

    def test_a_protected_or_deleted_chat_is_not_counted_as_incomplete(self):
        conn = memory_db()
        conn.execute("INSERT INTO chats VALUES (7, 1, -5, NULL, 1)")
        conn.execute("INSERT INTO chats VALUES (7, 1, -6, 50, 0)")
        conn.executemany(
            "INSERT INTO chat_list_entries VALUES (7, 1, ?1)", [(-5,), (-6,)]
        )
        conn.executemany(
            "INSERT INTO chat_sync_state VALUES (7, 1, ?1, 10, 20, 0, 900, NULL)",
            [(-5,), (-6,)],
        )

        self.assertEqual(
            probe.measure_convergence(conn, now_ms=1_000).listed_incomplete, 0
        )

    def test_counts_the_chats_still_waiting_for_their_first_backfill_turn(self):
        # After the v17 migration every incomplete chat starts here — the
        # one-guaranteed-turn-each repair — and the number drains as the
        # rotation runs. It is a progress reading, not a pass/fail.
        waiting = probe.measure_convergence(self.seed("pending"), now_ms=1_000)
        self.assertEqual(waiting.never_given_a_backfill_turn, 1)

        turned = probe.measure_convergence(
            self.seed("pending", last_backfill_at_ms=500), now_ms=1_000
        )
        self.assertEqual(turned.never_given_a_backfill_turn, 0)

    def test_reports_no_turn_reading_on_a_profile_predating_the_column(self):
        conn = sqlite3.connect(":memory:")
        conn.row_factory = sqlite3.Row
        conn.executescript(SCHEMA.replace(", last_backfill_at_ms INTEGER", ""))

        self.assertIsNone(
            probe.measure_convergence(conn, now_ms=1_000).never_given_a_backfill_turn,
            "a schema without the column recorded no turns; 0 would read as"
            " 'every chat has had one'",
        )


class CursorComparisonTests(unittest.TestCase):
    def test_a_widening_window_advances_and_a_shrinking_one_regresses(self):
        previous = {"a": [100, 200, 0], "b": [100, 200, 0], "c": [100, 200, 0]}
        current = {
            "a": [90, 210, 0],  # older backward, newer forward
            "b": [110, 200, 0],  # oldest moved forward: work was lost
            "c": [100, 190, 0],  # newest moved backward: work was lost
        }

        result = probe.compare_cursors(previous, current)

        self.assertEqual(result["advanced"], 1)
        self.assertEqual(result["regressed"], 2)
        self.assertFalse(result["monotonic"])

    def test_a_vanished_cursor_is_not_monotonic(self):
        result = probe.compare_cursors({"a": [1, 2, 0]}, {})

        self.assertEqual(result["missing"], 1)
        self.assertFalse(result["monotonic"])

    def test_losing_completion_is_a_regression_and_gaining_it_is_counted(self):
        self.assertEqual(
            probe.compare_cursors({"a": [1, 2, 1]}, {"a": [1, 2, 0]})["regressed"], 1
        )
        gained = probe.compare_cursors({"a": [1, 2, 0]}, {"a": [1, 2, 1]})
        self.assertEqual(gained["newly_complete"], 1)
        self.assertTrue(gained["monotonic"])

    def test_separates_backward_crawling_from_live_forward_movement(self):
        # The starvation signature: a live-active chat whose newest bound
        # keeps moving while its oldest never does has received messages, not
        # history. Counting it as "advanced" is what made the defect
        # invisible in aggregate.
        previous = {
            "starved": [100, 200, 0, 1],  # live-active, incomplete
            "crawling": [100, 200, 0, 1],  # live-active, incomplete
            "quiet": [100, 200, 0, 0],  # not live-active
        }
        current = {
            "starved": [100, 260, 0, 1],  # newer only: delivery, no history
            "crawling": [40, 260, 0, 1],  # older backward: a real turn
            "quiet": [40, 200, 0, 0],
        }

        result = probe.compare_cursors(previous, current)

        self.assertEqual(result["advanced"], 3, "all three moved somehow")
        self.assertEqual(result["crawled_backward"], 2)
        self.assertEqual(result["live_active_incomplete_compared"], 2)
        self.assertEqual(
            result["live_active_crawled_backward"],
            1,
            "only the chat whose oldest bound moved gained history",
        )
        self.assertTrue(result["monotonic"])

    def test_an_unmoved_cursor_is_monotonic_without_advancing(self):
        result = probe.compare_cursors({"a": [1, 2, 0]}, {"a": [1, 2, 0]})

        self.assertEqual(result["advanced"], 0)
        self.assertEqual(result["regressed"], 0)
        self.assertTrue(result["monotonic"])


class PhaseReportingTests(unittest.TestCase):
    def run_phase(self, phase, directory):
        state = Path(directory) / "state.sqlite3"
        seeded = sqlite3.connect(state)
        seeded.executescript(SCHEMA)
        seeded.execute("PRAGMA user_version = 16")
        seeded.commit()
        seeded.close()
        out = Path(directory) / f"{phase}.json"
        code = probe.run(
            phase,
            state,
            out,
            Path(directory) / "private",
            now_ms=1_000,
        )
        return code, json.loads(out.read_text(encoding="utf-8"))

    def test_a_phase_that_asserts_nothing_reports_null_rather_than_passed(self):
        # `"passed": true` beside `"checks": {}` is quotable as "the pre-fix
        # build passed", which it neither did nor was asked to.
        with tempfile.TemporaryDirectory() as directory:
            code, evidence = self.run_phase("before", directory)

        self.assertEqual(code, 0, "the baseline phase is not a failure")
        self.assertEqual(evidence["checks"], {})
        self.assertIsNone(evidence["passed"])

    def test_an_asserting_phase_reports_its_verdict_and_exit_code(self):
        with tempfile.TemporaryDirectory() as directory:
            code, evidence = self.run_phase("after", directory)

        # An empty namespace has no directories, so the "every directory
        # publishes a rollup" check is false and the run exits nonzero.
        self.assertFalse(evidence["checks"]["rollup_published_for_every_directory"])
        self.assertIs(evidence["passed"], False)
        self.assertEqual(code, 1)


if __name__ == "__main__":
    unittest.main()
