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
        if argv[:2] == ("otool", "-L"):
            return (
                0,
                f"{argv[2]}:\n"
                f"\t{tdlib.DYLIB_INSTALL_NAME} (compatibility version 1.0.0, current version 1.8.0)\n"
                "\t/usr/lib/libz.1.dylib (compatibility version 1.0.0, current version 1.2.12)\n"
                "\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1319.0.0)\n",
            )
        if argv[-1:] == ("version",) and "openssl" in argv[0]:
            return 0, "OpenSSL 3.6.3 9 Jun 2026\n"
        if argv[0].endswith("trust-probe"):
            return (
                0,
                "openssl=OpenSSL 3.6.3 9 Jun 2026 cert_file=/etc/ssl/cert.pem "
                "cert_objects=158\n",
            )
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
    b = tdlib.TdlibBuilder(
        repo_root=tmp,
        out_dir=tmp / ".temp" / "tdlib",
        runner=runner,
        jobs=4,
        environ=environ or {},
    )
    b.openssl_source_dir.mkdir(parents=True, exist_ok=True)
    (b.openssl_source_dir / tdlib.OPENSSL_LICENSE_SRC_NAME).write_text(
        "Apache License\nVersion 2.0, January 2004\n"
    )
    (b.openssl_prefix / "include" / "openssl").mkdir(parents=True, exist_ok=True)
    (b.openssl_prefix / "include" / "openssl" / "ssl.h").write_text("// ssl\n")
    (b.openssl_prefix / "lib").mkdir(parents=True, exist_ok=True)
    (b.openssl_prefix / "lib" / "libssl.a").write_bytes(b"ssl")
    (b.openssl_prefix / "lib" / "libcrypto.a").write_bytes(b"crypto")
    return b


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
        self.assertRegex(tdlib.TDLIB_SOURCE_DATE_EPOCH, r"^[0-9]+$")

    def test_openssl_source_is_exactly_pinned(self):
        self.assertEqual(tdlib.OPENSSL_VERSION, "3.6.3")
        self.assertRegex(tdlib.OPENSSL_SOURCE_SHA256, r"^[0-9a-f]{64}$")
        self.assertIn("openssl-3.6.3.tar.gz", tdlib.OPENSSL_SOURCE_URL)


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

    def test_openssl_archive_override_still_requires_pinned_digest(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner()
            archive = tmp / "openssl.tar.gz"
            archive.write_bytes(b"not the pinned source")
            b = builder_in(
                tmp, runner, environ={tdlib.OPENSSL_ARCHIVE_ENV: str(archive)}
            )
            with self.assertRaisesRegex(tdlib.StepFailed, "checksum mismatch"):
                b.fetch_openssl_archive()

    def test_trust_probe_scrubs_overrides_and_requires_system_cert_objects(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner()
            b = builder_in(
                tmp,
                runner,
                environ={
                    "PATH": "/usr/bin",
                    "HOME": "/clean",
                    "SSL_CERT_FILE": "/opt/homebrew/etc/cert.pem",
                    "OPENSSL_MODULES": "/opt/homebrew/modules",
                    "HOMEBREW_PREFIX": "/opt/homebrew",
                },
            )
            record = b.verify_system_trust_store()
            self.assertTrue(record["verified"])
            self.assertEqual(record["cert_file"], "/etc/ssl/cert.pem")
            env = next(
                env
                for argv, env in zip(runner.calls, runner.envs)
                if len(argv) == 1 and argv[0].endswith("trust-probe")
            )
            self.assertEqual(env["PATH"], "/usr/bin")
            self.assertEqual(env["HOME"], "/clean")
            self.assertNotIn("SSL_CERT_FILE", env)
            self.assertNotIn("OPENSSL_MODULES", env)
            self.assertNotIn("HOMEBREW_PREFIX", env)


class BuildTests(unittest.TestCase):
    def _prepared(self, tmp: Path, runner: FakeRunner):
        b = builder_in(tmp, runner)
        b.build_openssl = lambda *, clean: Path("/recipe/openssl/usr")
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
            self.assertIn("-DOPENSSL_ROOT_DIR=/recipe/openssl/usr", configure)
            self.assertIn("-DOPENSSL_USE_STATIC_LIBS=TRUE", configure)
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

    def test_openssl_build_is_static_pinned_and_uses_portable_defaults(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            runner = FakeRunner()
            b = builder_in(
                tmp,
                runner,
                environ={"OPENSSL_LOCAL_CONFIG_DIR": "/untrusted/local"},
            )
            b.prepare_openssl_source = lambda *, clean: b.openssl_source_dir
            self.assertEqual(b.build_openssl(clean=False), b.openssl_prefix)
            configure = runner.argv_containing("./Configure")
            self.assertIn("--prefix=/usr", configure)
            self.assertIn("--openssldir=/etc/ssl", configure)
            for option in tdlib.OPENSSL_BUILD_OPTIONS:
                self.assertIn(option, configure)
            self.assertIn("darwin64-arm64-cc", configure)
            self.assertIn("-ffile-prefix-map=.=/openssl", configure)
            self.assertNotIn(
                "OPENSSL_LOCAL_CONFIG_DIR", runner.env_for("./Configure")
            )
            self.assertEqual(
                runner.env_for("./Configure")["SOURCE_DATE_EPOCH"],
                tdlib.TDLIB_SOURCE_DATE_EPOCH,
            )

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

    def test_stage_requires_the_static_openssl_license(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            b = builder_in(tmp, FakeRunner())
            make_src_tree(b.src_dir)
            make_build_tree(b.build_dir)
            (b.openssl_source_dir / tdlib.OPENSSL_LICENSE_SRC_NAME).unlink()
            with self.assertRaisesRegex(tdlib.StepFailed, "OpenSSL license"):
                b.stage()

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
            self.assertTrue((b.stage_dir / tdlib.OPENSSL_LICENSE_PATH).is_file())
            # The install name is normalized to @rpath.
            self.assertTrue(runner.has_call("install_name_tool"))
            self.assertIn(tdlib.DYLIB_INSTALL_NAME, runner.argv_containing("install_name_tool"))

    def test_stage_rejects_builder_local_dynamic_openssl(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            output = (
                "libtdjson.dylib:\n"
                f"\t{tdlib.DYLIB_INSTALL_NAME} (compatibility version 1.0.0, current version 1.8.0)\n"
                "\t/opt/homebrew/opt/openssl@3/lib/libssl.3.dylib "
                "(compatibility version 3.0.0, current version 3.6.0)\n"
            )
            b = builder_in(tmp, FakeRunner({"otool -L": (0, output)}))
            make_src_tree(b.src_dir)
            make_build_tree(b.build_dir)
            with self.assertRaisesRegex(tdlib.StepFailed, "OpenSSL must be linked statically"):
                b.stage()

    def test_stage_rejects_non_system_builder_absolute_dependency(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            output = (
                "libtdjson.dylib:\n"
                f"\t{tdlib.DYLIB_INSTALL_NAME} (compatibility version 1.0.0, current version 1.8.0)\n"
                "\t/Users/builder/local/libcustom.dylib "
                "(compatibility version 1.0.0, current version 1.0.0)\n"
            )
            b = builder_in(tmp, FakeRunner({"otool -L": (0, output)}))
            make_src_tree(b.src_dir)
            make_build_tree(b.build_dir)
            with self.assertRaisesRegex(tdlib.StepFailed, "non-hermetic"):
                b.stage()

    def test_stage_rejects_compiled_homebrew_defaults_with_clean_load_commands(self):
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            b = builder_in(tmp, FakeRunner())
            make_src_tree(b.src_dir)
            make_build_tree(b.build_dir)
            (b.build_dir / tdlib.DYLIB_NAME).write_bytes(
                b"Mach-O\0/opt/homebrew/etc/openssl@3/cert.pem\0"
            )
            with self.assertRaisesRegex(tdlib.StepFailed, "builder-local"):
                b.stage()


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
            openssl = manifest["third_party"]["openssl"]
            self.assertEqual(openssl["version"], "3.6.3")
            self.assertEqual(
                openssl["source"]["sha256"], tdlib.OPENSSL_SOURCE_SHA256
            )
            self.assertEqual(openssl["license"]["id"], "Apache-2.0")
            self.assertEqual(
                openssl["license"]["file"], tdlib.OPENSSL_LICENSE_PATH.as_posix()
            )
            self.assertEqual(
                openssl["license"]["sha256"],
                tdlib.sha256_file(b.stage_dir / tdlib.OPENSSL_LICENSE_PATH),
            )
            self.assertEqual(manifest["target"]["label"], "macos-arm64")
            self.assertIn("sha256", manifest["artifacts"]["library"])
            self.assertEqual(manifest["runtime"]["openssl_linkage"], "static")
            self.assertEqual(
                manifest["runtime"]["dependency_policy"],
                tdlib.RUNTIME_DEPENDENCY_POLICY,
            )
            self.assertEqual(manifest["runtime"]["dependencies"], manifest["linkage"])
            self.assertTrue(manifest["runtime"]["forbidden_builder_paths_verified"])
            self.assertEqual(
                manifest["runtime"]["trust_store"]["cert_file"],
                "/etc/ssl/cert.pem",
            )

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
            self.assertIn(tdlib.OPENSSL_LICENSE_PATH.as_posix(), checks)


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
