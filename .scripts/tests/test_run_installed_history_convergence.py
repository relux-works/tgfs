#!/usr/bin/env python3
"""Tests for privacy-safe installed history convergence evidence."""

from __future__ import annotations

import importlib.util
import sqlite3
import struct
import sys
import tempfile
import unittest
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = (
    REPO_ROOT / ".scripts" / "acceptance" / "run_installed_history_convergence.py"
)


def load_runner():
    spec = importlib.util.spec_from_file_location(
        "run_installed_history_convergence", RUNNER_PATH
    )
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


history = load_runner()


def month_appearance(
    account: int, namespace: int, chat: int, year: int, month: int
) -> bytes:
    return bytes((1, 0x10, 1, 0x0C)) + struct.pack(
        ">qIqHB", account, namespace, chat, year, month
    )


class IdentityDecoderTests(unittest.TestCase):
    def test_decodes_main_and_folder_month_appearances(self):
        main = month_appearance(7, 3, -99, 2024, 2)
        self.assertEqual(
            history.decode_month_appearance(main), ("7:3:-99", 2024, 2)
        )
        folder = bytes((1, 0x10, 3)) + struct.pack(">i", 44) + main[3:]
        self.assertEqual(
            history.decode_month_appearance(folder), ("7:3:-99", 2024, 2)
        )

    def test_rejects_non_month_and_malformed_keys(self):
        self.assertIsNone(history.decode_month_appearance(b""))
        self.assertIsNone(
            history.decode_month_appearance(bytes((1, 0x10, 1, 0x03)) + bytes(20))
        )


class SnapshotTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.db.executescript(
            """
            CREATE TABLE accounts (
                account_id INTEGER PRIMARY KEY, auth_state TEXT,
                namespace_version INTEGER, display_timezone TEXT
            );
            CREATE TABLE chats (
                account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
                deleted_at_ms INTEGER
            );
            CREATE TABLE chat_list_entries (
                account_id INTEGER, namespace_version INTEGER, chat_id INTEGER
            );
            CREATE TABLE messages (
                account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
                sent_at_ms INTEGER
            );
            CREATE TABLE items (
                item_id BLOB PRIMARY KEY, account_id INTEGER,
                namespace_version INTEGER, kind TEXT, parent_item_id BLOB,
                safe_name TEXT, logical_size INTEGER, content_version TEXT,
                deleted_at_ms INTEGER
            );
            CREATE TABLE cache_entries (
                item_id BLOB, kind TEXT, size INTEGER, content_version TEXT,
                verification TEXT
            );
            CREATE TABLE render_state (
                item_id BLOB PRIMARY KEY, dirty INTEGER
            );
            CREATE TABLE chat_sync_state (
                account_id INTEGER, namespace_version INTEGER, chat_id INTEGER,
                oldest_loaded_message_id INTEGER, newest_loaded_message_id INTEGER,
                history_complete INTEGER
            );
            INSERT INTO accounts VALUES (7, 'authorized', 1, 'UTC');
            INSERT INTO chats VALUES (7, 1, 100, NULL);
            INSERT INTO chats VALUES (7, 1, 200, NULL);
            INSERT INTO chat_list_entries VALUES (7, 1, 100);
            INSERT INTO messages VALUES (7, 1, 100, 1704067200000);
            INSERT INTO messages VALUES (7, 1, 200, 1704067200000);
            INSERT INTO chat_sync_state VALUES (7, 1, 100, 10, 50, 0);
            INSERT INTO chat_sync_state VALUES (7, 1, 200, 10, 50, 0);
            """
        )
        month = month_appearance(7, 1, 100, 2024, 1)
        self.db.execute(
            "INSERT INTO items VALUES (?,7,1,'month_dir',NULL,'2024-01',NULL,NULL,NULL)",
            (month,),
        )
        for index, name in enumerate(("Messages.md", "Messages.ndjson"), start=1):
            item = bytes((index,))
            self.db.execute(
                "INSERT INTO items VALUES (?,7,1,'generated_doc',?, ?,10,'v1',NULL)",
                (item, month, name),
            )
            self.db.execute(
                "INSERT INTO cache_entries VALUES (?,'generated_doc',10,'v1','verified')",
                (item,),
            )
            self.db.execute(
                "INSERT INTO render_state VALUES (?,0)",
                (item,),
            )
        self.db.execute(
            "INSERT INTO items VALUES (x'99',7,1,'chat',NULL,'chat',NULL,NULL,NULL)"
        )

    def tearDown(self):
        self.db.close()

    def test_counts_only_listed_chat_and_requires_two_truthful_exports(self):
        facts = history.snapshot(self.db)
        self.assertEqual(facts["eligible_keys"], ["7:1:100"])
        self.assertEqual(facts["source_history_keys"], ["7:1:100"])
        self.assertEqual(facts["old_projected_keys"], ["7:1:100"])
        self.assertEqual(facts["truthful_generated_keys"], ["7:1:100"])
        self.assertEqual(facts["source_month_keys"], {"7:1:100": [(2024, 1)]})
        self.assertEqual(
            facts["published_source_month_keys"], [("7:1:100", 2024, 1)]
        )
        self.assertEqual(facts["full_coverage_keys"], [])
        self.assertEqual(facts["incomplete_cursor_count"], 1)
        self.assertEqual(facts["anchored_cursor_count"], 1)

        self.db.execute(
            "DELETE FROM cache_entries WHERE item_id=x'02'"
        )
        facts = history.snapshot(self.db)
        self.assertEqual(facts["old_projected_keys"], ["7:1:100"])
        self.assertEqual(facts["truthful_generated_keys"], [])
        self.assertEqual(facts["published_source_month_keys"], [])


class MonotonicityAndPrivacyTests(unittest.TestCase):
    def test_cpu_time_parser_accepts_ps_clock_shapes(self):
        self.assertEqual(history._cpu_time_seconds("02.50"), 2.5)
        self.assertEqual(history._cpu_time_seconds("1:02.50"), 62.5)
        self.assertEqual(history._cpu_time_seconds("2:01:02.50"), 7_262.5)

    def test_cursor_comparison_accepts_older_growth_and_rejects_regression(self):
        before = {"a": [50, 100, 0], "b": [None, None, 0]}
        after = {"a": [20, 100, 1], "b": [10, 40, 0]}
        self.assertEqual(history.compare_cursors(before, after), (0, 0, 2))
        self.assertEqual(
            history.compare_cursors(before, {"a": [60, 100, 0]}), (1, 1, 0)
        )

    def test_public_schema_allows_only_flat_typed_aggregates(self):
        evidence = {field: 0 for field in history.PUBLIC_FIELDS}
        evidence["phase"] = "after"
        for field in history.PUBLIC_BOOLEAN_FIELDS:
            evidence[field] = True
        history.validate_public(evidence)
        evidence["eligible_chat_count"] = "private-chat-id"
        with self.assertRaises(history.AcceptanceFailure):
            history.validate_public(evidence)


if __name__ == "__main__":
    unittest.main()
