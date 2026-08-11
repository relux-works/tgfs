#!/usr/bin/env python3
"""Regression tests for installed live-content acceptance comparisons."""

from __future__ import annotations

import importlib.util
import sqlite3
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = REPO_ROOT / ".scripts/acceptance/run_installed_live_content.py"


def load_runner_module():
    spec = importlib.util.spec_from_file_location("run_installed_live_content", RUNNER_PATH)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


live = load_runner_module()


class CandidateSelectionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.cloud = self.root / "cloud"
        self.cloud.mkdir()
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.executescript(
            """
            CREATE TABLE items (
                item_id BLOB PRIMARY KEY,
                parent_item_id BLOB,
                safe_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                availability TEXT NOT NULL DEFAULT 'local',
                logical_size INTEGER NOT NULL DEFAULT 0,
                deleted_at_ms INTEGER
            );
            CREATE TABLE cache_entries (
                item_id BLOB PRIMARY KEY,
                verification TEXT NOT NULL
            );
            """
        )

    def add_item(
        self,
        value: int,
        parent: int | None,
        name: str,
        kind: str,
        *,
        availability: str = "local",
        size: int = 0,
    ):
        self.db.execute(
            "INSERT INTO items VALUES (?, ?, ?, ?, ?, ?, NULL)",
            (
                bytes([value]),
                bytes([parent]) if parent is not None else None,
                name,
                kind,
                availability,
                size,
            ),
        )

    def seed_date_first_tree(self):
        self.add_item(1, None, "Account", "account")
        self.add_item(2, 1, "Chats", "chat_list")
        self.add_item(3, 2, "Chat", "chat")
        self.add_item(5, 3, ".chat.json", "chat_json", size=2)
        self.add_item(6, 3, "2026-07", "month_dir")
        self.add_item(7, 6, "Messages.md", "messages_markdown", size=2)
        self.add_item(8, 6, "Messages.ndjson", "messages_ndjson", size=2)
        self.add_item(
            9,
            6,
            "sample.bin",
            "attachment",
            availability="fetchable",
            size=4,
        )
        path = self.cloud / "Chats" / "Chat" / "2026-07"
        path.mkdir(parents=True)
        (path / "sample.bin").write_bytes(b"test")
        self.db.commit()

    def test_already_cached_item_cannot_false_pass_as_a_placeholder(self):
        self.seed_date_first_tree()
        self.db.execute(
            "INSERT INTO cache_entries VALUES (?, 'verified')", (bytes([9]),)
        )
        self.db.commit()
        with self.assertRaises(live.AcceptanceFailure) as caught:
            live.select_uncached_dataless_candidate(
                self.db, self.cloud, dataless_probe=lambda _path: True
            )
        self.assertEqual(str(caught.exception), "no-fresh-uncached-dataless-placeholder")

    def test_uncached_candidate_must_also_have_the_finder_dataless_flag(self):
        self.seed_date_first_tree()
        with self.assertRaises(live.AcceptanceFailure):
            live.select_uncached_dataless_candidate(
                self.db, self.cloud, dataless_probe=lambda _path: False
            )

    def test_placeholder_read_failure_uses_a_fixed_actionable_label(self):
        with self.assertRaises(live.AcceptanceFailure) as caught:
            live.read_placeholder_once(self.root / "missing-placeholder")
        self.assertEqual(
            str(caught.exception), "placeholder-hydration-read-failed"
        )


class GeneratedDocumentContractTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.cloud = self.root / "cloud"
        self.cloud.mkdir()
        self.cache = self.root / "cache"
        self.cache.mkdir()
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.executescript(
            """
            CREATE TABLE items (
                item_id BLOB PRIMARY KEY,
                parent_item_id BLOB,
                safe_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                mime_type TEXT,
                logical_size INTEGER,
                content_version TEXT,
                created_at_ms INTEGER,
                modified_at_ms INTEGER,
                deleted_at_ms INTEGER
            );
            CREATE TABLE cache_entries (
                item_id BLOB PRIMARY KEY,
                content_version TEXT NOT NULL,
                size INTEGER NOT NULL,
                verification TEXT NOT NULL,
                materialization_ref TEXT NOT NULL
            );
            """
        )
        rows = (
            (1, None, "Account", "account"),
            (2, 1, "Chats", "chat_list"),
            (3, 2, "Chat", "chat"),
            (4, 3, "2026-07", "month_dir"),
        )
        for value, parent, name, kind in rows:
            self.db.execute(
                "INSERT INTO items VALUES (?, ?, ?, ?, NULL, NULL, NULL, NULL, NULL, NULL)",
                (
                    bytes([value]),
                    bytes([parent]) if parent is not None else None,
                    name,
                    kind,
                ),
            )
        self.ids = (bytes([5]), bytes([6]), bytes([7]))
        documents = (
            (5, 4, "Messages.md", "text/markdown", b"# Messages\n"),
            (
                6,
                4,
                "Messages.ndjson",
                "application/x-ndjson",
                b'{"schema":"gramdrive.messages"}\n',
            ),
            (
                7,
                3,
                ".chat.json",
                "application/json",
                b'{"schema":"gramdrive.chat"}\n',
            ),
        )
        for value, parent, name, mime, payload in documents:
            version = f"v{value}"
            materialization = self.cache / f"{value}-{name}"
            materialization.write_bytes(payload)
            self.db.execute(
                "INSERT INTO items VALUES (?, ?, ?, 'generated_doc', ?, ?, ?, ?, ?, NULL)",
                (
                    bytes([value]),
                    bytes([parent]),
                    name,
                    mime,
                    len(payload),
                    version,
                    10,
                    20,
                ),
            )
            self.db.execute(
                "INSERT INTO cache_entries VALUES (?, ?, ?, 'verified', ?)",
                (bytes([value]), version, len(payload), str(materialization)),
            )
            finder = live.item_path(self.db, self.cloud, bytes([value]))
            finder.parent.mkdir(parents=True, exist_ok=True)
            finder.write_bytes(payload)
        self.db.commit()

    def test_exact_bytes_truthful_metadata_and_stable_dates_are_private_records(self):
        first = live.verify_generated_documents(self.db, self.cloud, self.ids)
        replay = live.verify_generated_documents(self.db, self.cloud, self.ids)
        self.assertEqual(first, replay)
        self.assertEqual(len(first), 3)
        self.assertTrue(all(record["logical_size"] > 0 for record in first))
        self.assertTrue(all(record["content_version"] for record in first))
        self.assertTrue(all(record["modified_at_ms"] == 20 for record in first))

    def test_finder_byte_divergence_fails_with_fixed_privacy_safe_label(self):
        finder = live.item_path(self.db, self.cloud, self.ids[-1])
        finder.write_bytes(b"wrong")
        with self.assertRaises(live.AcceptanceFailure) as caught:
            live.verify_generated_documents(self.db, self.cloud, self.ids)
        self.assertEqual(str(caught.exception), "generated-exact-bytes-mismatch")


class NamespaceContractTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.executescript(
            """
            CREATE TABLE items (
                item_id BLOB PRIMARY KEY,
                parent_item_id BLOB,
                safe_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                deleted_at_ms INTEGER
            );
            INSERT INTO items VALUES
                (x'01', NULL, 'Chats', 'chat_list', NULL),
                (x'03', x'01', 'Zero', 'chat', NULL),
                (x'04', x'03', '.chat.json', 'generated_doc', NULL),
                (x'10', x'01', 'Active', 'chat', NULL),
                (x'11', x'10', '.chat.json', 'generated_doc', NULL),
                (x'12', x'10', 'Active Stories', 'active_stories', NULL),
                (x'13', x'12', 'Story.jpg', 'story_appearance', NULL),
                (x'20', x'01', 'Persistent', 'chat', NULL),
                (x'21', x'20', '.chat.json', 'generated_doc', NULL),
                (x'22', x'20', '2026-07', 'month_dir', NULL),
                (x'23', x'22', 'Story.jpg', 'story_appearance', NULL);
            """
        )

    def test_hidden_metadata_and_story_container_truth_are_aggregated(self):
        facts = live.namespace_facts(self.db)
        self.assertEqual(facts.visible_chat_count, 3)
        self.assertEqual(facts.hidden_metadata_count, 3)
        self.assertEqual(facts.legacy_metadata_count, 0)
        self.assertTrue(facts.hidden_metadata_complete)
        self.assertEqual(facts.zero_story_chat_count, 1)
        self.assertTrue(facts.zero_story_containers_omitted)
        self.assertEqual(facts.nonempty_story_chat_count, 2)
        self.assertEqual(facts.active_story_chat_count, 1)
        self.assertEqual(facts.active_story_container_count, 1)
        self.assertTrue(facts.story_containers_truthful)

    def test_legacy_metadata_and_empty_story_containers_fail_their_gates(self):
        self.db.executescript(
            """
            INSERT INTO items VALUES
                (x'05', x'03', 'chat.json', 'generated_doc', NULL),
                (x'24', x'20', 'Active Stories', 'active_stories', NULL),
                (x'06', x'03', 'Active Stories', 'active_stories', NULL);
            """
        )
        facts = live.namespace_facts(self.db)
        self.assertEqual(facts.legacy_metadata_count, 1)
        self.assertFalse(facts.zero_story_containers_omitted)
        self.assertEqual(facts.empty_active_container_count, 2)
        self.assertFalse(facts.story_containers_truthful)


class GeneratedStorageContractTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.generated = self.root / "cache/generated"
        self.generated.mkdir(parents=True)
        (self.root / "agent").mkdir()
        (self.root / "agent/settings.json").write_text(
            '{"cacheQuotaBytes": 1024}\n'
        )
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.executescript(
            """
            CREATE TABLE cache_entries (
                item_id BLOB PRIMARY KEY,
                kind TEXT NOT NULL,
                size INTEGER NOT NULL,
                verification TEXT NOT NULL,
                materialization_ref TEXT
            );
            """
        )

    def add_current(self, value: int, name: str, payload: bytes) -> Path:
        path = self.generated / str(value) / "current" / name
        path.parent.mkdir(parents=True)
        path.write_bytes(payload)
        self.db.execute(
            "INSERT INTO cache_entries VALUES (?, 'generated_doc', ?, 'verified', ?)",
            (bytes([value]), len(payload), str(path)),
        )
        self.db.commit()
        return path

    def test_current_materializations_are_preserved_and_orphans_are_counted(self):
        current = self.add_current(1, "chat.json", b"{}\n")
        orphan = self.generated / "1/stale/chat.json"
        orphan.parent.mkdir(parents=True)
        orphan.write_bytes(b'{"stale":true}\n')

        facts = live.verify_generated_storage(self.db, self.root)

        self.assertEqual(facts.current_reference_count, 1)
        self.assertEqual(facts.physical_file_count, 2)
        self.assertEqual(facts.orphan_file_count, 1)
        self.assertTrue(facts.within_quota)
        self.assertTrue(facts.current_materializations_preserved)
        self.assertTrue(current.exists())

    def test_missing_current_or_physical_bytes_over_quota_fail_their_gates(self):
        current = self.add_current(1, "Messages.md", b"x" * 16)
        (self.root / "agent/settings.json").write_text('{"cacheQuotaBytes": 8}\n')
        current.unlink()

        facts = live.verify_generated_storage(self.db, self.root)

        self.assertTrue(facts.within_quota)
        self.assertEqual(facts.physical_bytes, 0)
        self.assertFalse(facts.current_materializations_preserved)

        orphan = self.generated / "orphan/Messages.ndjson"
        orphan.parent.mkdir(parents=True)
        orphan.write_bytes(b"x" * 16)
        facts = live.verify_generated_storage(self.db, self.root)
        self.assertFalse(facts.within_quota)
        self.assertEqual(facts.orphan_file_count, 1)


