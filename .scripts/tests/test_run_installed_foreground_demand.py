#!/usr/bin/env python3
"""Tests for the installed foreground-demand probe (BUG-260728-2qfzbd).

This probe is the instrument the "an ordinary Finder open advances the chat"
claim is read off, so the properties worth pinning are the ones a wrong
reading would hide: that it picks a chat the background rotation is *not*
about to reach anyway, that it refuses chats with no folder on the domain,
that a turn taken during the control window disqualifies the result rather
than being ignored, and that a turn stamp alone is not reported as backward
progress.
"""

from __future__ import annotations

import importlib.util
import json
import sqlite3
import struct
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = REPO_ROOT / ".scripts" / "acceptance" / "run_installed_foreground_demand.py"


def load_runner():
    spec = importlib.util.spec_from_file_location(
        "run_installed_foreground_demand", RUNNER_PATH
    )
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


probe = load_runner()

#: The v17 shape, reduced to the columns this probe reads.
SCHEMA = """
CREATE TABLE chat_sync_state (
    account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
    oldest_loaded_message_id INTEGER, newest_loaded_message_id INTEGER,
    history_complete INTEGER, last_sync_at_ms INTEGER,
    last_backfill_at_ms INTEGER
);
CREATE TABLE chats (
    account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
    deleted_at_ms INTEGER, is_protected INTEGER
);
CREATE TABLE chat_list_entries (
    account_id INTEGER, namespace_version INTEGER, chat_id INTEGER
);
CREATE TABLE messages (
    account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
    message_id INTEGER
);
CREATE TABLE chat_content_progress (
    account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
    phase TEXT, retry_at_ms INTEGER
);
CREATE TABLE items (
    item_id BLOB PRIMARY KEY, parent_item_id BLOB, account_id INTEGER,
    namespace_version INTEGER, kind TEXT, view_kind TEXT, safe_name TEXT,
    deleted_at_ms INTEGER, logical_size INTEGER
);
"""

ACCOUNT_ITEM = b"\x01\x01account"
CHATS_ITEM = b"\x01\x02chats"


def seed(conn: sqlite3.Connection, chats) -> None:
    """`chats` is a list of (chat_id, safe_name, last_backfill_at_ms, oldest).

    The item rows are laid out exactly as the projection lays them out — an
    account root, one `Chats` list directory, and one chat appearance under it
    keyed by the frozen v1 identifier — because the probe resolves the folder by
    walking that chain, not by reading a chat id off the row.
    """
    conn.executescript(SCHEMA)
    conn.execute(
        "INSERT INTO items VALUES (?, NULL, 1, 1, 'account', NULL, 'account', NULL, NULL)",
        (ACCOUNT_ITEM,),
    )
    conn.execute(
        "INSERT INTO items VALUES (?, ?, 1, 1, 'chat_list', 'main', 'Chats', NULL, NULL)",
        (CHATS_ITEM, ACCOUNT_ITEM),
    )
    for chat_id, safe_name, last_backfill, oldest in chats:
        conn.execute(
            "INSERT INTO chat_sync_state VALUES (1, 1, ?, ?, 900000, 0, 5000, ?)",
            (chat_id, oldest, last_backfill),
        )
        conn.execute("INSERT INTO chats VALUES (1, 1, ?, NULL, 0)", (chat_id,))
        conn.execute("INSERT INTO chat_list_entries VALUES (1, 1, ?)", (chat_id,))
        conn.execute(
            "INSERT INTO items VALUES (?, ?, 1, 1, 'chat', 'main', ?, NULL, NULL)",
            (probe.chat_appearance_item_id(1, 1, chat_id), CHATS_ITEM, safe_name),
        )
    conn.commit()


class ChoiceTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.mount = self.root / "mount"
        (self.mount / "Chats").mkdir(parents=True)
        self.conn = sqlite3.connect(":memory:")
        self.conn.row_factory = sqlite3.Row

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_picks_the_chat_the_rotation_reaches_last(self) -> None:
        """The whole attribution rests on this: if the probe picked the chat at
        the head of the backlog, an advance would prove nothing, because the
        rotation was about to crawl it regardless."""
        seed(
            self.conn,
            [(10, "soon", 1_000_000, 500), (20, "last", 5_000_000, 500)],
        )
        for name in ("soon", "last"):
            (self.mount / "Chats" / name).mkdir()
        scope, folder = probe.choose_chat(self.conn, self.mount, 9_000_000)
        self.assertEqual(scope, (1, 1, 20))
        self.assertEqual(folder.name, "last")

    def test_skips_a_chat_that_could_still_be_the_active_crawl(self) -> None:
        """The scheduler stamps a turn when it hands the work out and then keeps
        the crawl for several ticks, so the newest stamp is usually the chat
        being crawled right now. Measuring that chat reads its in-flight
        progress as an effect of the open — the exact false pass this exclusion
        prevents."""
        seed(
            self.conn,
            [(10, "idle", 1_000_000, 500), (20, "in-flight", 8_999_000, 500)],
        )
        for name in ("idle", "in-flight"):
            (self.mount / "Chats" / name).mkdir()
        scope, folder = probe.choose_chat(self.conn, self.mount, 9_000_000)
        self.assertEqual(folder.name, "idle")
        self.assertEqual(scope, (1, 1, 10))

        # Once that chat's turn is old enough to be over, it is measurable
        # again — the exclusion is a grace window, not a permanent skip.
        self.assertEqual(
            probe.choose_chat(
                self.conn, self.mount, 8_999_000 + probe.ACTIVE_CRAWL_GRACE_MS
            )[0],
            (1, 1, 20),
        )

    def test_skips_a_chat_with_no_folder_on_the_domain(self) -> None:
        seed(
            self.conn,
            [(10, "present", 1_000_000, 500), (20, "absent", 5_000_000, 500)],
        )
        (self.mount / "Chats" / "present").mkdir()
        scope, folder = probe.choose_chat(self.conn, self.mount, 9_000_000)
        self.assertEqual(scope, (1, 1, 10))
        self.assertEqual(folder.name, "present")

    def test_skips_complete_and_unreachable_chats(self) -> None:
        seed(self.conn, [(10, "reachable", 1_000_000, 500), (20, "done", 5_000_000, 500)])
        self.conn.execute(
            "UPDATE chat_sync_state SET history_complete = 1 WHERE chat_id = 20"
        )
        self.conn.execute(
            "INSERT INTO chat_content_progress VALUES (1, 1, 10, 'degraded', 20_000_000)"
        )
        self.conn.commit()
        for name in ("reachable", "done"):
            (self.mount / "Chats" / name).mkdir()

        # Chat 10 is self-fenced until 20_000_000 and chat 20 is complete, so at
        # 9_000_000 there is nothing measurable at all.
        self.assertEqual(
            probe.choose_chat(self.conn, self.mount, 9_000_000), (None, None)
        )
        # Once the fence expires chat 10 becomes the only candidate.
        self.assertEqual(
            probe.choose_chat(self.conn, self.mount, 25_000_000)[0], (1, 1, 10)
        )


