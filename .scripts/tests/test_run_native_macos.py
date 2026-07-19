#!/usr/bin/env python3
"""Tests for .scripts/acceptance/run_native_macos.py.

Run: python3 -m unittest discover -s .scripts/tests -t .scripts/tests
     (or via the gate itself: run_automated.py --suite repo --run-id local-repo)

The macOS native-acceptance harness cannot be exercised for real off a Mac with
a signed build and a Telegram account, which is the whole reason it is human-in-
the-loop. So these tests inject a fake command runner and a fake filesystem
oracle, and cover what the harness must get right *without* that host: the
catalog is the gate's ten scenarios, the preflight classifies a matrix vs a
non-matrix host correctly, the probes assert only what they can, the generated
docs carry every scenario, and — the property that keeps the gate honest — a
prepared run never reports a scenario as passed.
"""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = REPO_ROOT / ".scripts" / "acceptance" / "run_native_macos.py"


def load_module():
    """Import run_native_macos.py by path (`.scripts` is not a package)."""
    spec = importlib.util.spec_from_file_location("run_native_macos", RUNNER_PATH)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


macos = load_module()


class FakeRunner:
    """Replies from a scripted table keyed by a substring of the command.

    Anything unmatched succeeds with "ok\\n", so a test only states the commands
    it cares about. Records calls for assertions about what actually ran.
    """

    def __init__(self, results: dict[str, tuple[int, str]] | None = None):
        self.results = results or {}
        self.calls: list[tuple[str, ...]] = []

    def __call__(self, argv):
        argv = tuple(argv)
        self.calls.append(argv)
        joined = " ".join(argv)
        for key, result in self.results.items():
            if key in joined:
                return result
        return 0, "ok\n"

    @property
    def commands(self) -> list[str]:
        return [" ".join(argv) for argv in self.calls]


def matrix_runner(**overrides) -> FakeRunner:
    """A runner that answers like a healthy macOS 14 arm64 host by default."""
    table = {
        "sw_vers": (0, "14.5\n"),
        "uname": (0, "arm64\n"),
        "command -v fileproviderctl": (0, "/usr/bin/fileproviderctl\n"),
        "codesign --verify": (0, ""),
        "spctl --assess": (0, "accepted\n"),
        "git rev-parse": (0, "deadbeef\n"),
        "fileproviderctl dump": (0, f"domain: {macos.PROVIDER_BUNDLE_ID}\n"),
    }
    table.update(overrides)
    return FakeRunner(table)


class CatalogTests(unittest.TestCase):
    def setUp(self):
        self.catalog = macos.build_catalog()

    def test_catalog_is_exactly_the_ten_gate_scenarios_in_order(self):
        keys = [s.key for s in self.catalog]
        self.assertEqual(keys, list(macos.REQUIRED_SCENARIO_KEYS))
        self.assertEqual(len(keys), 10)

    def test_validate_accepts_the_real_catalog(self):
        macos.validate_catalog(self.catalog)  # must not raise

    def test_validate_rejects_a_missing_scenario(self):
        with self.assertRaises(ValueError):
            macos.validate_catalog(self.catalog[:-1])

    def test_validate_rejects_a_scenario_without_a_human_check(self):
        broken = macos.Scenario(
            key=self.catalog[0].key,
            title="x",
            spec_refs=("SYNC-001",),
            gate="g",
            preconditions=(),
            probes=(macos._dump_probe(),),
            manual_checks=(),
        )
        catalog = (broken,) + self.catalog[1:]
        with self.assertRaises(ValueError):
            macos.validate_catalog(catalog)

    def test_every_scenario_names_spec_refs_and_a_human_check(self):
        for scenario in self.catalog:
            self.assertTrue(scenario.spec_refs, scenario.key)
            self.assertTrue(scenario.manual_checks, scenario.key)

    def test_registration_asserts_the_provider_bundle_is_registered(self):
        registration = self.catalog[0]
        asserted = [p for p in registration.probes if p.assertion is not None]
        self.assertTrue(asserted, "registration must have a machine-checkable probe")


class AssertionTests(unittest.TestCase):
    def test_assert_contains_needs_zero_exit_and_the_needle(self):
        check = macos.assert_contains("dom", "present")
        self.assertEqual(check(0, "has dom here")[0], True)
        self.assertEqual(check(0, "nothing")[0], False)
        self.assertEqual(check(1, "has dom here")[0], False)

    def test_assert_zero(self):
        check = macos.assert_zero("valid")
        self.assertTrue(check(0, "")[0])
        self.assertFalse(check(1, "")[0])