class RelaunchComparisonTests(unittest.TestCase):
    def test_public_evidence_rejects_free_form_content_fields(self):
        with self.assertRaises(live.AcceptanceFailure):
            live.validate_public_evidence(
                {"privacy_safe": True, "chat_name": "must-not-persist"}
            )

    def test_item_count_and_set_are_compared_to_the_before_snapshot(self):
        stable = live.compare_items("same", 2, ["a", "b"], "same", 2, ["a", "b"])
        self.assertTrue(stable.count_stable)
        self.assertTrue(stable.set_stable)
        self.assertEqual(stable.count_delta, 0)
        self.assertTrue(stable.prior_items_preserved)
        self.assertTrue(stable.additive_only)

        changed = live.compare_items(
            "before", 2, ["a", "b"], "after", 3, ["a", "b", "c"]
        )
        self.assertFalse(changed.count_stable)
        self.assertFalse(changed.set_stable)
        self.assertEqual(changed.count_delta, 1)
        self.assertTrue(changed.prior_items_preserved)
        self.assertTrue(changed.additive_only)

        regressed = live.compare_items(
            "before", 2, ["a", "b"], "after", 2, ["a", "c"]
        )
        self.assertFalse(regressed.prior_items_preserved)
        self.assertFalse(regressed.additive_only)

    def test_false_stability_booleans_cannot_exit_as_a_pass(self):
        evidence = {
            "chat_json_present": True,
            "active_stories_present": True,
            "direct_month_present": True,
            "messages_markdown_nonempty": True,
            "messages_ndjson_nonempty": True,
            "sample_uncached_before_enumeration": True,
            "sample_dataless_before_enumeration": True,
            "sample_uncached_after_enumeration": True,
            "sample_dataless_after_enumeration": True,
            "initial_enumeration_materialized_selected_media": False,
            "hydration_count": 1,
            "hydrated_size_matches": True,
            "hydrated_bytes_verified": True,
            "generated_document_open_count": 3,
            "generated_exact_bytes_verified": True,
            "generated_metadata_truthful": True,
            "generated_storage_within_quota": True,
            "generated_current_materializations_preserved": True,
            "generated_orphan_file_count": 0,
            "hidden_chat_metadata_complete": True,
            "legacy_chat_metadata_absent": True,
            "zero_story_chat_count": 1,
            "zero_story_containers_omitted": True,
            "nonempty_story_chat_count": 1,
            "story_containers_truthful": True,
            "relaunch_item_count_stable": False,
            "relaunch_item_set_stable": False,
            "relaunch_prior_item_identity_preserved": True,
            "relaunch_item_set_additive_only": True,
            "relaunch_item_identity_stable": True,
            "relaunch_cursor_progress_preserved": True,
            "relaunch_retention_preserved": True,
            "relaunch_hydration_preserved": True,
            "generated_relaunch_exact_bytes_verified": True,
            "generated_metadata_stable": True,
            "generated_dates_stable": True,
        }
        self.assertTrue(live.evidence_passed("after", evidence))

        evidence["relaunch_prior_item_identity_preserved"] = False
        self.assertFalse(live.evidence_passed("after", evidence))

    def test_equal_and_monotonically_progressed_cursors_are_preserved(self):
        before = [
            [1, 1, 10, 100, 200, 0],
            [1, 1, 11, None, None, 0],
            [1, 1, 12, 50, 60, 1],
        ]
        after = [
            [1, 1, 10, 90, 210, 1],
            [1, 1, 11, None, None, 0],
            [1, 1, 12, 50, 60, 1],
            [1, 1, 13, 70, 80, 0],
        ]
        result = live.compare_cursors(before, after)
        self.assertTrue(result.preserved)
        self.assertEqual(result.before_count, 3)
        self.assertEqual(result.after_count, 4)
        self.assertEqual(result.progressed_count, 1)

    def test_missing_or_regressed_cursor_fails_preservation(self):
        before = [
            [1, 1, 10, 100, 200, 1],
            [1, 1, 11, 50, 60, 0],
        ]
        after = [[1, 1, 10, 110, 190, 0]]
        result = live.compare_cursors(before, after)
        self.assertFalse(result.preserved)
        self.assertEqual(result.missing_count, 1)
        self.assertEqual(result.regressed_count, 1)


class StabilitySnapshotTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.database = self.root / "gramdrive.sqlite3"
        self.state = self.root / "private-state.json"
        self.evidence = self.root / "evidence.json"
        db = sqlite3.connect(self.database)
        db.executescript(
            """
            CREATE TABLE items (
                item_id BLOB PRIMARY KEY,
                deleted_at_ms INTEGER
            );
            CREATE TABLE cache_entries (
                item_id BLOB PRIMARY KEY,
                blob_hash BLOB NOT NULL,
                size INTEGER NOT NULL,
                verification TEXT NOT NULL,
                materialized_at_ms INTEGER NOT NULL
            );
            CREATE TABLE chat_sync_state (
                account_id INTEGER NOT NULL,
                namespace_version INTEGER NOT NULL,
                chat_id INTEGER NOT NULL,
                oldest_loaded_message_id INTEGER,
                newest_loaded_message_id INTEGER,
                history_complete INTEGER NOT NULL
            );
            CREATE TABLE accounts (
                auth_state TEXT NOT NULL,
                retention_mode TEXT NOT NULL,
                archive_mode INTEGER NOT NULL
            );
            CREATE TABLE story_appearances (location TEXT NOT NULL);
            INSERT INTO accounts VALUES ('authorized', 'mirror', 0);
            INSERT INTO items VALUES (x'01', NULL), (x'02', NULL);
            """
        )
        db.commit()
        db.close()
        self.original_digest = bytes.fromhex("11" * 32)
        self.unrelated_digest = bytes.fromhex("22" * 32)
        live.write_json(
            self.state,
            {
                "sample_item": "01",
                "expected_size": 4,
                "hydrated_digest": self.original_digest.hex(),
                "generated_records": [],
            },
        )
        live.write_json(self.evidence, {"privacy_safe": True})

    def add_cache_rows(
        self,
        *,
        include_original: bool = True,
        original_size: int = 4,
        original_digest: bytes | None = None,
    ):
        db = sqlite3.connect(self.database)
        if include_original:
            db.execute(
                "INSERT INTO cache_entries VALUES (?, ?, ?, 'verified', ?)",
                (
                    bytes([1]),
                    original_digest or self.original_digest,
                    original_size,
                    100,
                ),
            )
        db.execute(
            "INSERT INTO cache_entries VALUES (?, ?, ?, 'verified', ?)",
            (bytes([2]), self.unrelated_digest, 8, 200),
        )
        db.commit()
        db.close()

    def run_snapshot(self):
        with mock.patch.object(live, "QUIESCENCE_STABLE_POLLS", 1):
            return live.run_stability_snapshot(
                self.database, self.state, self.evidence
            )

    def test_newer_unrelated_cache_row_does_not_replace_original_sample(self):
        self.add_cache_rows()
        result = self.run_snapshot()
        private = live.json.loads(self.state.read_text())
        self.assertEqual(result["quiescence_stable_poll_count"], 1)
        self.assertEqual(private["sample_item"], "01")
        self.assertEqual(private["expected_size"], 4)
        self.assertEqual(private["hydrated_digest"], self.original_digest.hex())

    def test_missing_original_cache_row_fails(self):
        self.add_cache_rows(include_original=False)
        with self.assertRaises(live.AcceptanceFailure) as caught:
            self.run_snapshot()
        self.assertEqual(
            str(caught.exception), "hydrated-sample-cache-entry-missing"
        )

    def test_mismatched_original_size_or_digest_fails(self):
        cases = (
            (5, self.original_digest, "hydrated-sample-size-mismatch"),
            (4, bytes.fromhex("33" * 32), "hydrated-sample-digest-mismatch"),
        )
        for index, (size, digest, expected) in enumerate(cases):
            with self.subTest(expected=expected):
                if index:
                    db = sqlite3.connect(self.database)
                    db.execute("DELETE FROM cache_entries")
                    db.commit()
                    db.close()
                self.add_cache_rows(
                    original_size=size, original_digest=digest
                )
                with self.assertRaises(live.AcceptanceFailure) as caught:
                    self.run_snapshot()
                self.assertEqual(str(caught.exception), expected)


if __name__ == "__main__":
    unittest.main()