class IdentifierTests(unittest.TestCase):
    """The probe rebuilds the chat's item identifier instead of looking it up,
    so the frozen v1 layout is pinned here against a real identifier read off an
    installed profile. If the encoding ever changes, this fails loudly rather
    than the probe silently finding no candidate."""

    #: A real main-view chat appearance: v1, appearance tag, main list, chat
    #: tag, then account 816078, namespace 1, chat -2073002527137.
    GOLDEN = bytes.fromhex("0110010300000000000c73ce00000001fffffe1d576bb65f")

    def test_matches_a_real_installed_identifier(self) -> None:
        self.assertEqual(
            probe.chat_appearance_item_id(816_078, 1, -2_073_002_527_137),
            self.GOLDEN,
        )
        self.assertEqual(len(self.GOLDEN), probe.CHAT_APPEARANCE_LEN)

    def test_each_field_is_part_of_the_identifier(self) -> None:
        base = probe.chat_appearance_item_id(816_078, 1, -2_073_002_527_137)
        for other in (
            probe.chat_appearance_item_id(816_079, 1, -2_073_002_527_137),
            probe.chat_appearance_item_id(816_078, 2, -2_073_002_527_137),
            probe.chat_appearance_item_id(816_078, 1, -2_073_002_527_138),
        ):
            self.assertNotEqual(base, other)


class FolderResolutionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.mount = Path(self.temp.name)
        self.conn = sqlite3.connect(":memory:")
        self.conn.row_factory = sqlite3.Row
        seed(self.conn, [(10, "Some Chat", 1_000, 500)])

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_path_is_the_parent_chain_without_the_account_root(self) -> None:
        resolved = probe.folder_path(
            self.conn, probe.chat_appearance_item_id(1, 1, 10), self.mount
        )
        self.assertEqual(resolved, self.mount / "Chats" / "Some Chat")

    def test_a_deleted_item_resolves_to_nothing(self) -> None:
        self.conn.execute(
            "UPDATE items SET deleted_at_ms = 1 WHERE safe_name = 'Some Chat'"
        )
        self.assertIsNone(
            probe.folder_path(
                self.conn, probe.chat_appearance_item_id(1, 1, 10), self.mount
            )
        )

    def test_an_unknown_identifier_resolves_to_nothing(self) -> None:
        self.assertIsNone(
            probe.folder_path(
                self.conn, probe.chat_appearance_item_id(1, 1, 999), self.mount
            )
        )


class ComparisonTests(unittest.TestCase):
    def reading(self, oldest, last_backfill, complete=0):
        return probe.Reading(
            oldest=oldest,
            newest=900_000,
            history_complete=complete,
            last_backfill_at_ms=last_backfill,
            last_sync_at_ms=5_000,
        )

    def test_a_turn_and_backward_progress_are_reported_separately(self) -> None:
        """A turn that crawled nothing is not the claim being made, so the
        frontier delta has to be visible next to the turn flag rather than
        folded into it."""
        stalled = probe.compare(
            self.reading(500, 1_000), self.reading(500, 2_000), 45.0
        )
        self.assertTrue(stalled.took_a_turn)
        self.assertEqual(stalled.frontier_moved_back_by, 0)

        crawled = probe.compare(
            self.reading(500, 1_000), self.reading(380, 2_000), 45.0
        )
        self.assertTrue(crawled.took_a_turn)
        self.assertEqual(crawled.frontier_moved_back_by, 120)

    def test_live_delivery_alone_is_not_a_turn(self) -> None:
        """`last_sync_at_ms` moves on every incoming message. Reading the turn
        off that column is exactly the defect schema v17 removed, so the probe
        must not repeat it."""
        before = self.reading(500, 1_000)
        after = probe.Reading(
            oldest=500,
            newest=900_050,
            history_complete=0,
            last_backfill_at_ms=1_000,
            last_sync_at_ms=99_000,
        )
        self.assertFalse(probe.compare(before, after, 45.0).took_a_turn)

    def test_reaching_completion_is_reported_once(self) -> None:
        crossed = probe.compare(
            self.reading(500, 1_000), self.reading(1, 2_000, complete=1), 45.0
        )
        self.assertTrue(crossed.reached_history_complete)
        already = probe.compare(
            self.reading(500, 1_000, complete=1),
            self.reading(1, 2_000, complete=1),
            45.0,
        )
        self.assertFalse(already.reached_history_complete)


