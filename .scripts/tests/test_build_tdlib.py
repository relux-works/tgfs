#!/usr/bin/env python3
"""Tests for .scripts/tdlib/build_tdlib.py.

Run: python3 -m unittest discover -s .scripts/tests -t .scripts/tests
     (or via the gate: run_automated.py --suite repo --run-id local-repo)

No test shells out to git, cmake, cargo or otool: a fake runner stands in for
every subprocess, so the suite is fast and runs on a machine without Xcode,
cmake, or a network. That buys coverage of the cases the real pipeline cannot be
asked to stage on demand -- a fetch that must be skipped because the pin is
already checked out, a build that reports success but writes no dylib, a smoke
run that prints nothing parseable.

The properties worth protecting are the ones a reader of the artifact cannot
check for themselves: that the pin is the single source of the checkout, that
the reproducibility flags are actually passed, that only the tdjson target is
built, that the license is required to be present (POL-6), and that the
manifest's reproducibility claim is derived from what the build did rather than
asserted.
"""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT_PATH = REPO_ROOT / ".scripts" / "tdlib" / "build_tdlib.py"


def load_module():
    """Import build_tdlib.py by path (`.scripts` is not a package)."""
    spec = importlib.util.spec_from_file_location("build_tdlib", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


tdlib = load_module()


class FakeRunner:
    """Records every invocation and replies from a scripted table.

    Matching is by substring against the joined argv, so a test states only the
    command it cares about. Anything unmatched succeeds with empty output.
    """

    def __init__(self, results: dict[str, tuple[int, str]] | None = None):
        self.results = results or {}
        self.calls: list[tuple[str, ...]] = []
        self.envs: list[dict[str, str] | None] = []

    def __call__(self, argv, cwd, env=None):
        argv = tuple(str(a) for a in argv)
        self.calls.append(argv)
        self.envs.append(env)
        joined = " ".join(argv)
        for needle, result in self.results.items():
            if needle in joined:
                return result
        return 0, ""

    def argv_containing(self, needle: str) -> tuple[str, ...]:
        for argv in self.calls:
            if needle in " ".join(argv):
                return argv
        raise AssertionError(f"no call containing {needle!r}; calls: {self.calls}")

    def has_call(self, needle: str) -> bool:
        return any(needle in " ".join(argv) for argv in self.calls)

    def env_for(self, needle: str) -> dict[str, str] | None:
        for argv, env in zip(self.calls, self.envs):
            if needle in " ".join(argv):
                return env
        raise AssertionError(f"no call containing {needle!r}")


def builder_in(tmp: Path, runner: FakeRunner, environ=None) -> "tdlib.TdlibBuilder":
    return tdlib.TdlibBuilder(
        repo_root=tmp,
        out_dir=tmp / ".temp" / "tdlib",
        runner=runner,
        jobs=4,
        environ=environ if environ is not None else {},
    )


def make_src_tree(src: Path, *, version: str = "1.8.51") -> None:
    """A minimal TDLib source checkout: the headers, license and CMakeLists."""
    (src / "td" / "telegram").mkdir(parents=True, exist_ok=True)
    for name in tdlib.PUBLIC_HEADERS:
        (src / "td" / "telegram" / name).write_text(f"// {name}\n")
    (src / tdlib.LICENSE_SRC_NAME).write_text("Boost Software License - Version 1.0\n")
    (src / "CMakeLists.txt").write_text(
        f"cmake_minimum_required(VERSION 3.10)\nproject(TDLib VERSION {version} LANGUAGES CXX C)\n"
    )


def make_build_tree(build: Path, *, with_dylib: bool = True) -> None:
    """A minimal cmake build tree: the dylib and the generated export header."""
    build.mkdir(parents=True, exist_ok=True)
    if with_dylib:
        (build / tdlib.DYLIB_NAME).write_bytes(b"\xcf\xfa\xed\xfe fake dylib bytes")
    gen_dir = build / "td" / "telegram"
    gen_dir.mkdir(parents=True, exist_ok=True)
    (gen_dir / tdlib.GENERATED_HEADER).write_text("// tdjson_export.h\n")


class PinTests(unittest.TestCase):
    def test_pin_is_a_full_commit_hash(self):
        self.assertRegex(tdlib.TDLIB_COMMIT, r"^[0-9a-f]{40}$")


class FetchTests(unittest.TestCase):
    def test_fetch_skips_network_when_pin_already_checked_out(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner({"rev-parse HEAD": (0, tdlib.TDLIB_COMMIT + "\n")})
            b = builder_in(tmp, runner)
            (b.src_dir / ".git").mkdir(parents=True)
            b.fetch()
            self.assertFalse(
                runner.has_call("git fetch"),
                "an already-pinned checkout must not touch the network",
            )
            self.assertFalse(runner.has_call("git checkout"))

    def test_fetch_clones_and_checks_out_the_pin_when_absent(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner()
            b = builder_in(tmp, runner)
            b.fetch()
            self.assertTrue(runner.has_call("git init"))
            fetch = runner.argv_containing("fetch")
            self.assertIn(tdlib.TDLIB_COMMIT, fetch)
            self.assertIn("--depth", fetch)
            checkout = runner.argv_containing("checkout")
            self.assertIn(tdlib.TDLIB_COMMIT, checkout)


class ToolTests(unittest.TestCase):
    def test_require_tools_names_the_missing_one(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner({"gperf --version": (127, "not found\n")})
            b = builder_in(tmp, runner)
            with self.assertRaises(tdlib.StepFailed) as ctx:
                b.require_tools()
            self.assertIn("gperf", str(ctx.exception))

    def test_openssl_root_prefers_env_override(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner()
            b = builder_in(tmp, runner, environ={"OPENSSL_ROOT_DIR": "/opt/custom/ssl"})
            self.assertEqual(b.openssl_root(), Path("/opt/custom/ssl"))
            self.assertFalse(runner.has_call("brew"), "override must not call brew")

    def test_openssl_root_resolves_via_brew(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner({"brew --prefix openssl@3": (0, "/opt/homebrew/opt/openssl@3\n")})
            b = builder_in(tmp, runner)
            self.assertEqual(b.openssl_root(), Path("/opt/homebrew/opt/openssl@3"))

    def test_openssl_root_fails_clearly_when_absent(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner({"brew --prefix": (1, "No available formula\n")})
            b = builder_in(tmp, runner)
            with self.assertRaises(tdlib.StepFailed) as ctx:
                b.openssl_root()
            self.assertIn("OpenSSL", str(ctx.exception))


class BuildTests(unittest.TestCase):
    def _prepared(self, tmp: Path, runner: FakeRunner):
        b = builder_in(tmp, runner, environ={"OPENSSL_ROOT_DIR": "/opt/ssl"})
        make_src_tree(b.src_dir)
        return b

    def test_configure_passes_reproducibility_and_release_flags(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner()
            b = self._prepared(tmp, runner)
            record = b.configure_and_build(clean=True)

            configure = runner.argv_containing("cmake -S")
            self.assertIn("-DCMAKE_BUILD_TYPE=Release", configure)
            self.assertIn("-DOPENSSL_ROOT_DIR=/opt/ssl", configure)
            self.assertTrue(
                any(a.startswith("-DCMAKE_CXX_FLAGS=-ffile-prefix-map=") for a in configure),
                f"missing -ffile-prefix-map in {configure}",
            )
            self.assertTrue(
                any("CMAKE_OSX_DEPLOYMENT_TARGET=14.0" in a for a in configure)
            )
            env = runner.env_for("cmake -S")
            self.assertEqual(env.get("ZERO_AR_DATE"), "1")

            self.assertEqual(record.build_type, "Release")
            self.assertTrue(record.clean_build_tree)
            self.assertTrue(record.deterministic_archives)
            self.assertEqual(record.remapped_to, "/tdlib")

    def test_build_targets_only_tdjson(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner()
            b = self._prepared(tmp, runner)
            b.configure_and_build(clean=True)
            build = runner.argv_containing("cmake --build")
            self.assertIn("tdjson", build)
            self.assertNotIn("install", build)
            self.assertNotIn("all", build)


class StageTests(unittest.TestCase):
    def test_stage_fails_when_build_wrote_no_dylib(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            b = builder_in(tmp, FakeRunner())
            make_src_tree(b.src_dir)
            make_build_tree(b.build_dir, with_dylib=False)
            with self.assertRaises(tdlib.StepFailed) as ctx:
                b.stage()
            self.assertIn("missing", str(ctx.exception).lower())

    def test_stage_requires_the_license(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            b = builder_in(tmp, FakeRunner())
            make_src_tree(b.src_dir)
            (b.src_dir / tdlib.LICENSE_SRC_NAME).unlink()
            make_build_tree(b.build_dir)
            with self.assertRaises(tdlib.StepFailed) as ctx:
                b.stage()
            self.assertIn("license", str(ctx.exception).lower())

    def test_stage_collects_library_headers_and_license(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner()
            b = builder_in(tmp, runner)
            make_src_tree(b.src_dir)
            make_build_tree(b.build_dir)
            b.stage()

            self.assertTrue(b.lib_out.is_file())
            header_dir = b.stage_dir / tdlib.HEADER_INSTALL_SUBDIR
            for name in tdlib.PUBLIC_HEADERS:
                self.assertTrue((header_dir / name).is_file(), name)
            self.assertTrue((header_dir / tdlib.GENERATED_HEADER).is_file())
            self.assertTrue((b.stage_dir / tdlib.LICENSE_SRC_NAME).is_file())
            # The install name is normalized to @rpath.
            self.assertTrue(runner.has_call("install_name_tool"))
            self.assertIn(tdlib.DYLIB_INSTALL_NAME, runner.argv_containing("install_name_tool"))


class SmokeVersionTests(unittest.TestCase):
    def test_parse_smoke_version_reads_the_labeled_line(self):
        out = "created client id 1\nTDLib version: 1.8.51\nbye\n"
        self.assertEqual(tdlib.parse_smoke_version(out), "1.8.51")

    def test_parse_smoke_version_returns_none_without_the_label(self):
        self.assertIsNone(tdlib.parse_smoke_version("no version printed here"))

    def test_smoke_version_uses_the_running_library(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner({"cargo run": (0, "TDLib version: 1.8.51\n")})
            b = builder_in(tmp, runner)
            self.assertEqual(b.smoke_version(), "1.8.51")
            run = runner.argv_containing("cargo run")
            env = runner.env_for("cargo run")
            self.assertIn(tdlib.SMOKE_ARTIFACT_ENV, env)

    def test_smoke_version_fails_when_nothing_parseable(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner({"cargo run": (0, "built and ran, printed nothing\n")})
            b = builder_in(tmp, runner)
            with self.assertRaises(tdlib.StepFailed):
                b.smoke_version()


class ManifestTests(unittest.TestCase):
    def _staged_builder(self, tmp: Path, runner: FakeRunner):
        b = builder_in(tmp, runner)
        make_src_tree(b.src_dir, version="1.8.51")
        make_build_tree(b.build_dir)
        b.stage()
        return b

    def test_manifest_records_the_pin_and_license(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            b = self._staged_builder(tmp, FakeRunner())
            record = tdlib.BuildRecord(True, "/tdlib", "Release", True)
            manifest = b.build_manifest(record, "1.8.51")

            self.assertEqual(manifest["tdlib"]["commit"], tdlib.TDLIB_COMMIT)
            self.assertEqual(manifest["tdlib"]["runtime_version"], "1.8.51")
            self.assertEqual(manifest["tdlib"]["source_version"], "1.8.51")
            self.assertEqual(manifest["license"]["id"], "BSL-1.0")
            self.assertEqual(manifest["target"]["label"], "macos-arm64")
            self.assertIn("sha256", manifest["artifacts"]["library"])

    def test_path_independent_is_derived_from_the_build(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            b = self._staged_builder(tmp, FakeRunner())
            clean = tdlib.BuildRecord(True, "/tdlib", "Release", True)
            dirty = tdlib.BuildRecord(False, "/tdlib", "Release", True)
            self.assertTrue(b.build_manifest(clean, "1.8.51")["reproducibility"]["path_independent"])
            self.assertFalse(b.build_manifest(dirty, "1.8.51")["reproducibility"]["path_independent"])

    def test_manifest_and_checksums_exclude_themselves(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            b = self._staged_builder(tmp, FakeRunner())
            record = tdlib.BuildRecord(True, "/tdlib", "Release", True)
            manifest = b.build_manifest(record, "1.8.51")
            b.write_manifest_and_checksums(manifest)

            files = manifest["artifacts"]["files"]
            self.assertNotIn(tdlib.MANIFEST_NAME, files)
            self.assertNotIn(tdlib.CHECKSUMS_NAME, files)
            checks = (b.stage_dir / tdlib.CHECKSUMS_NAME).read_text()
            self.assertNotIn(tdlib.CHECKSUMS_NAME, checks)
            self.assertNotIn(tdlib.MANIFEST_NAME, checks)
            self.assertIn("lib/" + tdlib.DYLIB_NAME, checks)


class HostGuardTests(unittest.TestCase):
    def test_non_darwin_host_is_rejected(self):
        original = tdlib.sys.platform
        try:
            tdlib.sys.platform = "linux"
            ok, reason = tdlib.host_supported()
            self.assertFalse(ok)
            self.assertIn("macOS", reason)
        finally:
            tdlib.sys.platform = original


if __name__ == "__main__":
    unittest.main()
