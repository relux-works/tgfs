#!/usr/bin/env python3
"""Deterministic tests for the QA-only installed fault recovery runner."""

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
RUNNER = REPO_ROOT / ".scripts/acceptance/run_installed_fault_recovery.py"
spec = importlib.util.spec_from_file_location("run_installed_fault_recovery", RUNNER)
faults = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = faults
spec.loader.exec_module(faults)


class RecordContractTests(unittest.TestCase):
    def test_canonical_record_and_mac_are_cross_language_stable(self):
        fields = faults.authenticated_fields(
            account_id=77,
            item_id="qa-synthetic-item-001",
            purpose="content",
            fault="source_not_found",
            nonce="a" * 32,
            expires_at_ms=2_000,
        )
        self.assertEqual(
            faults.canonical_bytes(fields),
            b'{"account_id":77,"expires_at_ms":2000,"fault":"source_not_found",'
            b'"item_id":"qa-synthetic-item-001","nonce":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",'
            b'"purpose":"content","schema":"gramdrive.qa-fault-control.v1"}',
        )
        record = faults.signed_record(fields, bytes.fromhex("01" * 32))
        self.assertEqual(
            record["mac"],
            "20be4cf1eb5eb78d305e4d8b34d1b013f4a8352f727472933789ecf09d0be772",
        )

    def test_control_record_is_atomic_owner_only_and_clearable(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / faults.CONTROL_RELATIVE_PATH
            faults.arm_fault(path, {"schema": faults.CONTROL_SCHEMA})
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(json.loads(path.read_text())["schema"], faults.CONTROL_SCHEMA)
            faults.clear_fault(path)
            self.assertFalse(path.exists())

    def test_acceptance_secret_requires_exact_mode_0600(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "secret"
            path.write_text("01" * 32 + "\n")
            path.chmod(0o600)
            self.assertEqual(faults.read_secret(path), bytes.fromhex("01" * 32))
            path.chmod(0o400)
            with self.assertRaises(faults.AcceptanceFailure):
                faults.read_secret(path)


class InstalledMatrixTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.data_root = self.root / "data"
        self.cloud_root = self.root / "cloud"
        self.database = self.data_root / "state/gramdrive.sqlite3"
        self.database.parent.mkdir(parents=True)
        self.cloud_root.mkdir()
        with sqlite3.connect(self.database) as db:
            db.executescript(
                """
                CREATE TABLE items (
                    item_id BLOB PRIMARY KEY,
                    account_id INTEGER NOT NULL,
                    parent_item_id BLOB,
                    safe_name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    content_version TEXT,
                    availability TEXT NOT NULL,
                    mime_type TEXT,
                    deleted_at_ms INTEGER
                );
                CREATE TABLE provider_fetch_health (
                    singleton INTEGER PRIMARY KEY,
                    callback_count INTEGER NOT NULL,
                    success_count INTEGER NOT NULL,
                    engine_failure_count INTEGER NOT NULL,
                    provider_mapping_count INTEGER NOT NULL,
                    no_such_item_count INTEGER NOT NULL,
                    retryable_count INTEGER NOT NULL
                );
                INSERT INTO provider_fetch_health VALUES (1, 0, 0, 0, 0, 0, 0);
                """
            )
            root_id = b"\x01root"
            db.execute(
                "INSERT INTO items VALUES (?, 77, NULL, 'Account', 'account', NULL, "
                "'fetchable', NULL, NULL)",
                (root_id,),
            )
            for index, (purpose, fault) in enumerate(
                (
                    (purpose, fault)
                    for purpose in faults.PURPOSES
                    for fault in faults.FAULTS
                ),
                1,
            ):
                raw = bytes([1, 5, index])
                name = f"gramdrive-qa-fault-{purpose}-{fault}.png"
                db.execute(
                    "INSERT INTO items VALUES (?, 77, ?, ?, 'attachment', 'v1', "
                    "'fetchable', 'image/png', NULL)",
                    (raw, root_id, name),
                )
                (self.cloud_root / name).write_bytes(b"synthetic")
            db.commit()

    def trigger(self, _path: Path, _scratch: Path) -> int:
        control = self.data_root / faults.CONTROL_RELATIVE_PATH
        with sqlite3.connect(self.database) as db:
            if control.exists():
                db.execute(
                    "UPDATE provider_fetch_health SET callback_count=callback_count+1, "
                    "engine_failure_count=engine_failure_count+1, "
                    "provider_mapping_count=provider_mapping_count+1, "
                    "retryable_count=retryable_count+1 WHERE singleton=1"
                )
            else:
                db.execute(
                    "UPDATE provider_fetch_health SET callback_count=callback_count+1, "
                    "success_count=success_count+1 WHERE singleton=1"
                )
            db.commit()
        return 0

    def test_full_open_preview_matrix_preserves_identity_and_recovers(self):
        evidence = faults.run_matrix(
            database=self.database,
            data_root=self.data_root,
            cloud_root=self.cloud_root,
            secret=bytes.fromhex("01" * 32),
            fixture_prefix="gramdrive-qa-fault",
            evidence_path=self.root / "evidence.json",
            trigger={"content": self.trigger, "thumbnail": self.trigger},
            dataless_probe=lambda _path: True,
        )
        self.assertTrue(evidence["passed"])
        self.assertEqual(evidence["case_count"], 10)
        serialized = json.dumps(evidence)
        self.assertNotIn("item_id", serialized)
        self.assertNotIn("synthetic-item", serialized)
        self.assertFalse((self.data_root / faults.CONTROL_RELATIVE_PATH).exists())

    def test_missing_fixture_fails_with_fixed_privacy_safe_label(self):
        with sqlite3.connect(self.database) as db:
            with self.assertRaises(faults.AcceptanceFailure) as caught:
                faults.fixture(
                    db,
                    self.cloud_root,
                    fixture_prefix="missing",
                    purpose="content",
                    fault="timeout",
                )
        self.assertEqual(str(caught.exception), "synthetic-fixture-missing")


if __name__ == "__main__":
    unittest.main()