class CrawledCountTests(unittest.TestCase):
    """Telegram server ids are not messages — the raw `oldest` delta is in
    shifted id units and reads about a thousand times larger than the work
    actually done. The evidence has to carry the real count."""

    def setUp(self) -> None:
        self.conn = sqlite3.connect(":memory:")
        self.conn.row_factory = sqlite3.Row
        seed(self.conn, [(10, "Some Chat", 1_000, 500)])
        for message_id in (300, 400, 500, 600):
            self.conn.execute(
                "INSERT INTO messages VALUES (1, 1, 10, ?)", (message_id,)
            )
        self.conn.execute("INSERT INTO messages VALUES (1, 1, 99, 350)")
        self.conn.commit()

    def reading(self, oldest):
        return probe.Reading(
            oldest=oldest,
            newest=900_000,
            history_complete=0,
            last_backfill_at_ms=1_000,
            last_sync_at_ms=5_000,
        )

    def test_counts_only_this_chats_messages_inside_the_move(self) -> None:
        self.assertEqual(
            probe.crawled_backward(
                self.conn, (1, 1, 10), self.reading(500), self.reading(300)
            ),
            3,
            "the 100-unit-wide id delta is 3 real messages, and another chat's "
            "message inside the same id range is not one of them",
        )

    def test_a_frontier_that_did_not_move_crawled_nothing(self) -> None:
        for after in (500, 600):
            self.assertEqual(
                probe.crawled_backward(
                    self.conn, (1, 1, 10), self.reading(500), self.reading(after)
                ),
                0,
            )


class VerdictTests(unittest.TestCase):
    """The acceptance boolean has to be false when the control window already
    moved — otherwise the probe would credit the open for a turn the rotation
    handed out on its own."""

    def verdict(self, control_turn: bool, open_turn: bool) -> bool:
        control = probe.Window(
            seconds=45.0,
            took_a_turn=control_turn,
            frontier_moved_back_by=100 if control_turn else 0,
            messages_crawled_backward=7 if control_turn else 0,
            reached_history_complete=False,
        )
        opened = probe.Window(
            seconds=45.0,
            took_a_turn=open_turn,
            frontier_moved_back_by=100 if open_turn else 0,
            messages_crawled_backward=7 if open_turn else 0,
            reached_history_complete=False,
        )
        return opened.took_a_turn and not control.took_a_turn

    def test_only_an_open_attributable_turn_passes(self) -> None:
        self.assertTrue(self.verdict(control_turn=False, open_turn=True))
        self.assertFalse(self.verdict(control_turn=False, open_turn=False))
        self.assertFalse(self.verdict(control_turn=True, open_turn=True))
        self.assertFalse(self.verdict(control_turn=True, open_turn=False))


class EnumerationTests(unittest.TestCase):
    def test_enumeration_reads_one_level_and_returns_its_width(self) -> None:
        """The trigger has to be the same shallow readdir Finder issues when a
        folder is opened. A recursive walk would materialize the whole subtree
        and stop being an ordinary open."""
        with tempfile.TemporaryDirectory() as name:
            folder = Path(name)
            (folder / "2026-07").mkdir()
            (folder / "2026-06").mkdir()
            (folder / "2026-06" / "deep.md").write_text("x")
            self.assertEqual(probe.enumerate_folder(folder), 2)


def add_document(
    conn: sqlite3.Connection,
    item_id: bytes,
    parent: bytes,
    safe_name: str,
    logical_size: int | None,
    kind: str = "generated_doc",
    deleted: int | None = None,
) -> None:
    conn.execute(
        "INSERT INTO items VALUES (?, ?, 1, 1, ?, NULL, ?, ?, ?)",
        (item_id, parent, kind, safe_name, deleted, logical_size),
    )
    conn.commit()