class PreflightTests(unittest.TestCase):
    def test_healthy_matrix_host_is_ready(self):
        env = macos.preflight(
            runner=matrix_runner(),
            app_candidates=("/Applications/GramDrive.app",),
            exists=lambda p: p == "/Applications/GramDrive.app",
        )
        self.assertTrue(env.ready, env.reasons)
        self.assertEqual(env.arch, "arm64")
        self.assertEqual(env.macos_version, "14.5")
        self.assertTrue(env.signature_valid)
        self.assertEqual(env.gatekeeper, "accepted")

    def test_old_macos_is_not_ready(self):
        env = macos.preflight(
            runner=matrix_runner(sw_vers=(0, "13.6\n")),
            app_candidates=("/Applications/GramDrive.app",),
            exists=lambda p: True,
        )
        self.assertFalse(env.ready)
        self.assertTrue(any("below the v1 minimum" in r for r in env.reasons))

    def test_non_arm_is_not_ready(self):
        env = macos.preflight(
            runner=matrix_runner(uname=(0, "x86_64\n")),
            app_candidates=("/Applications/GramDrive.app",),
            exists=lambda p: True,
        )
        self.assertFalse(env.ready)
        self.assertTrue(any("x86_64" in r for r in env.reasons))

    def test_missing_build_is_not_ready(self):
        env = macos.preflight(
            runner=matrix_runner(),
            app_candidates=("/Applications/GramDrive.app",),
            exists=lambda p: False,
        )
        self.assertFalse(env.ready)
        self.assertIsNone(env.app_path)
        self.assertTrue(any("no GramDrive.app" in r for r in env.reasons))

    def test_bad_signature_is_not_ready_but_gatekeeper_is_only_recorded(self):
        env = macos.preflight(
            runner=matrix_runner(
                **{"codesign --verify": (1, "invalid\n"), "spctl --assess": (1, "rejected\n")}
            ),
            app_candidates=("/Applications/GramDrive.app",),
            exists=lambda p: p == "/Applications/GramDrive.app",
        )
        self.assertFalse(env.ready)
        self.assertFalse(env.signature_valid)
        # An un-notarized Developer ID build is legitimately rejected by
        # Gatekeeper; that is recorded, not a readiness gate.
        self.assertEqual(env.gatekeeper, "rejected")
        self.assertFalse(any("spctl" in r.lower() for r in env.reasons))

    def test_non_macos_host_is_not_ready(self):
        env = macos.preflight(
            runner=FakeRunner({"sw_vers": (127, "not found\n"), "uname": (0, "x86_64\n")}),
            app_candidates=(),
            exists=lambda p: False,
        )
        self.assertFalse(env.ready)
        self.assertTrue(any("not a macOS host" in r for r in env.reasons))


class ProbeExecutionTests(unittest.TestCase):
    def _env(self, app_path="/Applications/GramDrive.app"):
        env = macos.Environment()
        env.app_path = app_path
        return env

    def test_asserted_probe_passes_when_domain_present(self):
        probe = macos.build_catalog()[0].probes[0]  # registration domain-present
        with tempfile.TemporaryDirectory() as tmp:
            result = macos.run_probe(
                probe,
                scenario_key="registration",
                env=self._env(),
                runner=matrix_runner(),
                out_dir=Path(tmp),
                write=True,
            )
        self.assertEqual(result.status, "pass")

    def test_asserted_probe_fails_when_domain_absent(self):
        probe = macos.build_catalog()[0].probes[0]
        with tempfile.TemporaryDirectory() as tmp:
            result = macos.run_probe(
                probe,
                scenario_key="registration",
                env=self._env(),
                runner=matrix_runner(**{"fileproviderctl dump": (0, "no domains\n")}),
                out_dir=Path(tmp),
                write=True,
            )
        self.assertEqual(result.status, "fail")

    def test_app_probe_is_skipped_when_no_bundle_located(self):
        app_probe = macos.Probe(
            name="needs-app",
            argv=("codesign", "-dv", macos.APP_TOKEN),
            purpose="x",
            assertion=macos.assert_zero("signed"),
        )
        env = macos.Environment()  # app_path is None
        with tempfile.TemporaryDirectory() as tmp:
            result = macos.run_probe(
                app_probe,
                scenario_key="s",
                env=env,
                runner=matrix_runner(),
                out_dir=Path(tmp),
                write=True,
            )
        self.assertEqual(result.status, "skipped")

    def test_evidence_probe_is_captured_not_judged(self):
        probe = macos._dump_probe()
        with tempfile.TemporaryDirectory() as tmp:
            result = macos.run_probe(
                probe,
                scenario_key="enumeration",
                env=self._env(),
                runner=matrix_runner(),
                out_dir=Path(tmp),
                write=True,
            )
            self.assertEqual(result.status, "captured")
            self.assertTrue((Path(tmp) / result.log_name).exists())


