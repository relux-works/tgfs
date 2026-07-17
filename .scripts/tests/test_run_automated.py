#!/usr/bin/env python3
"""Tests for .scripts/acceptance/run_automated.py.

Run: python3 -m unittest discover -s .scripts/tests -t .
     (or via the gate itself: run_automated.py --suite repo --run-id local-repo)

Every test injects a fake runner, so the suite never shells out to cargo. That
keeps these tests fast and hermetic, and it lets them exercise the cases that
matter most and are hardest to stage for real: a step failing, a tool missing
from PATH, a dirty worktree.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
RUNNER_PATH = REPO_ROOT / ".scripts" / "acceptance" / "run_automated.py"


def load_runner_module():
    """Import run_automated.py by path.

    `.scripts` is not an importable package name (the leading dot is not a
    valid identifier), so a normal import cannot reach it.
    """
    spec = importlib.util.spec_from_file_location("run_automated", RUNNER_PATH)
    module = importlib.util.module_from_spec(spec)
    # Register before exec: @dataclass resolves its own module out of
    # sys.modules while the class body executes, and blows up on None.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


run_automated = load_runner_module()


class FakeRunner:
    """Records invocations and replies from a scripted table.

    Anything not in `results` succeeds silently, so a test only has to state
    the commands it actually cares about.
    """

    def __init__(self, results: dict[str, tuple[int, str]] | None = None):
        self.results = results or {}
        self.calls: list[tuple[str, ...]] = []

    def __call__(self, argv, cwd):
        argv = tuple(argv)
        self.calls.append(argv)
        for key, result in self.results.items():
            if key in " ".join(argv):
                return result
        return 0, "ok\n"

    @property
    def commands(self) -> list[str]:
        return [" ".join(argv) for argv in self.calls]


class SuiteResolutionTests(unittest.TestCase):
    def setUp(self):
        self.catalog = run_automated.build_steps()

    def test_core_suite_covers_every_gate_the_task_requires(self):
        names = [step.name for step in run_automated.resolve_suite("core", self.catalog)]
        self.assertEqual(
            names,
            ["toolchain", "format", "lint", "test", "architecture", "supply-chain"],
        )

    def test_all_suite_flattens_nested_suites_in_order(self):
        names = [step.name for step in run_automated.resolve_suite("all", self.catalog)]
        core = [step.name for step in run_automated.resolve_suite("core", self.catalog)]
        repo = [step.name for step in run_automated.resolve_suite("repo", self.catalog)]
        self.assertEqual(names, core + repo)

    def test_suite_expansion_never_repeats_a_step(self):
        for suite in run_automated.SUITES:
            names = [step.name for step in run_automated.resolve_suite(suite, self.catalog)]
            self.assertEqual(len(names), len(set(names)), f"suite {suite} repeats a step")

    def test_unknown_suite_raises(self):
        with self.assertRaises(KeyError):
            run_automated.resolve_suite("does-not-exist", self.catalog)

    def test_a_step_name_resolves_to_a_one_step_run(self):
        # Re-running a single gate must stay reachable through this entrypoint.
        steps = run_automated.resolve_suite("supply-chain", self.catalog)
        self.assertEqual([step.name for step in steps], ["supply-chain"])

    def test_suite_name_wins_over_a_same_named_step(self):
        # No collision today; this pins the precedence if one is ever added,
        # so a new step named "core" cannot quietly shrink the core gate.
        for suite in run_automated.SUITES:
            if suite in self.catalog:
                steps = run_automated.resolve_suite(suite, self.catalog)
                self.assertGreater(len(steps), 1, f"{suite} resolved as a step")

    def test_every_suite_references_only_real_steps(self):
        for suite, members in run_automated.SUITES.items():
            for member in members:
                self.assertTrue(
                    member in self.catalog or member in run_automated.SUITES,
                    f"suite {suite} references unknown member {member}",
                )

    def test_every_step_is_reachable_from_a_suite(self):
        reachable = {
            step.name
            for suite in run_automated.SUITES
            for step in run_automated.resolve_suite(suite, self.catalog)
        }
        self.assertEqual(reachable, set(self.catalog), "a step no suite runs is dead config")

    def test_lint_step_makes_warnings_errors(self):
        # The lint gate is only a gate because of `-D warnings`; without it,
        # every warn-level lint in [workspace.lints] is advisory.
        lint = self.catalog["lint"]
        self.assertIn("-D", lint.argv)
        self.assertIn("warnings", lint.argv)

    def test_format_step_checks_rather_than_rewrites(self):
        # `cargo fmt` without --check would silently mutate the tree and pass.
        self.assertIn("--check", self.catalog["format"].argv)


class RunSuiteTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo_root = Path(self.tmp.name)
        self.catalog = run_automated.build_steps()
        self.addCleanup(self.tmp.cleanup)

    def run_steps(self, step_names, runner, *, require_clean=False, run_id="test-run"):
        steps = [self.catalog[name] for name in step_names]
        return run_automated.run_suite(
            steps,
            repo_root=self.repo_root,
            run_id=run_id,
            suite="core",
            require_clean=require_clean,
            runner=runner,
            echo=lambda _message: None,
        )

    def test_all_steps_passing_exits_zero(self):
        summary, code = self.run_steps(["format", "test"], FakeRunner())
        self.assertEqual(code, run_automated.EXIT_OK)
        self.assertEqual(summary["result"], "passed")
        self.assertEqual([s["status"] for s in summary["steps"]], ["passed", "passed"])

    def test_failing_step_exits_one(self):
        runner = FakeRunner({"cargo fmt": (1, "Diff in lib.rs\n")})
        summary, code = self.run_steps(["format", "test"], runner)
        self.assertEqual(code, run_automated.EXIT_FAILED)
        self.assertEqual(summary["result"], "failed")

    def test_later_steps_still_run_after_a_failure(self):
        # A gate that stops at the first failure turns one push into three.
        runner = FakeRunner({"cargo fmt": (1, "Diff in lib.rs\n")})
        summary, _ = self.run_steps(["format", "test", "architecture"], runner)
        self.assertEqual(
            [s["status"] for s in summary["steps"]], ["failed", "passed", "passed"]
        )
        self.assertIn("cargo test --workspace --all-features", runner.commands)

    def test_missing_tool_fails_the_step_instead_of_crashing_the_runner(self):
        def exploding_runner(argv, cwd):
            return run_automated.default_runner(("definitely-not-a-real-binary",), cwd)

        summary, code = self.run_steps(["supply-chain"], exploding_runner)
        self.assertEqual(code, run_automated.EXIT_FAILED)
        self.assertEqual(summary["steps"][0]["exit_code"], 127)

    def test_provenance_records_commit_and_worktree_state(self):
        runner = FakeRunner(
            {
                "git rev-parse HEAD": (0, "abc123def\n"),
                "git status --porcelain": (0, ""),
            }
        )
        summary, _ = self.run_steps(["format"], runner)
        self.assertEqual(summary["commit"], "abc123def")
        self.assertTrue(summary["worktree_clean"])

    def test_provenance_writes_summary_and_per_step_logs(self):
        runner = FakeRunner({"cargo fmt": (1, "Diff in lib.rs\n")})
        self.run_steps(["format", "test"], runner)

        out_dir = run_automated.provenance_dir(self.repo_root, "test-run")
        written = json.loads((out_dir / "summary.json").read_text())
        self.assertEqual(written["run_id"], "test-run")
        self.assertEqual(written["result"], "failed")
        # The log is the artifact CI uploads; it has to hold the real output.
        self.assertEqual((out_dir / "format.log").read_text(), "Diff in lib.rs\n")
        self.assertTrue((out_dir / "test.log").exists())

    def test_summary_records_each_command_verbatim(self):
        # Provenance has to say what actually ran, not what a suite name implies.
        summary, _ = self.run_steps(["lint"], FakeRunner())
        self.assertEqual(
            summary["steps"][0]["command"], list(self.catalog["lint"].argv)
        )

    def test_rerunning_a_run_id_discards_the_previous_logs(self):
        self.run_steps(["format", "test"], FakeRunner())
        # Second run covers fewer steps; the first run's test.log must not
        # survive to be read as evidence about this run.
        self.run_steps(["format"], FakeRunner())

        out_dir = run_automated.provenance_dir(self.repo_root, "test-run")
        self.assertTrue((out_dir / "format.log").exists())
        self.assertFalse((out_dir / "test.log").exists())

    def test_require_clean_refuses_a_dirty_worktree(self):
        runner = FakeRunner({"git status --porcelain": (0, " M Cargo.toml\n")})
        summary, code = self.run_steps(["format"], runner, require_clean=True)

        self.assertEqual(code, run_automated.EXIT_CANNOT_START)
        self.assertEqual(summary["result"], "cannot-start")
        self.assertFalse(summary["worktree_clean"])
        self.assertIn("uncommitted changes", summary["error"])

    def test_require_clean_refusal_runs_no_steps(self):
        runner = FakeRunner({"git status --porcelain": (0, " M Cargo.toml\n")})
        self.run_steps(["format"], runner, require_clean=True)
        self.assertNotIn("cargo fmt --all --check", runner.commands)

    def test_require_clean_refusal_is_still_recorded(self):
        runner = FakeRunner({"git status --porcelain": (0, " M Cargo.toml\n")})
        self.run_steps(["format"], runner, require_clean=True)

        out_dir = run_automated.provenance_dir(self.repo_root, "test-run")
        written = json.loads((out_dir / "summary.json").read_text())
        self.assertEqual(written["result"], "cannot-start")

    def test_dirty_worktree_runs_normally_without_require_clean(self):
        runner = FakeRunner({"git status --porcelain": (0, " M Cargo.toml\n")})
        summary, code = self.run_steps(["format"], runner, require_clean=False)
        self.assertEqual(code, run_automated.EXIT_OK)
        self.assertFalse(summary["worktree_clean"])

    def test_tool_versions_are_recorded_and_absence_is_explicit(self):
        runner = FakeRunner({"cargo deny --version": (127, "not found")})
        summary, _ = self.run_steps(["format"], runner)
        self.assertEqual(summary["tools"]["cargo-deny"], "unavailable")
        self.assertIn("python", summary["tools"])


class ArgumentTests(unittest.TestCase):
    def invoke(self, argv):
        """Call main() with stdout/stderr captured, returning its exit code."""
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            return run_automated.main(argv)

    def expect_rejected(self, argv):
        with self.assertRaises(SystemExit) as caught:
            self.invoke(argv)
        # argparse.error() exits 2 — the same "cannot start" code the runner
        # uses for a refused run.
        self.assertEqual(caught.exception.code, run_automated.EXIT_CANNOT_START)

    def test_list_exits_zero_without_a_suite(self):
        self.assertEqual(self.invoke(["--list"]), run_automated.EXIT_OK)

    def test_suite_without_run_id_is_rejected(self):
        self.expect_rejected(["--suite", "core"])

    def test_run_id_without_suite_is_rejected(self):
        self.expect_rejected(["--run-id", "local-core"])

    def test_unknown_suite_is_rejected(self):
        self.expect_rejected(["--suite", "nope", "--run-id", "x"])

    def test_run_id_cannot_escape_the_provenance_directory(self):
        # --run-id names a directory; a traversal would write outside .temp/.
        for bad in ("../escape", "a/b", "/absolute", "", ".hidden", "x" * 65):
            with self.subTest(run_id=bad):
                self.expect_rejected(["--suite", "core", "--run-id", bad])

    def test_realistic_run_ids_are_accepted(self):
        for good in ("ci-core", "local-core", "ci-all", "pr-1234", "v0.1.0_core"):
            with self.subTest(run_id=good):
                self.assertIsNotNone(run_automated.RUN_ID_RE.match(good))


class CatalogDocumentationTests(unittest.TestCase):
    def test_list_output_names_every_suite_and_step(self):
        text = run_automated.format_catalog()
        for suite in run_automated.SUITES:
            self.assertIn(suite, text)
        for name in run_automated.build_steps():
            self.assertIn(name, text)

    def test_every_step_documents_its_purpose(self):
        for step in run_automated.build_steps().values():
            self.assertTrue(step.purpose.strip(), f"{step.name} has no stated purpose")


if __name__ == "__main__":
    unittest.main()