class DocumentChoiceTests(unittest.TestCase):
    """The read gesture is only honest if it reads something the agent renders
    from the index it already has. An attachment would download Telegram
    payload bytes to measure a scheduling claim, which the bug's scope
    forbids."""

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.mount = self.root / "mount"
        self.chat = self.mount / "Chats" / "Some Chat"
        (self.chat / "2026-07").mkdir(parents=True)
        self.conn = sqlite3.connect(":memory:")
        self.conn.row_factory = sqlite3.Row
        seed(self.conn, [(10, "Some Chat", 1_000, 500)])
        self.chat_item = probe.chat_appearance_item_id(1, 1, 10)
        self.month_item = b"month-2026-07"
        add_document(
            self.conn, self.month_item, self.chat_item, "2026-07", None, kind="month_dir"
        )

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_prefers_the_chat_level_document(self) -> None:
        """A month's `Messages.md` grows with the chat; `.chat.json` does not.
        Depth first, then size, so the cheapest read wins."""
        add_document(self.conn, b"doc-chat", self.chat_item, ".chat.json", 259)
        add_document(self.conn, b"doc-month", self.month_item, "Messages.md", 12)
        (self.chat / ".chat.json").write_text("{}")
        (self.chat / "2026-07" / "Messages.md").write_text("x")
        self.assertEqual(
            probe.choose_document(self.conn, self.chat_item, self.mount),
            self.chat / ".chat.json",
        )

    def test_falls_back_to_the_smallest_document_inside_a_month(self) -> None:
        add_document(self.conn, b"doc-big", self.month_item, "messages.ndjson", 900)
        add_document(self.conn, b"doc-small", self.month_item, "Messages.md", 30)
        (self.chat / "2026-07" / "messages.ndjson").write_text("x")
        (self.chat / "2026-07" / "Messages.md").write_text("x")
        self.assertEqual(
            probe.choose_document(self.conn, self.chat_item, self.mount),
            self.chat / "2026-07" / "Messages.md",
        )

    def test_an_attachment_is_never_chosen(self) -> None:
        add_document(
            self.conn, b"att", self.month_item, "photo.jpg", 4, kind="attachment"
        )
        (self.chat / "2026-07" / "photo.jpg").write_text("x")
        self.assertIsNone(probe.choose_document(self.conn, self.chat_item, self.mount))

    def test_a_document_missing_from_the_domain_is_not_chosen(self) -> None:
        add_document(self.conn, b"doc-chat", self.chat_item, ".chat.json", 259)
        self.assertIsNone(probe.choose_document(self.conn, self.chat_item, self.mount))

    def test_a_tombstoned_document_is_not_chosen(self) -> None:
        add_document(
            self.conn, b"doc-chat", self.chat_item, ".chat.json", 259, deleted=1
        )
        (self.chat / ".chat.json").write_text("{}")
        self.assertIsNone(probe.choose_document(self.conn, self.chat_item, self.mount))

    def test_reading_returns_the_bytes_it_read(self) -> None:
        (self.chat / ".chat.json").write_text('{"a": 1}')
        self.assertEqual(
            probe.read_document(self.chat / ".chat.json"), {"document_bytes_read": 8}
        )

    def test_a_read_that_fails_is_reported_rather_than_raised(self) -> None:
        """The demand is raised when the fetch resolves its item, before any
        byte moves, so a fetch the system gives up on has still delivered the
        hint. Crashing here would throw away a measurable window and hide the
        fact that no bytes arrived."""
        import errno

        result = probe.read_document(self.chat / "not-materialized.json")
        self.assertEqual(result["document_bytes_read"], 0)
        self.assertEqual(result["document_read_failed_errno"], errno.ENOENT)


