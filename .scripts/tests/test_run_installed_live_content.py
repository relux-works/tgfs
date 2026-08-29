#!/usr/bin/env python3
"""Regression tests for installed live-content acceptance comparisons."""

from __future__ import annotations

import importlib.util
import hashlib
import io
import json
import os
import resource
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock
from pathlib import Path


REPO_ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = Path(
    os.environ.get(
        "GRAMDRIVE_LIVE_CONTENT_RUNNER",
        REPO_ROOT / ".scripts/acceptance/run_installed_live_content.py",
    )
)


def load_runner_module():
    spec = importlib.util.spec_from_file_location(
        "run_installed_live_content", RUNNER_PATH
    )
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

    def add_second_attachment(self):
        self.add_item(
            10,
            6,
            "second.bin",
            "attachment",
            availability="fetchable",
            size=8,
        )
        (self.cloud / "Chats" / "Chat" / "2026-07" / "second.bin").write_bytes(
            b"second"
        )
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
        self.assertEqual(
            str(caught.exception), "no-fresh-uncached-dataless-placeholder"
        )

    def test_uncached_candidate_must_also_have_the_finder_dataless_flag(self):
        self.seed_date_first_tree()
        with self.assertRaises(live.AcceptanceFailure):
            live.select_uncached_dataless_candidate(
                self.db, self.cloud, dataless_probe=lambda _path: False
            )

    def test_selection_skips_one_timed_out_placeholder_within_the_probe_bound(self):
        self.seed_date_first_tree()
        self.add_second_attachment()
        outcomes = iter(
            (
                live.PlaceholderProbeResult(live.PlaceholderState.TIMEOUT, 500),
                live.PlaceholderProbeResult(live.PlaceholderState.DATALESS, 2),
            )
        )

        candidate, _path, facts = live.select_uncached_dataless_candidate(
            self.db, self.cloud, dataless_probe=lambda _path: next(outcomes)
        )

        self.assertEqual(candidate.item_id, bytes([10]))
        self.assertEqual(facts.candidates_considered, 2)
        self.assertEqual(facts.stat_timeout_count, 1)
        self.assertEqual(facts.dataless_count, 1)

    def test_selection_never_probes_a_twenty_first_candidate(self):
        self.seed_date_first_tree()
        for value in range(10, 30):
            self.add_item(
                value,
                6,
                f"candidate-{value}.bin",
                "attachment",
                availability="fetchable",
                size=value - 5,
            )
        self.db.commit()
        calls = 0

        def dataless_only_after_the_bound(_path):
            nonlocal calls
            calls += 1
            state = (
                live.PlaceholderState.DATALESS
                if calls == 21
                else live.PlaceholderState.MATERIALIZED
            )
            return live.PlaceholderProbeResult(state, 1)

        with self.assertRaises(live.AcceptanceFailure) as caught:
            live.select_uncached_dataless_candidate(
                self.db, self.cloud, dataless_probe=dataless_only_after_the_bound
            )

        self.assertEqual(str(caught.exception), "bounded-placeholder-selection-exhausted")
        self.assertEqual(calls, 20)
        self.assertEqual(
            caught.exception.public_evidence["placeholder_candidates_considered"],
            20,
        )

    def test_selection_resolves_one_exact_missing_item_then_requires_dataless(self):
        self.seed_date_first_tree()
        probes = iter(
            (
                live.PlaceholderProbeResult(live.PlaceholderState.MISSING, 1),
                live.PlaceholderProbeResult(live.PlaceholderState.DATALESS, 2),
            )
        )
        resolved = []

        candidate, _path, facts = live.select_uncached_dataless_candidate(
            self.db,
            self.cloud,
            dataless_probe=lambda _path: next(probes),
            materialize_missing=lambda item_id: resolved.append(item_id) or True,
        )

        self.assertEqual(candidate.item_id, bytes([9]))
        self.assertEqual(resolved, [bytes([9])])
        self.assertEqual(facts.candidates_considered, 1)
        self.assertEqual(facts.missing_count, 1)
        self.assertEqual(facts.dataless_count, 1)

    def test_selection_never_launders_resolver_failure_into_dataless(self):
        self.seed_date_first_tree()

        with self.assertRaises(live.AcceptanceFailure) as caught:
            live.select_uncached_dataless_candidate(
                self.db,
                self.cloud,
                dataless_probe=lambda _path: live.PlaceholderProbeResult(
                    live.PlaceholderState.MISSING, 1
                ),
                materialize_missing=lambda _item_id: live.PlaceholderResolveResult(
                    live.PlaceholderResolveState.TIMEOUT, 2000
                ),
            )

        self.assertEqual(str(caught.exception), "bounded-placeholder-selection-exhausted")
        self.assertEqual(
            caught.exception.public_evidence["placeholder_resolve_timeout_count"], 1
        )
        self.assertEqual(
            caught.exception.public_evidence["placeholder_dataless_count"], 0
        )

    def test_selection_reports_identity_mismatch_without_private_identity(self):
        self.seed_date_first_tree()

        with self.assertRaises(live.AcceptanceFailure) as caught:
            live.select_uncached_dataless_candidate(
                self.db,
                self.cloud,
                dataless_probe=lambda _path: live.PlaceholderProbeResult(
                    live.PlaceholderState.MISSING, 1
                ),
                materialize_missing=lambda _item_id: live.PlaceholderResolveResult(
                    live.PlaceholderResolveState.IDENTITY_MISMATCH, 2
                ),
            )

        self.assertEqual(str(caught.exception), "bounded-placeholder-selection-exhausted")
        evidence = caught.exception.public_evidence
        self.assertEqual(evidence["placeholder_resolve_identity_mismatch_count"], 1)
        self.assertEqual(evidence["placeholder_dataless_count"], 0)
        self.assertTrue(set(evidence).issubset(live.PUBLIC_EVIDENCE_FIELDS))

    def test_provider_identifier_is_private_core_golden_text_not_binary_or_hex(self):
        account_42 = bytes.fromhex("0101000000000000002a")
        self.assertEqual(
            live.provider_item_identifier(account_42), "gdaeaqaaaaaaaaaabk"
        )

    def test_installed_resolver_keeps_identifier_off_argv_and_requires_fixed_output(self):
        process = live.WorkerProcessResult(0, "resolved\n", "", False, True, 4)
        with mock.patch.object(
            live, "run_worker_process", return_value=process
        ) as run:
            item_id = bytes.fromhex("0101000000000000002a")
            result = live.finder_resolve_placeholder(item_id)

        self.assertIs(result.state, live.PlaceholderResolveState.RESOLVED)
        command = run.call_args.args[0]
        self.assertEqual(
            command,
            (
                str(live.DEFAULT_COMPANION_EXECUTABLE),
                live.PLACEHOLDER_RESOLVE_COMMAND,
            ),
        )
        self.assertNotIn("gdaeaqaaaaaaaaaabk", " ".join(command))
        self.assertEqual(
            run.call_args.kwargs["stdin_text"], "gdaeaqaaaaaaaaaabk\n"
        )

        process = live.WorkerProcessResult(
            5, "identity-mismatch\n", "", False, True, 4
        )
        with mock.patch.object(live, "run_worker_process", return_value=process):
            mismatch = live.finder_resolve_placeholder(item_id)
        self.assertIs(
            mismatch.state, live.PlaceholderResolveState.IDENTITY_MISMATCH
        )

        process = live.WorkerProcessResult(0, "unexpected\n", "", False, True, 4)
        with mock.patch.object(live, "run_worker_process", return_value=process):
            refused = live.finder_resolve_placeholder(item_id)
        self.assertIs(refused.state, live.PlaceholderResolveState.PLATFORM_ERROR)

        process = live.WorkerProcessResult(-9, "", "", True, True, 4000)
        with mock.patch.object(live, "run_worker_process", return_value=process):
            timed_out = live.finder_resolve_placeholder(item_id)
        self.assertIs(timed_out.state, live.PlaceholderResolveState.TIMEOUT)

        with mock.patch.object(live, "run_worker_process", side_effect=OSError()):
            spawn_failed = live.finder_resolve_placeholder(item_id)
        self.assertIs(spawn_failed.state, live.PlaceholderResolveState.PLATFORM_ERROR)

    def test_candidate_plan_uses_the_bounded_partial_index_without_temp_sort(self):
        self.seed_date_first_tree()
        self.db.execute(
            "CREATE INDEX items_live_fetchable_attachments_by_size "
            "ON items(logical_size, item_id) "
            "WHERE kind='attachment' AND availability='fetchable' "
            "AND deleted_at_ms IS NULL AND logical_size > 0"
        )
        plan = " ".join(
            row[3]
            for row in self.db.execute(
                "EXPLAIN QUERY PLAN " + live.CANDIDATE_QUERY,
                (
                    live.MAX_GENERATED_VERIFICATION_BYTES,
                    live.MAX_CANDIDATES,
                ),
            )
        ).lower()

        self.assertIn("items_live_fetchable_attachments_by_size", plan)
        self.assertNotIn("temp b-tree", plan)

    def test_candidate_query_excludes_an_oversized_generated_document_set(self):
        self.seed_date_first_tree()
        self.db.execute(
            "UPDATE items SET logical_size=? WHERE item_id=?",
            (live.MAX_GENERATED_VERIFICATION_BYTES, bytes([7])),
        )
        self.db.commit()

        self.assertEqual(live.candidate_rows(self.db), [])

    def test_fixed_probe_states_distinguish_missing_platform_error_and_timeout(self):
        completed = subprocess.CompletedProcess(("probe",), 0, "1073741824\n", "")
        with mock.patch.object(live.subprocess, "run", return_value=completed):
            self.assertIs(
                live.finder_placeholder_probe(self.cloud).state,
                live.PlaceholderState.DATALESS,
            )
        completed = subprocess.CompletedProcess(("probe",), 3, "", "")
        with mock.patch.object(live.subprocess, "run", return_value=completed):
            self.assertIs(
                live.finder_placeholder_probe(self.cloud).state,
                live.PlaceholderState.MISSING,
            )
        completed = subprocess.CompletedProcess(("probe",), 4, "", "")
        with mock.patch.object(live.subprocess, "run", return_value=completed):
            self.assertIs(
                live.finder_placeholder_probe(self.cloud).state,
                live.PlaceholderState.PLATFORM_ERROR,
            )
        with mock.patch.object(
            live.subprocess,
            "run",
            side_effect=subprocess.TimeoutExpired(("probe",), 0.5),
        ):
            self.assertIs(
                live.finder_placeholder_probe(self.cloud).state,
                live.PlaceholderState.TIMEOUT,
            )

    def test_probe_read_failures_are_not_laundered_as_placeholder_absence(self):
        malformed = subprocess.CompletedProcess(("probe",), 0, "not-an-integer\n", "")
        with mock.patch.object(live.subprocess, "run", return_value=malformed):
            self.assertIs(
                live.finder_placeholder_probe(self.cloud).state,
                live.PlaceholderState.PLATFORM_ERROR,
            )
        with mock.patch.object(
            live.subprocess, "run", side_effect=OSError("spawn refused")
        ):
            self.assertIs(
                live.finder_placeholder_probe(self.cloud).state,
                live.PlaceholderState.PLATFORM_ERROR,
            )

    @unittest.skipUnless(sys.platform == "darwin", "st_flags is a macOS contract")
    def test_real_probe_child_reports_materialized_and_missing_without_content_reads(self):
        materialized = live.finder_placeholder_probe(self.cloud)
        missing = live.finder_placeholder_probe(self.root / "missing-placeholder")

        self.assertIs(materialized.state, live.PlaceholderState.MATERIALIZED)
        self.assertIs(missing.state, live.PlaceholderState.MISSING)
        self.assertLessEqual(
            materialized.elapsed_ms + missing.elapsed_ms,
            round(live.PLACEHOLDER_PROBE_TIMEOUT_SECONDS * 2000),
        )

    def test_path_remap_fails_closed_as_identity_mismatch(self):
        self.seed_date_first_tree()
        first = self.cloud / "old-placeholder"
        second = self.cloud / "new-placeholder"
        with mock.patch.object(live, "item_path", side_effect=(first, second)):
            with self.assertRaises(live.AcceptanceFailure) as caught:
                live.select_uncached_dataless_candidate(
                    self.db,
                    self.cloud,
                    dataless_probe=lambda _path: live.PlaceholderProbeResult(
                        live.PlaceholderState.DATALESS, 1
                    ),
                )
        self.assertEqual(str(caught.exception), "bounded-placeholder-selection-exhausted")
        self.assertEqual(
            caught.exception.public_evidence["placeholder_path_mismatch_count"], 1
        )

    def test_placeholder_read_failure_uses_a_fixed_actionable_label(self):
        with self.assertRaises(live.AcceptanceFailure) as caught:
            live.read_placeholder_once(self.root / "missing-placeholder")
        self.assertEqual(str(caught.exception), "placeholder-hydration-read-failed")


