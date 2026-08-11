#!/usr/bin/env python3
"""Tests for the privacy-safe live-content acceptance runner."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = REPO_ROOT / ".scripts" / "acceptance" / "run_live_content.py"


def load_runner_module():
    spec = importlib.util.spec_from_file_location("run_live_content", RUNNER_PATH)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


live = load_runner_module()
VERSIONS = {
    "python": "Python 3.test",
    "cargo": "cargo 1.test",
    "rustc": "rustc 1.test",
    "swift": "Swift 6.test",
}


class RecordingRunner:
    def __init__(self, failures: set[str] | None = None, timeout: str | None = None):
        self.failures = failures or set()
        self.timeout = timeout
        self.calls: list[tuple[tuple[str, ...], int]] = []

    def __call__(self, argv, _repo_root, deadline):
        argv = tuple(argv)
        self.calls.append((argv, deadline))
        command = " ".join(argv)
        if self.timeout and self.timeout in command:
            return 124, True, 42
        return (9 if any(token in command for token in self.failures) else 0), False, 7


class CatalogTests(unittest.TestCase):
    def test_catalog_composes_the_accepted_rust_and_swift_surfaces(self):
        catalog = live.build_catalog()
        labels = [scenario.label for scenario in catalog]
        self.assertEqual(
            labels,
            [
                "rust-history-live-stories",
                "rust-monthly-render",
                "rust-markdown-ndjson",
                "rust-state-fidelity-retention-scale",
                "rust-ffi-hydration-policy",
                "swift-package-build",
                "swift-provider-companion-regressions",
            ],
        )
        commands = "\n".join(" ".join(scenario.argv) for scenario in catalog)
        for required in [
            "history_crawl",
            "live_updates",
            "story_discovery",
            "backfill_scheduler",
            "render_pipeline",
            "gramdrive-render",
            "repo_changes",
            "repo_content_progress",
            "repo_live_content",
            "repo_retention",
            "query_plans",
            "gramdrive-ffi --lib",
            "swift build",
            "swift test",
        ]:
            self.assertIn(required, commands)

    def test_catalog_rejects_relabelled_or_missing_scenarios(self):
        with self.assertRaises(ValueError):
            live.validate_catalog(live.build_catalog()[:-1])


class EvidenceTests(unittest.TestCase):
    def run_matrix(self, runner: RecordingRunner):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        output = Path(tmp.name) / "live-content.json"
        evidence, code = live.run_acceptance(
            repo_root=Path(tmp.name),
            output=output,
            catalog=live.build_catalog(),
            runner=runner,
            versions=VERSIONS,
            echo=lambda _message: None,
        )
        return evidence, code, output

    def test_green_matrix_records_only_counts_booleans_timings_versions_and_fixed_labels(self):
        evidence, code, output = self.run_matrix(RecordingRunner())
        self.assertEqual(code, live.EXIT_OK)
        self.assertTrue(evidence["passed"])
        self.assertEqual(evidence["scenario_count"], 7)
        self.assertEqual(evidence["passed_count"], 7)
        self.assertEqual(evidence["failed_count"], 0)
        self.assertTrue(evidence["synthetic_only"])
        self.assertTrue(evidence["privacy_safe"])
        self.assertTrue(evidence["evidence_within_bound"])
        self.assertLessEqual(output.stat().st_size, live.EVIDENCE_BYTE_LIMIT)
        self.assertEqual(json.loads(output.read_text()), evidence)

    def test_failure_is_recorded_and_later_scenarios_still_run(self):
        runner = RecordingRunner(failures={"repo_retention"})
        evidence, code, _ = self.run_matrix(runner)
        self.assertEqual(code, live.EXIT_FAILED)
        self.assertFalse(evidence["passed"])
        self.assertEqual(evidence["failed_count"], 1)
        self.assertEqual(len(runner.calls), len(live.build_catalog()))

    def test_timeout_uses_the_fixed_deadline_and_is_a_failure(self):
        runner = RecordingRunner(timeout="swift test")
        evidence, code, _ = self.run_matrix(runner)
        self.assertEqual(code, live.EXIT_FAILED)
        self.assertTrue(evidence["scenarios"][-1]["timed_out"])
        self.assertTrue(
            all(deadline == live.SCENARIO_DEADLINE_SECONDS for _, deadline in runner.calls)
        )

    def test_evidence_schema_rejects_free_form_content_fields(self):
        evidence, _, _ = self.run_matrix(RecordingRunner())
        evidence["chat_title"] = "must never persist"
        with self.assertRaises(ValueError):
            live.validate_evidence(evidence, live.build_catalog())

    def test_serialized_evidence_contains_no_commands_logs_or_content(self):
        _, _, output = self.run_matrix(RecordingRunner())
        text = output.read_text()
        for forbidden in [
            "command",
            "argv",
            "stdout",
            "stderr",
            "log",
            "content",
            "message",
            "chat_title",
            "account_name",
        ]:
            self.assertNotIn(f'"{forbidden}"', text)


class ProcessTests(unittest.TestCase):
    def test_default_runner_enforces_the_deadline(self):
        with tempfile.TemporaryDirectory() as tmp:
            code, timed_out, duration_ms = live.default_runner(
                (sys.executable, "-c", "import time; time.sleep(60)"),
                Path(tmp),
                0,
            )
        self.assertEqual(code, 124)
        self.assertTrue(timed_out)
        self.assertLess(duration_ms, 6_000)


class CliTests(unittest.TestCase):
    def test_list_needs_no_output_path(self):
        self.assertEqual(live.main(["--list"]), live.EXIT_OK)

    def test_run_requires_an_output_path(self):
        with self.assertRaises(SystemExit):
            live.main([])


if __name__ == "__main__":
    unittest.main()