class HintCounterTests(unittest.TestCase):
    """Without the receiving end, "the chat did not advance" cannot be
    attributed to the provider not sending or the agent not honoring."""

    def test_counters_are_read_from_a_status_answer(self) -> None:
        import socket as socketlib
        import threading

        with tempfile.TemporaryDirectory() as name:
            path = Path(name) / "control.sock"
            server = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
            server.bind(str(path))
            server.listen(1)
            received: list[bytes] = []

            def serve() -> None:
                connection, _ = server.accept()
                with connection:
                    received.append(connection.recv(4096))
                    connection.sendall(
                        json.dumps(
                            {
                                "event": "status",
                                "status": {
                                    "historyPriorityHints": {
                                        "accepted": 4,
                                        "requested": 3,
                                        "background": 1,
                                        "visible": 0,
                                        "unroutable": 0,
                                    }
                                },
                            }
                        ).encode()
                        + b"\n"
                    )

            thread = threading.Thread(target=serve)
            thread.start()
            counts = probe.agent_hint_counts(path)
            thread.join()
            server.close()

        self.assertEqual(counts["accepted"], 4)
        self.assertEqual(json.loads(received[0])["operation"], "status")

    def test_a_path_too_long_for_sun_path_still_connects(self) -> None:
        """The real endpoint lives under the group container, whose path is
        longer than `sun_path` holds. Connecting by leaf from inside the
        directory is how the agent's own clients reach it; without this the
        counters would read as unobservable on every installed run."""
        import socket as socketlib
        import threading

        with tempfile.TemporaryDirectory() as name:
            deep = Path(name)
            while len(str(deep / "control.sock").encode()) <= 104:
                deep = deep / "Library Group Container Application Support"
            deep.mkdir(parents=True)
            path = deep / "control.sock"
            previous = Path.cwd()
            import os

            os.chdir(deep)
            server = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
            server.bind("control.sock")
            os.chdir(previous)
            server.listen(1)

            def serve() -> None:
                connection, _ = server.accept()
                with connection:
                    connection.recv(4096)
                    connection.sendall(
                        json.dumps(
                            {
                                "event": "status",
                                "status": {"historyPriorityHints": {"accepted": 1}},
                            }
                        ).encode()
                        + b"\n"
                    )

            thread = threading.Thread(target=serve)
            thread.start()
            counts = probe.agent_hint_counts(path)
            thread.join()
            server.close()

        self.assertEqual(counts, {"accepted": 1})
        self.assertEqual(Path.cwd(), previous, "the probe must not move the process")

    def test_an_absent_agent_is_reported_as_unobserved(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            self.assertIsNone(probe.agent_hint_counts(Path(name) / "nothing.sock"))
        self.assertEqual(probe.hint_delta(None, {"accepted": 2}), {"observed": False})

    def test_a_delta_counts_only_what_arrived_across_the_gesture(self) -> None:
        delta = probe.hint_delta(
            {"accepted": 10, "requested": 4, "background": 6, "visible": 0, "unroutable": 1},
            {"accepted": 12, "requested": 5, "background": 7, "visible": 0, "unroutable": 1},
        )
        self.assertTrue(delta["observed"])
        self.assertEqual(delta["accepted_delta"], 2)
        self.assertEqual(delta["requested_delta"], 1)
        self.assertEqual(delta["background_delta"], 1)
        self.assertEqual(delta["unroutable_delta"], 0)


class DemandProbeFixture(unittest.TestCase):
    """A seeded state file and a fake mount, with the observation pauses
    replaced by a callback that plays the agent's part. Carries no tests of its
    own — the gesture suites below drive it."""

    # Deliberately not a small number: the leak assertion below looks for the
    # id as a substring, and a two-digit id would collide with an ordinary count.
    CHAT = 216_133_163
    SAFE_NAME = "Katerina Averina — private notes"

    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.state = self.root / "gramdrive.sqlite3"
        self.mount = self.root / "mount"
        (self.mount / "Chats" / self.SAFE_NAME).mkdir(parents=True)
        (self.mount / "Chats" / self.SAFE_NAME / "2026-07").mkdir()
        conn = sqlite3.connect(self.state)
        seed(conn, [(10, "head-of-rotation", 1_000, 500), (self.CHAT, self.SAFE_NAME, 9_000, 500)])
        for message_id in (390, 420, 480):
            conn.execute(
                "INSERT INTO messages VALUES (1, 1, ?, ?)", (self.CHAT, message_id)
            )
        conn.commit()
        (self.mount / "Chats" / "head-of-rotation").mkdir()
        conn.close()
        self.out = self.root / "evidence.json"
        self.private = self.root / "private" / "detail.json"

    def tearDown(self) -> None:
        self.temp.cleanup()

    def advance_chosen_chat(self) -> None:
        """What a real backfill turn does to the row: a fresh turn stamp and a
        lower backward frontier."""
        conn = sqlite3.connect(self.state)
        conn.execute(
            "UPDATE chat_sync_state SET last_backfill_at_ms = last_backfill_at_ms + 1000,"
            " oldest_loaded_message_id = oldest_loaded_message_id - 120"
            " WHERE chat_id = ?",
            (self.CHAT,),
        )
        conn.commit()
        conn.close()

    def pause_advancing_on(self, window_index: int):
        calls = []

        def pause(_seconds: float) -> None:
            if len(calls) == window_index:
                self.advance_chosen_chat()
            calls.append(None)

        return pause


class EndToEndTests(DemandProbeFixture):
    """The folder-open gesture: kept measured because the claim it disproves —
    that opening a chat folder prioritises it — is one a reader would otherwise
    assume. This is what pins the verdict, the exit code, and the fact that the
    public artifact carries no identity."""

    def test_a_turn_taken_only_after_the_open_passes_and_leaks_nothing(self) -> None:
        code = probe.run(
            self.state,
            self.mount,
            0.0,
            self.out,
            self.private,
            pause=self.pause_advancing_on(1),
            gesture="open",
        )
        self.assertEqual(code, 0)
        evidence = json.loads(self.out.read_text())
        self.assertTrue(evidence["foreground_open_granted_a_turn"])
        self.assertFalse(evidence["control"]["took_a_turn"])
        self.assertTrue(evidence["after_gesture"]["took_a_turn"])
        self.assertEqual(evidence["after_gesture"]["frontier_moved_back_by"], 120)
        self.assertEqual(
            evidence["after_gesture"]["messages_crawled_backward"],
            3,
            "the reported work is the messages indexed, not the id delta",
        )
        self.assertEqual(evidence["folder_entries_enumerated"], 1)

        rendered = self.out.read_text()
        self.assertNotIn(self.SAFE_NAME, rendered)
        self.assertNotIn(str(self.CHAT), rendered)
        self.assertNotIn(str(self.mount), rendered)
        # The identity is kept, but only in the private file.
        self.assertEqual(json.loads(self.private.read_text())["chat_id"], self.CHAT)

    def test_a_turn_taken_during_the_control_window_fails_the_probe(self) -> None:
        """The rotation reaching the chat on its own must not be credited to
        the open; that reading is exactly the false pass this probe exists to
        prevent."""
        code = probe.run(
            self.state,
            self.mount,
            0.0,
            self.out,
            self.private,
            pause=self.pause_advancing_on(0),
            gesture="open",
        )
        self.assertEqual(code, 1)
        evidence = json.loads(self.out.read_text())
        self.assertFalse(evidence["foreground_open_granted_a_turn"])
        self.assertTrue(evidence["control"]["took_a_turn"])

    def test_a_chat_that_never_moves_fails_the_probe(self) -> None:
        code = probe.run(
            self.state,
            self.mount,
            0.0,
            self.out,
            self.private,
            pause=lambda _: None,
            gesture="open",
        )
        self.assertEqual(code, 1)
        self.assertFalse(
            json.loads(self.out.read_text())["foreground_open_granted_a_turn"]
        )


class ReadGestureEndToEndTests(DemandProbeFixture):
    """The same shape driven through the gesture the acceptance boolean is
    actually read off: a content read, which is the one interaction that
    reliably reaches the extension."""

    DOCUMENT = ".chat.json"

    def setUp(self) -> None:
        super().setUp()
        conn = sqlite3.connect(self.state)
        add_document(
            conn,
            b"doc-chat",
            probe.chat_appearance_item_id(1, 1, self.CHAT),
            self.DOCUMENT,
            259,
        )
        conn.close()
        (self.mount / "Chats" / self.SAFE_NAME / self.DOCUMENT).write_text(
            '{"title": "private"}'
        )

    def stub_hints(self, before: dict | None, after: dict | None):
        readings = [before, after]

        def hints(_socket):
            return readings.pop(0) if readings else after

        return hints

    def test_a_turn_taken_only_after_the_read_passes_and_leaks_nothing(self) -> None:
        code = probe.run(
            self.state,
            self.mount,
            0.0,
            self.out,
            self.private,
            pause=self.pause_advancing_on(1),
            gesture="read",
            hints=self.stub_hints(
                {"accepted": 8, "requested": 3, "background": 3, "visible": 2, "unroutable": 0},
                {"accepted": 10, "requested": 4, "background": 4, "visible": 2, "unroutable": 0},
            ),
        )
        self.assertEqual(code, 0)
        evidence = json.loads(self.out.read_text())
        self.assertTrue(evidence["content_read_granted_a_turn"])
        self.assertEqual(evidence["gesture"], "read")
        self.assertEqual(evidence["document_bytes_read"], 20)
        self.assertFalse(evidence["control"]["took_a_turn"])
        self.assertTrue(evidence["after_gesture"]["took_a_turn"])
        # The receiving end saw the raise and its release, which is what
        # separates "the provider never sent one" from "the agent ignored it".
        self.assertEqual(evidence["hints_delivered_to_the_agent"]["accepted_delta"], 2)
        self.assertEqual(evidence["hints_delivered_to_the_agent"]["requested_delta"], 1)

        rendered = self.out.read_text()
        self.assertNotIn(self.SAFE_NAME, rendered)
        self.assertNotIn(str(self.CHAT), rendered)
        self.assertNotIn(str(self.mount), rendered)
        self.assertEqual(
            json.loads(self.private.read_text())["document"],
            str(self.mount / "Chats" / self.SAFE_NAME / self.DOCUMENT),
        )

    def test_a_turn_taken_during_the_control_window_fails_the_read_probe(self) -> None:
        code = probe.run(
            self.state,
            self.mount,
            0.0,
            self.out,
            self.private,
            pause=self.pause_advancing_on(0),
            gesture="read",
            hints=lambda _socket: None,
        )
        self.assertEqual(code, 1)
        evidence = json.loads(self.out.read_text())
        self.assertFalse(evidence["content_read_granted_a_turn"])
        self.assertFalse(evidence["hints_delivered_to_the_agent"]["observed"])

    def test_a_chat_with_no_generated_document_is_reported_unmeasured(self) -> None:
        """Not a failure: there was nothing to read, so nothing was measured.
        Reporting it as a failed claim would be a false negative."""
        conn = sqlite3.connect(self.state)
        conn.execute("DELETE FROM items WHERE kind = 'generated_doc'")
        conn.commit()
        conn.close()
        code = probe.run(
            self.state,
            self.mount,
            0.0,
            self.out,
            self.private,
            pause=lambda _: None,
            gesture="read",
            hints=lambda _socket: None,
        )
        self.assertEqual(code, 0)
        evidence = json.loads(self.out.read_text())
        self.assertFalse(evidence["measured"])
        self.assertNotIn("content_read_granted_a_turn", evidence)


class SocketLocationTests(unittest.TestCase):
    def test_the_control_endpoint_sits_beside_the_state_file(self) -> None:
        self.assertEqual(
            probe.control_socket(Path("/data/GramDrive/state/gramdrive.sqlite3")),
            Path("/data/GramDrive/agent/control.sock"),
        )


if __name__ == "__main__":
    unittest.main()