class BoundedPlaceholderReadTests(unittest.TestCase):
    def test_bounded_child_returns_exact_bytes_for_every_private_path(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            first = root / "first"
            second = root / "second"
            first.write_bytes(b"first")
            second.write_bytes(b"second")

            records = live.read_placeholder_paths_bounded(
                (first, second), 1.0, "generated-document-verification-timeout"
            )

        self.assertEqual(records[0][1], 5)
        self.assertEqual(records[1][1], 6)
        self.assertNotEqual(records[0][0], records[1][0])

    def test_bounded_child_kills_a_blocked_read_and_fails_closed(self):
        prior_children = {child.pid for child in live.multiprocessing.active_children()}
        started = time.monotonic()

        with self.assertRaises(live.AcceptanceFailure) as caught:
            live.read_placeholder_paths_bounded(
                (Path("/dev/zero"),),
                0.01,
                "generated-document-verification-timeout",
            )

        elapsed = time.monotonic() - started
        current_children = {
            child.pid for child in live.multiprocessing.active_children()
        }
        self.assertEqual(
            str(caught.exception), "generated-document-verification-timeout"
        )
        self.assertLess(elapsed, 1.0)
        self.assertEqual(current_children, prior_children)

    def test_bounded_child_does_not_launder_a_read_failure_as_absence(self):
        with tempfile.TemporaryDirectory() as directory:
            missing = Path(directory) / "missing"
            with self.assertRaises(live.AcceptanceFailure) as caught:
                live.read_placeholder_paths_bounded(
                    (missing,), 1.0, "foreground-hydration-timeout"
                )

        self.assertEqual(str(caught.exception), "placeholder-hydration-read-failed")


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
        (self.root / "agent/settings.json").write_text('{"cacheQuotaBytes": 1024}\n')
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
            CREATE INDEX cache_entries_by_materialization_ref
                ON cache_entries(materialization_ref)
                WHERE materialization_ref IS NOT NULL;
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

    def test_physical_inventory_has_an_explicit_entry_bound(self):
        self.add_current(1, "Messages.md", b"one")
        extra = self.generated / "extra"
        extra.mkdir()
        with self.assertRaises(live.AcceptanceFailure) as caught:
            live.verify_generated_storage(self.db, self.root, scan_entry_limit=1)
        self.assertEqual(str(caught.exception), "generated-cache-entry-limit-exceeded")

    def test_verified_generated_row_without_materialization_fails_preservation(self):
        self.db.execute(
            "INSERT INTO cache_entries VALUES"
            "(?, 'generated_doc', 1, 'verified', NULL)",
            (bytes([1]),),
        )
        self.db.commit()
        facts = live.verify_generated_storage(self.db, self.root)
        self.assertEqual(facts.current_reference_count, 1)
        self.assertFalse(facts.current_materializations_preserved)


class IndexedSnapshotContractTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.database = self.root / "live.sqlite3"
        self.snapshot = self.root / "private.snapshot.sqlite3"
        db = sqlite3.connect(self.database)
        db.executescript(
            """
            CREATE TABLE items (
                item_id BLOB PRIMARY KEY,
                deleted_at_ms INTEGER
            );
            CREATE TABLE chat_sync_state (
                account_id INTEGER NOT NULL,
                namespace_version INTEGER NOT NULL,
                chat_id INTEGER NOT NULL,
                oldest_loaded_message_id INTEGER,
                newest_loaded_message_id INTEGER,
                history_complete INTEGER NOT NULL,
                PRIMARY KEY (account_id, namespace_version, chat_id)
            );
            INSERT INTO items VALUES (x'01', NULL), (x'02', NULL);
            INSERT INTO chat_sync_state VALUES
                (1, 1, 10, 100, 200, 0),
                (1, 1, 11, 50, 60, 1);
            """
        )
        db.commit()
        db.close()

    def test_sqlite_sidecar_proves_additions_deletions_and_cursor_monotonicity(self):
        counts = live.create_indexed_snapshot(self.database, self.snapshot)
        self.assertEqual(counts, {"item_count": 2, "cursor_count": 2})
        self.assertEqual(self.snapshot.stat().st_mode & 0o777, 0o600)

        db = sqlite3.connect(self.database)
        db.executescript(
            """
            UPDATE items SET deleted_at_ms=1 WHERE item_id=x'02';
            INSERT INTO items VALUES (x'03', NULL);
            UPDATE chat_sync_state
               SET oldest_loaded_message_id=90,
                   newest_loaded_message_id=210,
                   history_complete=1
             WHERE chat_id=10;
            DELETE FROM chat_sync_state WHERE chat_id=11;
            """
        )
        db.commit()
        db.close()

        check = live.connection(self.database)
        live.attach_snapshot(check, self.snapshot)
        try:
            items = live.compare_items_indexed(check)
            cursors = live.compare_cursors_indexed(check)
        finally:
            check.close()
        self.assertEqual(items.before_count, 2)
        self.assertEqual(items.after_count, 2)
        self.assertFalse(items.prior_items_preserved)
        self.assertFalse(items.additive_only)
        self.assertEqual(cursors.missing_count, 1)
        self.assertEqual(cursors.progressed_count, 1)
        self.assertFalse(cursors.preserved)

    def test_identity_and_cursor_comparisons_use_keyed_point_lookups(self):
        live.create_indexed_snapshot(self.database, self.snapshot)
        check = live.connection(self.database)
        live.attach_snapshot(check, self.snapshot)
        try:
            item_plan = " ".join(
                row[3]
                for row in check.execute(
                    """
                    EXPLAIN QUERY PLAN
                    SELECT count(*)
                    FROM acceptance_snapshot.active_items prior
                    WHERE NOT EXISTS (
                        SELECT 1 FROM items current
                        WHERE current.item_id=prior.item_id
                          AND current.deleted_at_ms IS NULL
                    )
                    """
                )
            )
            cursor_plan = " ".join(
                row[3]
                for row in check.execute(
                    """
                    EXPLAIN QUERY PLAN
                    SELECT 1 FROM acceptance_snapshot.cursors prior
                    JOIN chat_sync_state current
                      ON current.account_id=prior.account_id
                     AND current.namespace_version=prior.namespace_version
                     AND current.chat_id=prior.chat_id
                    """
                )
            )
        finally:
            check.close()
        self.assertIn("item_id=?", item_plan)
        self.assertIn("account_id=? AND namespace_version=? AND chat_id=?", cursor_plan)

    def test_private_json_is_atomic_and_owner_only(self):
        private = self.root / "private.json"
        live.write_private_json(private, {"sample_item": "private"})
        self.assertEqual(private.stat().st_mode & 0o777, 0o600)
        self.assertFalse(private.with_name("private.json.writing").exists())
        self.assertEqual(live.json.loads(private.read_text())["sample_item"], "private")


class InstalledPhaseEndToEndTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.data = self.root / "data"
        self.cloud = self.root / "cloud"
        self.database = self.data / "state/gramdrive.sqlite3"
        self.state = self.root / "private.json"
        self.evidence = self.root / "evidence.json"
        self.database.parent.mkdir(parents=True)
        self.cloud.mkdir()
        (self.data / "agent").mkdir()
        (self.data / "agent/settings.json").write_text('{"cacheQuotaBytes": 1000000}\n')
        self.db = sqlite3.connect(self.database)
        self.addCleanup(self.close_database)
        self.db.executescript(
            """
            CREATE TABLE items (
                item_id BLOB PRIMARY KEY,
                parent_item_id BLOB,
                safe_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                availability TEXT NOT NULL DEFAULT 'local',
                logical_size INTEGER,
                deleted_at_ms INTEGER,
                mime_type TEXT,
                content_version TEXT,
                created_at_ms INTEGER,
                modified_at_ms INTEGER
            );
            CREATE UNIQUE INDEX items_sibling_name
                ON items(parent_item_id, safe_name)
                WHERE parent_item_id IS NOT NULL AND deleted_at_ms IS NULL;
            CREATE TABLE cache_entries (
                item_id BLOB PRIMARY KEY,
                content_version TEXT,
                size INTEGER NOT NULL,
                verification TEXT NOT NULL,
                materialization_ref TEXT,
                kind TEXT NOT NULL,
                blob_hash BLOB
            );
            CREATE INDEX cache_entries_by_materialization_ref
                ON cache_entries(materialization_ref)
                WHERE materialization_ref IS NOT NULL;
            CREATE TABLE chat_sync_state (
                account_id INTEGER NOT NULL,
                namespace_version INTEGER NOT NULL,
                chat_id INTEGER NOT NULL,
                oldest_loaded_message_id INTEGER,
                newest_loaded_message_id INTEGER,
                history_complete INTEGER NOT NULL,
                PRIMARY KEY(account_id, namespace_version, chat_id)
            );
            CREATE TABLE accounts (
                auth_state TEXT NOT NULL,
                retention_mode TEXT NOT NULL,
                archive_mode INTEGER NOT NULL
            );
            CREATE TABLE story_appearances (location TEXT NOT NULL);
            INSERT INTO accounts VALUES('authorized', 'mirror', 0);
            INSERT INTO story_appearances VALUES('active');
            INSERT INTO chat_sync_state VALUES(1, 1, 10, 100, 200, 0);
            """
        )
        self.add_item(1, None, "Account", "account")
        self.add_item(2, 1, "Chats", "chat_list")
        self.add_item(3, 2, "Chat", "chat")
        self.add_item(4, 3, ".chat.json", "generated_doc", payload=b'{"chat":1}\n')
        self.add_item(5, 3, "2026-08", "month_dir")
        self.add_item(6, 5, "Messages.md", "generated_doc", payload=b"# Messages\n")
        self.add_item(
            7,
            5,
            "Messages.ndjson",
            "generated_doc",
            payload=b'{"message":1}\n',
        )
        self.add_item(
            8,
            5,
            "sample.bin",
            "attachment",
            availability="fetchable",
            payload=b"sample",
            generated=False,
        )
        self.add_item(9, 3, "Active Stories", "active_stories")
        self.add_item(10, 9, "Story.jpg", "story_appearance")
        self.add_item(11, 2, "Zero", "chat")
        self.add_item(12, 11, ".chat.json", "generated_doc")
        self.db.commit()
        self.attachment = self.cloud / "Chats/Chat/2026-08/sample.bin"

    def close_database(self):
        try:
            self.db.close()
        except sqlite3.ProgrammingError:
            pass

    def add_item(
        self,
        value: int,
        parent: int | None,
        name: str,
        kind: str,
        *,
        availability: str = "local",
        payload: bytes | None = None,
        generated: bool = True,
    ) -> None:
        item_id = bytes([value])
        parent_id = None if parent is None else bytes([parent])
        mime = {
            "Messages.md": "text/markdown",
            "Messages.ndjson": "application/x-ndjson",
            ".chat.json": "application/json",
        }.get(name)
        version = f"v{value}" if payload is not None and generated else None
        self.db.execute(
            "INSERT INTO items VALUES(?, ?, ?, ?, ?, ?, NULL, ?, ?, 10, 20)",
            (
                item_id,
                parent_id,
                name,
                kind,
                availability,
                None if payload is None else len(payload),
                mime,
                version,
            ),
        )
        if payload is None:
            return
        finder = live.item_path(self.db, self.cloud, item_id)
        finder.parent.mkdir(parents=True, exist_ok=True)
        finder.write_bytes(payload)
        if not generated:
            return
        generated_name = "chat.json" if name == ".chat.json" else name
        materialization = self.data / "cache/generated" / str(value) / generated_name
        materialization.parent.mkdir(parents=True, exist_ok=True)
        materialization.write_bytes(payload)
        self.db.execute(
            "INSERT INTO cache_entries VALUES"
            "(?, ?, ?, 'verified', ?, 'generated_doc', ?)",
            (
                item_id,
                version,
                len(payload),
                str(materialization),
                hashlib.sha256(payload).digest(),
            ),
        )

    def test_before_and_after_preserve_every_contract_without_python_bulk_lists(self):
        original_read = live.read_placeholder_once

        def read_and_publish(path: Path):
            result = original_read(path)
            if path == self.attachment:
                digest, size = result
                publisher = sqlite3.connect(self.database)
                publisher.execute(
                    "INSERT OR REPLACE INTO cache_entries VALUES"
                    "(?, 'attachment-v1', ?, 'verified', ?, 'blob', ?)",
                    (bytes([8]), size, str(path), bytes.fromhex(digest)),
                )
                publisher.commit()
                publisher.close()
            return result

        self.db.close()
        with mock.patch.object(
            live, "read_placeholder_once", side_effect=read_and_publish
        ):
            before = live.run_before(
                self.database,
                self.data,
                self.cloud,
                self.state,
                self.evidence,
                dataless_probe=lambda _path: True,
            )
        self.assertTrue(live.evidence_passed("before", before))
        private = live.json.loads(self.state.read_text())
        self.assertNotIn("item_ids", private)
        self.assertNotIn("cursors", private)
        self.assertTrue(live.snapshot_database_path(self.state).is_file())

        update = sqlite3.connect(self.database)
        update.execute(
            "UPDATE chat_sync_state SET oldest_loaded_message_id=90, "
            "newest_loaded_message_id=210, history_complete=1"
        )
        update.execute(
            "INSERT INTO items VALUES"
            "(x'0D', x'05', 'new.bin', 'attachment', 'fetchable', 1, NULL, "
            "'application/octet-stream', 'new-v1', 10, 20)"
        )
        update.commit()
        update.close()
        after = live.run_after(
            self.database, self.data, self.cloud, self.state, self.evidence
        )
        self.assertTrue(live.evidence_passed("after", after))
        self.assertTrue(after["relaunch_prior_item_identity_preserved"])
        self.assertTrue(after["relaunch_item_set_additive_only"])
        self.assertTrue(after["relaunch_cursor_progress_preserved"])

    def test_large_profile_generated_verification_cannot_starve_foreground_hydration(
        self,
    ):
        class VirtualClock:
            def __init__(self):
                self.value = 1_000.0

            def __call__(self):
                return self.value

            def advance(self, seconds: float):
                self.value += seconds

        large_payload = b"x" * 8_388_608
        large_finder = live.item_path(self.db, self.cloud, bytes([6]))
        large_cache = Path(
            self.db.execute(
                "SELECT materialization_ref FROM cache_entries WHERE item_id=?",
                (bytes([6]),),
            ).fetchone()[0]
        )
        large_finder.write_bytes(large_payload)
        large_cache.write_bytes(large_payload)
        self.db.execute(
            "UPDATE items SET logical_size=? WHERE item_id=?",
            (len(large_payload), bytes([6])),
        )
        self.db.execute(
            "UPDATE cache_entries SET size=?, blob_hash=? WHERE item_id=?",
            (
                len(large_payload),
                hashlib.sha256(large_payload).digest(),
                bytes([6]),
            ),
        )
        (self.data / "agent/settings.json").write_text(
            '{"cacheQuotaBytes": 33554432}\n'
        )

        self.add_item(13, 2, "Bounded", "chat")
        self.add_item(14, 13, ".chat.json", "generated_doc", payload=b'{"chat":2}\n')
        self.add_item(15, 13, "2026-08", "month_dir")
        self.add_item(16, 15, "Messages.md", "generated_doc", payload=b"# Bounded\n")
        self.add_item(
            17,
            15,
            "Messages.ndjson",
            "generated_doc",
            payload=b'{"bounded":true}\n',
        )
        self.add_item(
            18,
            15,
            "bounded.bin",
            "attachment",
            availability="fetchable",
            payload=b"bounded",
            generated=False,
        )
        self.db.commit()

        clock = VirtualClock()
        legacy_generated_finder_paths = {
            live.item_path(self.db, self.cloud, bytes([item_id]))
            for item_id in (4, 6, 7)
        }
        bounded_generated_finder_paths = {
            live.item_path(self.db, self.cloud, bytes([item_id]))
            for item_id in (14, 16, 17)
        }
        bounded_attachment = live.item_path(self.db, self.cloud, bytes([18]))
        attachment_ids = {
            self.attachment: bytes([8]),
            bounded_attachment: bytes([18]),
        }
        foreground_published = False
        legacy_generated_calls = 0
        observed_legacy_generated_seconds = 72.547

        def bounded_large_profile_read(paths, timeout, timeout_category):
            nonlocal foreground_published, legacy_generated_calls
            paths = tuple(paths)
            if len(paths) == 1 and paths[0] in attachment_ids:
                self.assertEqual(timeout, live.FOREGROUND_HYDRATION_TIMEOUT_SECONDS)
                clock.advance(timeout - 0.001)
                records = tuple(live.read_placeholder_once(path) for path in paths)
                digest, size = records[0]
                item_id = attachment_ids[paths[0]]
                publisher = sqlite3.connect(self.database)
                publisher.execute(
                    "INSERT OR REPLACE INTO cache_entries VALUES"
                    "(?, 'attachment-v1', ?, 'verified', ?, 'blob', ?)",
                    (item_id, size, str(paths[0]), bytes.fromhex(digest)),
                )
                publisher.commit()
                publisher.close()
                foreground_published = True
                return records

            self.assertTrue(
                foreground_published,
                "generated verification started before foreground publication",
            )
            self.assertEqual(timeout, live.GENERATED_DOCUMENT_VERIFY_TIMEOUT_SECONDS)
            finder_path = paths[-1]
            if finder_path in legacy_generated_finder_paths:
                legacy_generated_calls += 1
                # The observed generated aggregate was 72.547s. Even granting
                # two reads the largest successful 9.999s duration leaves the
                # third above the production 10s per-document bound.
                if legacy_generated_calls == 3:
                    residual = observed_legacy_generated_seconds - 2 * (
                        timeout - 0.001
                    )
                    self.assertGreater(residual, timeout)
                    clock.advance(timeout)
                    raise live.AcceptanceFailure(timeout_category)
            else:
                self.assertIn(finder_path, bounded_generated_finder_paths)
            clock.advance(timeout - 0.001)
            return tuple(live.read_placeholder_once(path) for path in paths)

        self.db.close()
        with mock.patch.object(
            live.time, "monotonic", side_effect=clock
        ), mock.patch.object(
            live,
            "read_placeholder_paths_bounded",
            side_effect=bounded_large_profile_read,
        ):
            self.assertEqual(live.DEFAULT_OVERALL_DEADLINE_SECONDS, 120.0)
            worker_budget = live.DEFAULT_OVERALL_DEADLINE_SECONDS - (
                live.worker_cleanup_reserve(live.DEFAULT_OVERALL_DEADLINE_SECONDS)
            )
            self.assertEqual(worker_budget, 118.0)
            self.assertLess(
                live.FOREGROUND_HYDRATION_TIMEOUT_SECONDS
                + 3 * live.GENERATED_DOCUMENT_VERIFY_TIMEOUT_SECONDS,
                worker_budget,
            )
            deadline = live.Deadline(worker_budget)
            recorder = live.StageRecorder("before", deadline)
            before = live.run_before(
                self.database,
                self.data,
                self.cloud,
                self.state,
                self.evidence,
                dataless_probe=lambda _path: True,
                deadline=deadline,
                recorder=recorder,
            )
            deadline_remaining_ms = deadline.remaining_ms()

        self.assertTrue(live.evidence_passed("before", before), before)
        private = json.loads(self.state.read_text())
        self.assertEqual(private["sample_item"], bytes([18]).hex())
        self.assertEqual(deadline_remaining_ms, 28_004)
        self.assertEqual(recorder.timings["hydrate_attachment"], 59_999)
        self.assertEqual(recorder.timings["verify_generated_documents"], 29_997)
        self.assertLessEqual(
            before["generated_verification_bytes"],
            before["generated_verification_byte_limit"],
        )

    def test_before_fails_closed_when_the_selected_placeholder_reprobe_times_out(self):
        outcomes = iter(
            (
                live.PlaceholderProbeResult(live.PlaceholderState.DATALESS, 1),
                live.PlaceholderProbeResult(live.PlaceholderState.TIMEOUT, 500),
            )
        )
        self.db.close()

        with self.assertRaises(live.AcceptanceFailure) as caught:
            live.run_before(
                self.database,
                self.data,
                self.cloud,
                self.state,
                self.evidence,
                dataless_probe=lambda _path: next(outcomes),
            )

        self.assertEqual(str(caught.exception), "finder-placeholder-stat-timeout")
        self.assertFalse(self.state.exists())

    def test_before_resolves_the_exact_missing_placeholder_before_one_read(self):
        original_read = live.read_placeholder_once
        probes = iter(
            (
                live.PlaceholderProbeResult(live.PlaceholderState.MISSING, 1),
                live.PlaceholderProbeResult(live.PlaceholderState.DATALESS, 1),
                live.PlaceholderProbeResult(live.PlaceholderState.DATALESS, 1),
                live.PlaceholderProbeResult(live.PlaceholderState.DATALESS, 1),
            )
        )
        resolved = []

        def read_and_publish(path: Path):
            result = original_read(path)
            digest, size = result
            publisher = sqlite3.connect(self.database)
            publisher.execute(
                "INSERT OR REPLACE INTO cache_entries VALUES"
                "(?, 'attachment-v1', ?, 'verified', ?, 'blob', ?)",
                (bytes([8]), size, str(path), bytes.fromhex(digest)),
            )
            publisher.commit()
            publisher.close()
            return result

        self.db.close()
        with mock.patch.object(
            live, "read_placeholder_once", side_effect=read_and_publish
        ):
            before = live.run_before(
                self.database,
                self.data,
                self.cloud,
                self.state,
                self.evidence,
                dataless_probe=lambda _path: next(probes),
                placeholder_resolver=lambda item_id: resolved.append(item_id) or True,
            )

        self.assertTrue(live.evidence_passed("before", before))
        self.assertEqual(resolved, [bytes([8])])


class DeadlineAndCleanupTests(unittest.TestCase):
    def test_worker_publishes_only_aggregate_bounded_selection_diagnostics(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence_path = root / "evidence.json"
            args = live.argparse.Namespace(
                phase="before",
                deadline_seconds=2.0,
                progress=None,
                data_root=root,
                cloud_root=root,
                state=root / "private.json",
                evidence=evidence_path,
            )
            facts = live.PlaceholderSelectionFacts(
                candidates_considered=3,
                missing_count=1,
                stat_error_count=1,
                stat_timeout_count=1,
                probe_elapsed_ms=504,
            )
            failure = live.AcceptanceFailure(
                "bounded-placeholder-selection-exhausted", facts.public_evidence()
            )
            with mock.patch.object(live, "run_before", side_effect=failure), mock.patch(
                "sys.stderr", io.StringIO()
            ):
                result = live.run_worker(args)

            evidence = live.json.loads(evidence_path.read_text())
            self.assertEqual(result, 1)
            self.assertEqual(
                evidence["failure_category"],
                "bounded-placeholder-selection-exhausted",
            )
            self.assertEqual(evidence["placeholder_candidates_considered"], 3)
            self.assertEqual(evidence["placeholder_missing_count"], 1)
            self.assertEqual(evidence["placeholder_stat_error_count"], 1)
            self.assertEqual(evidence["placeholder_stat_timeout_count"], 1)
            live.validate_public_evidence(evidence)

    def test_parent_classifies_hard_timeout_with_fixed_category(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence_path = root / "evidence.json"
            timed_out = live.WorkerProcessResult(
                returncode=-signal.SIGKILL,
                stdout="",
                stderr="",
                timed_out=True,
                cleanup_complete=True,
                elapsed_ms=250,
            )
            with mock.patch.object(
                live, "run_worker_process", return_value=timed_out
            ), mock.patch("sys.stdout", io.StringIO()), mock.patch(
                "sys.stderr", io.StringIO()
            ):
                result = live.main(
                    (
                        "after",
                        "--data-root",
                        str(root),
                        "--state",
                        str(root / "private.json"),
                        "--evidence",
                        str(evidence_path),
                        "--deadline-seconds",
                        "0.25",
                    )
                )
            evidence = live.json.loads(evidence_path.read_text())
            self.assertEqual(result, 1)
            self.assertEqual(evidence["failure_category"], "overall-deadline-exceeded")
            self.assertEqual(evidence["deadline_ms"], 250)
            self.assertTrue(evidence["child_cleanup_complete"])

    def test_parent_classifies_cleanup_deadline_separately(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence_path = root / "evidence.json"
            timed_out = live.WorkerProcessResult(
                returncode=-1,
                stdout="",
                stderr="",
                timed_out=True,
                cleanup_complete=False,
                elapsed_ms=250,
            )
            with mock.patch.object(
                live, "run_worker_process", return_value=timed_out
            ), mock.patch("sys.stdout", io.StringIO()), mock.patch(
                "sys.stderr", io.StringIO()
            ):
                result = live.main(
                    (
                        "after",
                        "--data-root",
                        str(root),
                        "--state",
                        str(root / "private.json"),
                        "--evidence",
                        str(evidence_path),
                        "--deadline-seconds",
                        "0.25",
                    )
                )
            evidence = json.loads(evidence_path.read_text())
            self.assertEqual(result, 1)
            self.assertEqual(
                evidence["failure_category"], "worker-cleanup-deadline-exceeded"
            )
            self.assertFalse(evidence["child_cleanup_complete"])

    def test_cleanup_wait_and_pipe_drain_are_always_deadline_bounded(self):
        class NeverReapedProcess:
            pid = 999_999

            def __init__(self):
                self.returncode = None
                self.stdout = mock.Mock()
                self.stderr = mock.Mock()
                self.wait_timeouts = []
                self.communicate_timeouts = []

            def poll(self):
                return None

            def wait(self, *, timeout):
                self.wait_timeouts.append(timeout)
                raise subprocess.TimeoutExpired(("worker",), timeout)

            def communicate(self, *, timeout):
                self.communicate_timeouts.append(timeout)
                raise subprocess.TimeoutExpired(("worker",), timeout)

        process = NeverReapedProcess()
        deadline = time.monotonic() + 0.01
        with mock.patch.object(live.os, "killpg"):
            self.assertFalse(live.terminate_process_group(process, deadline))
            self.assertEqual(live.collect_worker_output(process, deadline), ("", ""))
        self.assertTrue(process.wait_timeouts)
        self.assertTrue(process.communicate_timeouts)
        self.assertTrue(all(timeout >= 0 for timeout in process.wait_timeouts))
        self.assertTrue(all(timeout >= 0 for timeout in process.communicate_timeouts))
        process.stdout.close.assert_called_once_with()
        process.stderr.close.assert_called_once_with()

    def test_parent_emits_fixed_failure_evidence_and_reaps_worker(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence_path = root / "evidence.json"
            with mock.patch("sys.stdout", io.StringIO()), mock.patch(
                "sys.stderr", io.StringIO()
            ):
                result = live.main(
                    (
                        "before",
                        "--data-root",
                        str(root / "missing-data"),
                        "--cloud-root",
                        str(root / "missing-cloud"),
                        "--state",
                        str(root / "private.json"),
                        "--evidence",
                        str(evidence_path),
                        "--deadline-seconds",
                        "2",
                    )
                )
            evidence = live.json.loads(evidence_path.read_text())
            self.assertEqual(result, 1)
            self.assertEqual(evidence["failure_category"], "acceptance-io-failed")
            self.assertEqual(evidence["timeout_stage"], "select_candidate")
            self.assertTrue(evidence["child_cleanup_complete"])
            self.assertNotEqual(evidence["worker_exit_code"], 0)
            self.assertEqual(
                set(evidence["stage_timings_ms"]), set(live.PHASE_STAGES["before"])
            )

    def test_expired_snapshot_deadline_uses_fixed_category_and_removes_partial(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            database = root / "live.sqlite3"
            snapshot = root / "snapshot.sqlite3"
            db = sqlite3.connect(database)
            db.executescript(
                """
                CREATE TABLE items (item_id BLOB PRIMARY KEY, deleted_at_ms INTEGER);
                CREATE TABLE chat_sync_state (
                    account_id INTEGER,
                    namespace_version INTEGER,
                    chat_id INTEGER,
                    oldest_loaded_message_id INTEGER,
                    newest_loaded_message_id INTEGER,
                    history_complete INTEGER
                );
                """
            )
            db.close()
            deadline = live.Deadline(0)
            with self.assertRaises(live.DeadlineExceeded) as caught:
                live.create_indexed_snapshot(database, snapshot, deadline)
            self.assertEqual(str(caught.exception), "overall-deadline-exceeded")
            self.assertFalse(snapshot.exists())
            self.assertFalse(live._snapshot_build_path(snapshot).exists())

    @unittest.skipUnless(os.name == "posix", "process-group cleanup is POSIX-only")
    def test_hard_timeout_kills_exact_worker_process_group(self):
        with tempfile.TemporaryDirectory() as directory:
            pid_file = Path(directory) / "grandchild.pid"
            command = [
                sys.executable,
                "-c",
                (
                    "import os,sys,time;"
                    "p=os.fork();"
                    "p == 0 and time.sleep(60);"
                    "f=open(sys.argv[1],'w');f.write(str(p));f.close();"
                    "time.sleep(60)"
                ),
                str(pid_file),
            ]
            result = live.run_worker_process(command, timeout=0.2)
            self.assertTrue(result.timed_out)
            self.assertTrue(result.cleanup_complete)
            grandchild = int(pid_file.read_text())
            try:
                for _ in range(100):
                    try:
                        os.kill(grandchild, 0)
                    except ProcessLookupError:
                        break
                    status = subprocess.run(
                        ("ps", "-o", "stat=", "-p", str(grandchild)),
                        capture_output=True,
                        text=True,
                        check=False,
                    ).stdout.strip()
                    if not status or status.startswith("Z"):
                        break
                    time.sleep(0.01)
                else:
                    self.fail("worker grandchild survived exact process-group cleanup")
            finally:
                try:
                    os.kill(grandchild, signal.SIGKILL)
                except ProcessLookupError:
                    pass

    @unittest.skipUnless(os.name == "posix", "process-group cleanup is POSIX-only")
    def test_sigterm_resistant_worker_is_reaped_inside_overall_deadline(self):
        command = [
            sys.executable,
            "-c",
            (
                "import signal,time;"
                "signal.signal(signal.SIGTERM, signal.SIG_IGN);"
                "time.sleep(60)"
            ),
        ]
        deadline = 0.25
        started = time.monotonic()
        result = live.run_worker_process(command, timeout=deadline)
        wall_elapsed = time.monotonic() - started
        self.assertTrue(result.timed_out)
        self.assertTrue(result.cleanup_complete)
        self.assertEqual(result.returncode, -signal.SIGKILL)
        self.assertLessEqual(result.elapsed_ms, round(deadline * 1000) + 25)
        self.assertLessEqual(wall_elapsed, deadline + 0.025)

    def test_parent_derives_non_stale_timed_out_stage_duration(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence_path = root / "evidence.json"
            progress_path = evidence_path.with_name(
                f"{evidence_path.name}.progress.json"
            )
            timed_out = live.WorkerProcessResult(
                returncode=-signal.SIGKILL,
                stdout="",
                stderr="",
                timed_out=True,
                cleanup_complete=True,
                elapsed_ms=250,
            )

            def record_timed_out_stage(*_args, **_kwargs):
                progress_path.write_text(
                    live.json.dumps(
                        {
                            "phase": "before",
                            "current_stage": "select_candidate",
                            "current_stage_elapsed_ms": 0,
                            "current_stage_started_monotonic_ns": (
                                time.monotonic_ns() - 200_000_000
                            ),
                            "stage_timings_ms": {
                                stage: None for stage in live.PHASE_STAGES["before"]
                            },
                        }
                    )
                )
                return timed_out

            with mock.patch.object(
                live, "run_worker_process", side_effect=record_timed_out_stage
            ), mock.patch("sys.stdout", io.StringIO()), mock.patch(
                "sys.stderr", io.StringIO()
            ):
                result = live.main(
                    (
                        "before",
                        "--data-root",
                        str(root),
                        "--state",
                        str(root / "private.json"),
                        "--evidence",
                        str(evidence_path),
                        "--deadline-seconds",
                        "0.25",
                    )
                )
            evidence = live.json.loads(evidence_path.read_text())
            elapsed = evidence["stage_timings_ms"]["select_candidate"]
            self.assertEqual(result, 1)
            self.assertGreaterEqual(elapsed, 150)
            self.assertLessEqual(elapsed, evidence["elapsed_ms"])
            self.assertFalse(progress_path.exists())

    @unittest.skipUnless(os.name == "posix", "process-group cleanup is POSIX-only")
    def test_end_to_end_timeout_is_bounded_diagnostic_and_leaves_no_worker(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence_path = root / "evidence.json"
            pid_path = root / "worker.pid"
            stages = repr(list(live.PHASE_STAGES["before"]))
            resistant_worker = [
                sys.executable,
                "-c",
                (
                    "import json,os,pathlib,signal,sys,time;"
                    "progress,pid=sys.argv[1:3];"
                    "pathlib.Path(pid).write_text(str(os.getpid()));"
                    f"stages={stages};"
                    "pathlib.Path(progress).write_text(json.dumps({"
                    "'phase':'before','current_stage':'select_candidate',"
                    "'current_stage_elapsed_ms':0,"
                    "'current_stage_started_monotonic_ns':time.monotonic_ns(),"
                    "'stage_timings_ms':dict.fromkeys(stages)}));"
                    "signal.signal(signal.SIGTERM,signal.SIG_IGN);"
                    "time.sleep(60)"
                ),
            ]
            deadline = 0.4
            with mock.patch.object(
                live,
                "worker_command",
                side_effect=lambda _args, progress, _budget: [
                    *resistant_worker,
                    str(progress),
                    str(pid_path),
                ],
            ), mock.patch("sys.stdout", io.StringIO()), mock.patch(
                "sys.stderr", io.StringIO()
            ):
                started = time.monotonic()
                result = live.main(
                    (
                        "before",
                        "--data-root",
                        str(root),
                        "--state",
                        str(root / "private.json"),
                        "--evidence",
                        str(evidence_path),
                        "--deadline-seconds",
                        str(deadline),
                    )
                )
                wall_elapsed = time.monotonic() - started
            evidence = live.json.loads(evidence_path.read_text())
            worker_pid = int(pid_path.read_text())
            self.assertEqual(result, 1)
            self.assertLessEqual(wall_elapsed, deadline + 0.025)
            self.assertTrue(evidence["child_cleanup_complete"])
            self.assertEqual(evidence["worker_exit_code"], -signal.SIGKILL)
            self.assertEqual(evidence["timeout_stage"], "select_candidate")
            self.assertGreater(evidence["stage_timings_ms"]["select_candidate"], 0)
            self.assertLessEqual(
                evidence["stage_timings_ms"]["select_candidate"],
                evidence["elapsed_ms"],
            )
            with self.assertRaises(ProcessLookupError):
                os.kill(worker_pid, 0)

    def test_public_stage_timings_require_the_fixed_phase_schema(self):
        evidence = {
            "privacy_safe": True,
            "phase": "after",
            "stage_timings_ms": {stage: 1 for stage in live.PHASE_STAGES["after"]},
        }
        live.validate_public_evidence(evidence)
        evidence["stage_timings_ms"]["raw-chat-name"] = 1
        with self.assertRaises(live.AcceptanceFailure):
            live.validate_public_evidence(evidence)


@unittest.skipUnless(
    os.environ.get("GRAMDRIVE_LIVE_CONTENT_SCALE_TEST") == "1",
    "representative 3M-item/100k-document profile is opt-in",
)
class RepresentativeScaleTests(unittest.TestCase):
    def test_indexed_proofs_stay_inside_declared_deadline_at_profile_scale(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            database = root / "state.sqlite3"
            snapshot = root / "private.snapshot.sqlite3"
            generated = root / "cache/generated"
            generated.mkdir(parents=True)
            (root / "agent").mkdir()
            (root / "agent/settings.json").write_text('{"cacheQuotaBytes": 1000000}\n')
            db = sqlite3.connect(database)
            db.executescript(
                """
                CREATE TABLE items (
                    item_id BLOB PRIMARY KEY,
                    parent_item_id BLOB,
                    safe_name TEXT NOT NULL DEFAULT 'filler',
                    kind TEXT NOT NULL DEFAULT 'filler',
                    availability TEXT NOT NULL DEFAULT 'local',
                    logical_size INTEGER NOT NULL DEFAULT 0,
                    deleted_at_ms INTEGER,
                    mime_type TEXT,
                    content_version TEXT,
                    created_at_ms INTEGER,
                    modified_at_ms INTEGER
                );
                CREATE INDEX items_children_by_id ON items(parent_item_id, item_id);
                CREATE UNIQUE INDEX items_sibling_name
                    ON items(parent_item_id, safe_name)
                    WHERE parent_item_id IS NOT NULL AND deleted_at_ms IS NULL;
                CREATE INDEX items_live_fetchable_attachments_by_size
                    ON items(logical_size, item_id)
                    WHERE kind='attachment' AND availability='fetchable'
                      AND deleted_at_ms IS NULL AND logical_size > 0;
                CREATE TABLE chat_sync_state (
                    account_id INTEGER NOT NULL,
                    namespace_version INTEGER NOT NULL,
                    chat_id INTEGER NOT NULL,
                    oldest_loaded_message_id INTEGER,
                    newest_loaded_message_id INTEGER,
                    history_complete INTEGER NOT NULL,
                    PRIMARY KEY (account_id, namespace_version, chat_id)
                );
                CREATE TABLE cache_entries (
                    item_id BLOB PRIMARY KEY,
                    kind TEXT NOT NULL,
                    size INTEGER NOT NULL,
                    verification TEXT NOT NULL,
                    materialization_ref TEXT,
                    blob_hash BLOB
                );
                CREATE INDEX cache_entries_by_materialization_ref
                    ON cache_entries(materialization_ref)
                    WHERE materialization_ref IS NOT NULL;
                CREATE TABLE accounts (
                    auth_state TEXT NOT NULL,
                    retention_mode TEXT NOT NULL,
                    archive_mode INTEGER NOT NULL
                );
                INSERT INTO accounts VALUES ('authorized', 'forever', 0);
                CREATE TABLE story_appearances (location TEXT NOT NULL);
                INSERT INTO story_appearances VALUES ('active');
                WITH RECURSIVE ids(value) AS (
                    VALUES(1)
                    UNION ALL SELECT value + 1 FROM ids WHERE value < 2999988
                )
                INSERT INTO items(item_id, deleted_at_ms)
                SELECT CAST(printf('%016x', value) AS BLOB), NULL FROM ids;
                INSERT INTO items(
                    item_id,parent_item_id,safe_name,kind,availability,logical_size,
                    deleted_at_ms,mime_type,content_version,created_at_ms,modified_at_ms
                ) VALUES
                    (x'FF01',NULL,'Account','account','local',0,NULL,NULL,NULL,10,20),
                    (x'FF02',x'FF01','Chats','chat_list','local',0,NULL,NULL,NULL,10,20),
                    (x'FF03',x'FF02','Active','chat','local',0,NULL,NULL,NULL,10,20),
                    (x'FF04',x'FF03','2026-08','month_dir','local',0,NULL,NULL,NULL,10,20),
                    (x'FF05',x'FF04','Messages.md','generated_doc','local',1,NULL,'text/markdown','md',10,20),
                    (x'FF06',x'FF04','Messages.ndjson','generated_doc','local',1,NULL,'application/x-ndjson','nd',10,20),
                    (x'FF07',x'FF03','.chat.json','generated_doc','local',1,NULL,'application/json','chat',10,20),
                    (x'FF08',x'FF04','sample.bin','attachment','fetchable',1,NULL,'application/octet-stream','sample',10,20),
                    (x'FF09',x'FF03','Active Stories','active_stories','local',0,NULL,NULL,NULL,10,20),
                    (x'FF0A',x'FF09','Story.jpg','story_appearance','local',1,NULL,'image/jpeg','story',10,20),
                    (x'FF0B',x'FF02','Zero','chat','local',0,NULL,NULL,NULL,10,20),
                    (x'FF0C',x'FF0B','.chat.json','generated_doc','local',0,NULL,'application/json','zero-chat',10,20);
                WITH RECURSIVE cursors(value) AS (
                    VALUES(1)
                    UNION ALL SELECT value + 1 FROM cursors WHERE value < 10000
                )
                INSERT INTO chat_sync_state
                SELECT 1, 1, value, 100, 200, 0 FROM cursors;
                """
            )
            names = ("Messages.md", "Messages.ndjson", "chat.json")
            pending = []
            for value in range(100_000):
                path = generated / str(value // 3) / "current" / names[value % 3]
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(b"x")
                pending.append((value.to_bytes(8, "big"), 1, str(path)))
                if len(pending) == 1_000:
                    db.executemany(
                        "INSERT INTO cache_entries("
                        "item_id,kind,size,verification,materialization_ref) VALUES "
                        "(?, 'generated_doc', ?, 'verified', ?)",
                        pending,
                    )
                    pending.clear()
            if pending:
                db.executemany(
                    "INSERT INTO cache_entries("
                    "item_id,kind,size,verification,materialization_ref) VALUES "
                    "(?, 'generated_doc', ?, 'verified', ?)",
                    pending,
                )
            db.execute(
                "INSERT INTO cache_entries VALUES "
                "(CAST('0000000000000001' AS BLOB), "
                "'attachment', 1, 'verified', NULL, x'00')"
            )
            db.commit()
            db.close()

            state = root / "private.json"
            evidence = root / "evidence.json"
            state.write_text(
                json.dumps(
                    {
                        "sample_item": "30303030303030303030303030303031",
                        "expected_size": 1,
                        "hydrated_digest": "00",
                        "generated_records": [],
                    }
                )
            )
            evidence.write_text("{}")
            with mock.patch.object(
                live, "QUIESCENCE_STABLE_POLLS", 1
            ), mock.patch.object(live, "QUIESCENCE_WAIT_SECONDS", 0):
                phase = live.run_stability_snapshot(database, state, evidence)
            max_rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
            max_rss_bytes = max_rss if sys.platform == "darwin" else max_rss * 1024
            print(
                "representative-scale-foreground "
                f"items={phase['before_item_count']} "
                f"python_max_rss_bytes={max_rss_bytes} "
                "python_max_rss_limit_bytes=402653184"
            )
            self.assertEqual(phase["before_item_count"], 3_000_000)
            self.assertLess(max_rss_bytes, 384 * 1024 * 1024)
            self.assertTrue(live.snapshot_database_path(state).is_file())

            deadline = live.Deadline(90)
            started = time.monotonic()
            counts = live.create_indexed_snapshot(database, snapshot, deadline)
            check = live.connection(database, deadline)
            live.attach_snapshot(check, snapshot)
            items = live.compare_items_indexed(check)
            cursors = live.compare_cursors_indexed(check)
            storage = live.verify_generated_storage(check, root, deadline)
            candidate_plan = " ".join(
                row[3]
                for row in check.execute(
                    "EXPLAIN QUERY PLAN " + live.CANDIDATE_QUERY,
                    (
                        live.MAX_GENERATED_VERIFICATION_BYTES,
                        live.MAX_CANDIDATES,
                    ),
                )
            ).lower()
            candidates = live.candidate_rows(check)
            namespace = live.namespace_facts(check)
            check.close()
            elapsed = time.monotonic() - started
            print(
                "representative-scale-proof "
                f"items={counts['item_count']} "
                f"generated_documents={storage.physical_file_count} "
                f"proof_elapsed_ms={round(elapsed * 1000)} "
                "deadline_ms=90000"
            )

            self.assertEqual(counts["item_count"], 3_000_000)
            self.assertEqual(storage.current_reference_count, 100_000)
            self.assertEqual(storage.physical_file_count, 100_000)
            self.assertEqual(storage.orphan_file_count, 0)
            self.assertTrue(storage.current_materializations_preserved)
            self.assertTrue(items.additive_only)
            self.assertTrue(cursors.preserved)
            self.assertEqual(len(candidates), 1)
            self.assertIn("items_live_fetchable_attachments_by_size", candidate_plan)
            self.assertNotIn("temp b-tree", candidate_plan)
            self.assertTrue(namespace.hidden_metadata_complete)
            self.assertTrue(namespace.zero_story_containers_omitted)
            self.assertTrue(namespace.story_containers_truthful)
            self.assertLess(elapsed, 90)


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

        regressed = live.compare_items("before", 2, ["a", "b"], "after", 2, ["a", "c"])
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
            "generated_verification_bytes": 1024,
            "generated_verification_byte_limit": (
                live.MAX_GENERATED_VERIFICATION_BYTES
            ),
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

        evidence["generated_verification_bytes"] = (
            evidence["generated_verification_byte_limit"] + 1
        )
        self.assertFalse(live.evidence_passed("after", evidence))
        evidence["generated_verification_bytes"] = 1024

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
            return live.run_stability_snapshot(self.database, self.state, self.evidence)

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
        self.assertEqual(str(caught.exception), "hydrated-sample-cache-entry-missing")

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
                self.add_cache_rows(original_size=size, original_digest=digest)
                with self.assertRaises(live.AcceptanceFailure) as caught:
                    self.run_snapshot()
                self.assertEqual(str(caught.exception), expected)


if __name__ == "__main__":
    unittest.main()
