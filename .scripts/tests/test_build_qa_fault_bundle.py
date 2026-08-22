#!/usr/bin/env python3
"""Tests for the explicit non-shipping QA bundle wrapper."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT = REPO_ROOT / ".scripts/apple-app/build_qa_fault_bundle.py"
spec = importlib.util.spec_from_file_location("build_qa_fault_bundle", SCRIPT)
qa = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = qa
spec.loader.exec_module(qa)


class QABundleSecretTests(unittest.TestCase):
    def test_secret_requires_owner_only_canonical_hex(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "secret"
            path.write_text("ab" * 32 + "\n")
            path.chmod(0o600)
            self.assertEqual(qa.read_secret(path), "ab" * 32)
            path.chmod(0o644)
            with self.assertRaisesRegex(qa.app.StepFailed, "mode 0600"):
                qa.read_secret(path)
            path.chmod(0o400)
            with self.assertRaisesRegex(qa.app.StepFailed, "mode 0600"):
                qa.read_secret(path)

    def test_malformed_secret_is_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "secret"
            path.write_text("ABC")
            path.chmod(0o600)
            with self.assertRaisesRegex(qa.app.StepFailed, "32 lowercase hex"):
                qa.read_secret(path)


if __name__ == "__main__":
    unittest.main()