class PreparedRunTests(unittest.TestCase):
    def test_prepared_run_writes_docs_and_summary(self):
        with tempfile.TemporaryDirectory() as tmp:
            summary, code = macos.prepare_run(
                run_id="unit-prepared",
                repo_root=Path(tmp),
                catalog=macos.build_catalog(),
                runner=matrix_runner(),
                app_candidates=("/Applications/GramDrive.app",),
                exists=lambda p: p == "/Applications/GramDrive.app",
                echo=lambda _m: None,
            )
            out = Path(tmp) / macos.PROVENANCE_ROOT / "unit-prepared"
            self.assertEqual(code, macos.EXIT_OK)
            self.assertTrue((out / "runsheet.md").exists())
            self.assertTrue((out / "evidence-template.md").exists())
            self.assertTrue((out / "summary.json").exists())
            written = json.loads((out / "summary.json").read_text())
            self.assertEqual(written["result"], "prepared")
            self.assertEqual(len(written["scenarios"]), 10)

    def test_prepared_run_never_reports_a_scenario_as_passed(self):
        with tempfile.TemporaryDirectory() as tmp:
            summary, _ = macos.prepare_run(
                run_id="unit-honest",
                repo_root=Path(tmp),
                catalog=macos.build_catalog(),
                runner=matrix_runner(),
                app_candidates=("/Applications/GramDrive.app",),
                exists=lambda p: p == "/Applications/GramDrive.app",
                echo=lambda _m: None,
            )
        self.assertNotEqual(summary["result"], "passed")
        for scenario in summary["scenarios"]:
            self.assertEqual(scenario["human_verdict"], "pending")
            for check in scenario["manual_checks"]:
                self.assertEqual(check["verdict"], "pending")

    def test_require_ready_refuses_a_non_matrix_host_but_still_writes_docs(self):
        with tempfile.TemporaryDirectory() as tmp:
            summary, code = macos.prepare_run(
                run_id="unit-notready",
                repo_root=Path(tmp),
                catalog=macos.build_catalog(),
                runner=matrix_runner(uname=(0, "x86_64\n")),
                app_candidates=("/Applications/GramDrive.app",),
                exists=lambda p: True,
                require_ready=True,
                echo=lambda _m: None,
            )
            out = Path(tmp) / macos.PROVENANCE_ROOT / "unit-notready"
            self.assertEqual(code, macos.EXIT_NOT_READY)
            self.assertEqual(summary["result"], "environment-not-ready")
            # Docs still land so an operator can read the run-sheet.
            self.assertTrue((out / "runsheet.md").exists())
            self.assertTrue((out / "summary.json").exists())

    def test_prepared_run_captures_preflight_logs(self):
        with tempfile.TemporaryDirectory() as tmp:
            macos.prepare_run(
                run_id="unit-preflight",
                repo_root=Path(tmp),
                catalog=macos.build_catalog(),
                runner=matrix_runner(),
                app_candidates=("/Applications/GramDrive.app",),
                exists=lambda p: p == "/Applications/GramDrive.app",
                echo=lambda _m: None,
            )
            out = Path(tmp) / macos.PROVENANCE_ROOT / "unit-preflight"
            self.assertTrue((out / "preflight.sw_vers.log").exists())
            self.assertTrue((out / "preflight.codesign.log").exists())


class DocumentTests(unittest.TestCase):
    def setUp(self):
        self.catalog = macos.build_catalog()

    def test_runsheet_covers_every_scenario_and_expected_outcomes(self):
        text = macos.render_runsheet(self.catalog, run_id="r", commit="c")
        for scenario in self.catalog:
            self.assertIn(scenario.title, text)
            self.assertIn(scenario.key, text)
            for check in scenario.manual_checks:
                self.assertIn(check.expected, text)
        # The safety rules the gate depends on must be stated.
        self.assertIn("Read-only", text)
        self.assertIn("Synthetic fixtures", text)

    def test_evidence_template_has_a_signoff_slot_per_scenario_and_overall(self):
        text = macos.render_evidence_template(self.catalog, run_id="r", commit="c")
        for scenario in self.catalog:
            self.assertIn(scenario.title, text)
        self.assertIn("Verdict", text)
        self.assertIn("Release-gate verdict", text)

    def test_list_names_every_scenario(self):
        text = macos.render_list(self.catalog)
        for scenario in self.catalog:
            self.assertIn(scenario.key, text)


class CliTests(unittest.TestCase):
    def test_list_exits_zero(self):
        self.assertEqual(macos.main(["--list"]), macos.EXIT_OK)

    def test_emit_runsheet_to_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "runsheet.md"
            code = macos.main(["--emit-runsheet", str(dest)])
            self.assertEqual(code, macos.EXIT_OK)
            self.assertIn("native acceptance run-sheet", dest.read_text())

    def test_emit_evidence_template_to_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            dest = Path(tmp) / "evidence.md"
            code = macos.main(["--emit-evidence-template", str(dest)])
            self.assertEqual(code, macos.EXIT_OK)
            self.assertIn("evidence & sign-off", dest.read_text())

    def test_bad_run_id_is_rejected(self):
        with self.assertRaises(SystemExit):
            macos.main(["--run-id", "../escape"])

    def test_run_id_required_for_a_real_run(self):
        with self.assertRaises(SystemExit):
            macos.main([])


if __name__ == "__main__":
    unittest.main()
