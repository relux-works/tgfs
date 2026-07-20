#!/usr/bin/env python3
"""Tests for .scripts/packaging/build_core_artifacts.py.

Run: python3 -m unittest discover -s .scripts/tests -t .scripts/tests
     (or via the gate: run_automated.py --suite repo --run-id local-repo)

No test shells out to cargo, xcodebuild or swift: a fake runner stands in for
every subprocess, so the suite is fast and runs on a machine without Xcode. What
that buys is coverage of the cases the real pipeline cannot be asked to stage on
demand -- a build that reports success but writes no library, a verifier that
prints nothing parseable, a bindgen run that renames its output.

The properties worth protecting here are the ones a reader of the artifact
cannot check for themselves: that the reproducibility flags are actually passed,
that the LTO-restoring crate-type override is actually used, and that the
manifest's contract version can only come from the built binary.
"""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT_PATH = REPO_ROOT / ".scripts" / "packaging" / "build_core_artifacts.py"


def load_module():
    """Import build_core_artifacts.py by path (`.scripts` is not a package)."""
    spec = importlib.util.spec_from_file_location("build_core_artifacts", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


packaging = load_module()


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


def encoded(env) -> list[str]:
    """The flags cargo will actually see, decoded."""
    return env[packaging.ENCODED_RUSTFLAGS].split(packaging.RUSTFLAG_SEPARATOR)


class RemapRustflagsTest(unittest.TestCase):
    """Half of the reproducibility guarantee: the path stays out of debug info.

    A plain build embeds absolute paths and differs between two checkouts of the
    same commit (measured: b6c393fe vs 275d96ab). These tests pin the flags.
    The other half -- a clean target directory -- is BuildRecordTest below;
    neither alone makes the build path-independent.
    """

    def test_remaps_workspace_and_cargo_home(self):
        flags = packaging.remap_rustflags(Path("/work/tgfs"), Path("/home/u/.cargo"))
        self.assertIn("--remap-path-prefix=/work/tgfs=/gramdrive", flags)
        self.assertIn("--remap-path-prefix=/home/u/.cargo=/cargo", flags)

    def test_two_checkouts_produce_identical_flags(self):
        # The point of remapping: the flag set is what makes the *output* path
        # independent, so two different checkout paths must remap onto the same
        # target prefix.
        a = packaging.remap_rustflags(Path("/Users/iv/tgfs"), Path("/home/u/.cargo"))
        b = packaging.remap_rustflags(Path("/tmp/worktree"), Path("/home/u/.cargo"))
        self.assertEqual(
            [f.split("=", 2)[2] for f in a if f.startswith("--remap")],
            [f.split("=", 2)[2] for f in b if f.startswith("--remap")],
        )

    def test_preserves_caller_rustflags(self):
        flags = packaging.remap_rustflags(Path("/w"), Path("/c"), base=["-C", "target-cpu=apple-m1"])
        self.assertEqual(flags[:2], ["-C", "target-cpu=apple-m1"])
        self.assertIn("--remap-path-prefix=/w=/gramdrive", flags)

    def test_build_env_sets_encoded_flags_and_deployment_target(self):
        env = packaging.build_env(Path("/w"), Path("/t"), {"HOME": "/home/u", "PATH": "/bin"})
        self.assertIn("--remap-path-prefix=/w=/gramdrive", encoded(env))
        self.assertIn("--remap-path-prefix=/home/u/.cargo=/cargo", encoded(env))
        self.assertEqual(env["MACOSX_DEPLOYMENT_TARGET"], packaging.MACOSX_DEPLOYMENT_TARGET)
        self.assertEqual(env["PATH"], "/bin")

    def test_build_env_honors_explicit_cargo_home(self):
        env = packaging.build_env(
            Path("/w"), Path("/t"), {"HOME": "/home/u", "CARGO_HOME": "/opt/cargo"}
        )
        self.assertIn("--remap-path-prefix=/opt/cargo=/cargo", encoded(env))
        self.assertNotIn("/home/u/.cargo", env[packaging.ENCODED_RUSTFLAGS])

    def test_build_env_points_cargo_at_the_packaging_target_dir(self):
        env = packaging.build_env(Path("/w"), Path("/out/target"), {"HOME": "/h"})
        self.assertEqual(env["CARGO_TARGET_DIR"], "/out/target")

    def test_a_path_with_spaces_survives(self):
        # The reason for the encoded form. Space-joined RUSTFLAGS would split
        # this into broken flags and lose the remap *silently* -- the build
        # still succeeds and only reproducibility quietly stops holding.
        env = packaging.build_env(
            Path("/Users/u/My Projects/tgfs"), Path("/t"), {"HOME": "/home/u"}
        )
        self.assertIn(
            "--remap-path-prefix=/Users/u/My Projects/tgfs=/gramdrive", encoded(env)
        )

    def test_rustflags_is_not_left_beside_the_encoded_form(self):
        # Cargo reads CARGO_ENCODED_RUSTFLAGS and ignores RUSTFLAGS when both
        # are set; leaving the latter behind would be a value that looks live
        # and is not.
        env = packaging.build_env(
            Path("/w"), Path("/t"), {"HOME": "/h", "RUSTFLAGS": "-C target-cpu=apple-m1"}
        )
        self.assertNotIn("RUSTFLAGS", env)

    def test_caller_rustflags_are_preserved_into_the_encoded_form(self):
        env = packaging.build_env(
            Path("/w"), Path("/t"), {"HOME": "/h", "RUSTFLAGS": "-C target-cpu=apple-m1"}
        )
        self.assertEqual(encoded(env)[:2], ["-C", "target-cpu=apple-m1"])

    def test_caller_encoded_rustflags_win_over_plain_ones(self):
        # Cargo's own precedence. Reading them the other way round would drop
        # the flags the caller actually expected to take effect.
        env = packaging.build_env(
            Path("/w"),
            Path("/t"),
            {
                "HOME": "/h",
                "RUSTFLAGS": "-C ignored=yes",
                packaging.ENCODED_RUSTFLAGS: "-C wins=yes",
            },
        )
        self.assertEqual(encoded(env)[0], "-C wins=yes")
        self.assertNotIn("-C ignored=yes", encoded(env))


class BuildRecordTest(unittest.TestCase):
    """The other half: the manifest reports the build, it does not assert one.

    Measured 2026-07-17 (LOGBOOK 0552): at one fixed path, reusing the repo's
    target/ produced bab48d50 while a fresh target directory produced 110b1b9a
    -- the value every clean build produces at every path. So path-independence
    needs both remapping and a clean target dir, and the manifest field is
    derived from what the build did rather than written as a literal.
    """

    def test_both_conditions_met_is_path_independent(self):
        record = packaging.BuildRecord(clean_target_dir=True, remapped_to=("/gramdrive",))
        self.assertTrue(packaging.reproducibility_record(record)["path_independent"])

    def test_a_reused_target_dir_is_not_path_independent(self):
        # The measured failure. A future change that reuses the target dir must
        # flip this field rather than keep asserting a property it broke.
        record = packaging.BuildRecord(clean_target_dir=False, remapped_to=("/gramdrive",))
        self.assertFalse(packaging.reproducibility_record(record)["path_independent"])

    def test_no_remapping_is_not_path_independent(self):
        record = packaging.BuildRecord(clean_target_dir=True, remapped_to=())
        self.assertFalse(packaging.reproducibility_record(record)["path_independent"])

    def test_no_build_at_all_is_not_path_independent(self):
        # A manifest for a build that never ran must not claim the property.
        self.assertFalse(packaging.reproducibility_record(None)["path_independent"])

    def test_records_the_prefixes_the_build_actually_used(self):
        env = packaging.build_env(Path("/w"), Path("/t"), {"HOME": "/home/u"})
        self.assertEqual(packaging.remapped_prefixes(env), ("/gramdrive", "/cargo"))

    def test_records_only_the_destinations_never_the_local_paths(self):
        # The manifest ships inside the zip. Recording the `<from>` side would
        # put this machine's checkout and home directory back into the artifact
        # through the metadata, undoing in JSON exactly what the remapping just
        # stripped out of the binary.
        env = packaging.build_env(
            Path("/Users/someone/Developer/tgfs"),
            Path("/t"),
            {"HOME": "/Users/someone"},
        )
        recorded = packaging.remapped_prefixes(env)
        self.assertNotIn("/Users/someone/Developer/tgfs", recorded)
        self.assertFalse([p for p in recorded if p.startswith("/Users/")])


class CargoArgvTest(unittest.TestCase):
    def test_build_overrides_crate_type_to_staticlib(self):
        # Load-bearing: cargo omits -C lto when the same invocation also emits an
        # rlib, so without this override the shipped library silently loses the
        # LTO [profile.release] asks for.
        argv = packaging.cargo_staticlib_argv("aarch64-apple-darwin")
        self.assertIn("--crate-type", argv)
        self.assertEqual(argv[argv.index("--crate-type") + 1], "staticlib")
        self.assertIn("--release", argv)
        self.assertEqual(argv[argv.index("--target") + 1], "aarch64-apple-darwin")

    def test_bindgen_reads_the_shipped_library(self):
        argv = packaging.bindgen_argv(Path("/t/libgramdrive_ffi.a"), Path("/out"))
        self.assertEqual(argv[argv.index("--library") + 1], "/t/libgramdrive_ffi.a")
        self.assertIn("swift", argv)
        # Formatters are not build requirements; their absence must not fail a
        # release build.
        self.assertIn("--no-format", argv)


class ShippedTargetsTest(unittest.TestCase):
    def test_v1_ships_macos_arm64_only(self):
        # POL-5/DEC-017. This test exists to make widening the support matrix a
        # deliberate edit with a spec change behind it, rather than a drive-by.
        self.assertEqual([s.triple for s in packaging.SLICES], ["aarch64-apple-darwin"])
        self.assertEqual([s.label for s in packaging.SLICES], ["macos-arm64"])

    def test_swift_arch_spelling_of_the_shipped_triples(self):
        # `swift build --arch` speaks Apple's arch names, cargo speaks Rust
        # triples; the cross-link verifier translates between them.
        self.assertEqual(packaging.swift_arch("aarch64-apple-darwin"), "arm64")
        self.assertEqual(packaging.swift_arch("x86_64-apple-darwin"), "x86_64")


class VerifierReportTest(unittest.TestCase):
    def test_parses_report_among_other_output(self):
        report = packaging.parse_verifier_report(
            'Compiling GramDriveCore\nBuild complete!\n{"contract_version": "0.1.0"}\n'
        )
        self.assertEqual(report["contract_version"], "0.1.0")

    def test_takes_the_last_json_object(self):
        report = packaging.parse_verifier_report(
            '{"contract_version": "0.0.1"}\n{"contract_version": "0.1.0"}'
        )
        self.assertEqual(report["contract_version"], "0.1.0")

    def test_ignores_json_without_a_contract_version(self):
        report = packaging.parse_verifier_report(
            '{"contract_version": "0.1.0"}\n{"unrelated": true}'
        )
        self.assertEqual(report["contract_version"], "0.1.0")

    def test_no_report_is_a_failure_not_a_default(self):
        # A silent default here would let the manifest claim a version nothing
        # verified, which is the exact failure this pipeline exists to prevent.
        with self.assertRaises(packaging.StepFailed):
            packaging.parse_verifier_report("Build complete!\n")

    def test_malformed_json_is_a_failure(self):
        with self.assertRaises(packaging.StepFailed):
            packaging.parse_verifier_report('{"contract_version": ')


class ChecksumTest(unittest.TestCase):
    def test_checksums_every_file_with_stable_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "sub").mkdir()
            (root / "b.txt").write_text("b")
            (root / "a.txt").write_text("a")
            (root / "sub" / "c.txt").write_text("c")
            checksums = packaging.checksum_tree(root)
            self.assertEqual(sorted(checksums), ["a.txt", "b.txt", "sub/c.txt"])
            self.assertEqual(
                checksums["a.txt"],
                "ca978112ca1bbdcafac231b39a23dc4da786eff8147c4e72b9807785afee48bb",
            )

    def test_format_is_shasum_check_compatible(self):
        rendered = packaging.format_checksums({"b.txt": "beef", "a.txt": "cafe"})
        self.assertEqual(rendered, "cafe  a.txt\nbeef  b.txt\n")


class DeterministicZipTest(unittest.TestCase):
    def test_same_content_produces_identical_bytes(self):
        # Without fixed timestamps the archive of a byte-identical artifact
        # differs per run, which would make the published checksum meaningless.
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "src"
            (source / "Sources").mkdir(parents=True)
            (source / "Package.swift").write_text("// package")
            (source / "Sources" / "a.swift").write_text("let a = 1")

            first, second = root / "1.zip", root / "2.zip"
            packaging.write_deterministic_zip(source, first, prefix="GramDriveCore")
            # Touch mtimes to prove they are not what the archive records.
            (source / "Package.swift").touch()
            packaging.write_deterministic_zip(source, second, prefix="GramDriveCore")
            self.assertEqual(packaging.sha256_file(first), packaging.sha256_file(second))

    def test_entries_are_prefixed_and_sorted(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "src"
            source.mkdir()
            (source / "z.txt").write_text("z")
            (source / "a.txt").write_text("a")
            archive_path = root / "out.zip"
            packaging.write_deterministic_zip(source, archive_path, prefix="GramDriveCore")
            with zipfile.ZipFile(archive_path) as archive:
                self.assertEqual(
                    archive.namelist(), ["GramDriveCore/a.txt", "GramDriveCore/z.txt"]
                )


class ManifestTest(unittest.TestCase):
    def manifest(self, **overrides):
        base = dict(
            contract_version="0.1.0",
            crate_version="0.1.0",
            git={"describe": "v0.1.0-2-gabc", "commit": "abc", "worktree_clean": True},
            toolchain={"rustc": "rustc 1.91.0", "uniffi": "0.32.0"},
            slices=[
                {"triple": "aarch64-apple-darwin", "label": "macos-arm64", "staticlib_bytes": 10}
            ],
            source_date="2026-07-17T00:00:00+00:00",
            reproducible=packaging.reproducibility_record(
                packaging.BuildRecord(clean_target_dir=True, remapped_to=("/gramdrive",))
            ),
        )
        base.update(overrides)
        return packaging.build_manifest(**base)

    def test_carries_the_identity_a_consumer_needs(self):
        manifest = self.manifest()
        self.assertEqual(manifest["contract_version"], "0.1.0")
        self.assertEqual(manifest["git"]["describe"], "v0.1.0-2-gabc")
        self.assertEqual(manifest["toolchain"]["uniffi"], "0.32.0")
        self.assertTrue(manifest["reproducible"]["path_independent"])

    def test_is_json_serializable(self):
        json.dumps(self.manifest())

    def test_records_a_source_date_not_a_build_time(self):
        # A wall-clock build time would land in the manifest, the manifest in the
        # zip, and the published checksum would change every run while nothing
        # about the software did.
        manifest = self.manifest()
        self.assertEqual(manifest["source_date"], "2026-07-17T00:00:00+00:00")
        self.assertNotIn("built_at", manifest)

    def test_readme_states_version_commit_and_slices(self):
        readme = packaging.render_artifact_readme(self.manifest())
        self.assertIn("0.1.0", readme)
        self.assertIn("v0.1.0-2-gabc", readme)
        self.assertIn("aarch64-apple-darwin", readme)
        # Windows/Linux consume the crate directly; that has to be findable from
        # the artifact itself, not only from the repo README.
        self.assertIn("gramdrive-ffi", readme)


class SourceDateTest(unittest.TestCase):
    def packager(self, runner, environ=None):
        return packaging.Packager(
            Path("/repo"), Path("/out"), runner=runner, echo=lambda _: None, environ=environ or {}
        )

    def test_uses_the_commit_date(self):
        runner = FakeRunner({"git log": (0, "2026-07-17T05:04:00+02:00\n")})
        self.assertEqual(self.packager(runner).source_date(), "2026-07-17T05:04:00+02:00")

    def test_source_date_epoch_wins(self):
        # The reproducible-builds convention, and what a release pipeline
        # reaching for a fixed date will already be setting.
        runner = FakeRunner({"git log": (0, "2026-07-17T05:04:00+02:00\n")})
        packager = self.packager(runner, environ={"SOURCE_DATE_EPOCH": "1700000000"})
        self.assertEqual(packager.source_date(), "2023-11-14T22:13:20+00:00")

    def test_ignores_a_malformed_source_date_epoch(self):
        runner = FakeRunner({"git log": (0, "2026-07-17T05:04:00+02:00\n")})
        packager = self.packager(runner, environ={"SOURCE_DATE_EPOCH": "not-a-number"})
        self.assertEqual(packager.source_date(), "2026-07-17T05:04:00+02:00")

    def test_no_git_yields_unknown_rather_than_a_fabricated_time(self):
        runner = FakeRunner({"git log": (128, "fatal: not a git repository\n")})
        self.assertEqual(self.packager(runner).source_date(), "unknown")

    def test_two_runs_agree(self):
        # The property the whole design exists for: nothing in the identity
        # record depends on when the build ran.
        runner = FakeRunner({"git log": (0, "2026-07-17T05:04:00+02:00\n")})
        first = self.packager(runner).source_date()
        second = self.packager(runner).source_date()
        self.assertEqual(first, second)


class ArtifactPackageSwiftTest(unittest.TestCase):
    def test_exports_only_the_swift_module(self):
        for tdjson in (False, True):
            source = packaging.artifact_package_swift(tdjson=tdjson)
            self.assertIn('.library(name: "GramDriveCore", targets: ["GramDriveCore"])', source)
            self.assertIn(
                '.binaryTarget(name: "GramDriveCoreFFI", path: "GramDriveCore.xcframework")',
                source,
            )
            # The raw C module must not be a product: it is an implementation
            # detail and a consumer importing it would bypass the contract.
            self.assertNotIn('.library(name: "GramDriveCoreFFI"', source)

    def test_declares_the_v1_platform_floor(self):
        self.assertIn(".macOS(.v14)", packaging.artifact_package_swift(tdjson=False))

    def test_tdjson_staging_declares_the_runtime_library(self):
        # The hermetic artifact must not name tdjson at all; the env-gated
        # staging must, so consumers link `-ltdjson` without unsafe flags.
        self.assertNotIn("tdjson", packaging.artifact_package_swift(tdjson=False))
        self.assertIn(
            '.linkedLibrary("tdjson")', packaging.artifact_package_swift(tdjson=True)
        )


class PrepareConsumerTest(unittest.TestCase):
    """The copy that protects the acceptance test from the source tree.

    Copying protects `.scripts/` from the build. Without the exclusion nothing
    protects the build from `.scripts/`: a .build/ left by someone running
    `swift build` there by hand would be carried into the tree the pipeline then
    builds, and stale SwiftPM state inside the artifact's own acceptance test is
    a false pass waiting to happen.
    """

    def consumer_source(self, repo: Path) -> Path:
        source = repo / ".scripts" / "packaging" / "swift-consumer"
        (source / "Sources" / "GramDriveVerify").mkdir(parents=True)
        (source / "Package.swift").write_text("// consumer")
        (source / "Sources" / "GramDriveVerify" / "main.swift").write_text("// main")
        return source

    def test_copies_the_package_sources(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, out = Path(tmp) / "repo", Path(tmp) / "out"
            self.consumer_source(repo)
            consumer = packaging.prepare_consumer(repo, out)
            self.assertTrue((consumer / "Package.swift").is_file())
            self.assertTrue((consumer / "Sources" / "GramDriveVerify" / "main.swift").is_file())

    def test_does_not_carry_stale_swiftpm_state_into_the_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, out = Path(tmp) / "repo", Path(tmp) / "out"
            source = self.consumer_source(repo)
            index = source / ".build" / "index-build" / "db"
            index.mkdir(parents=True)
            (index / "data.mdb").write_bytes(b"stale index")
            (source / "Package.resolved").write_text("{}")

            consumer = packaging.prepare_consumer(repo, out)
            self.assertFalse((consumer / ".build").exists())
            self.assertFalse((consumer / "Package.resolved").exists())
            # The source is untouched: the pipeline reads .scripts/, never writes it.
            self.assertTrue((source / ".build" / "index-build" / "db" / "data.mdb").is_file())

    def test_a_rerun_replaces_the_previous_copy(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, out = Path(tmp) / "repo", Path(tmp) / "out"
            self.consumer_source(repo)
            consumer = packaging.prepare_consumer(repo, out)
            (consumer / "leftover.txt").write_text("from an older run")
            packaging.prepare_consumer(repo, out)
            self.assertFalse((consumer / "leftover.txt").exists())


class StageBuildInputsTest(unittest.TestCase):
    """What --check-reproducible builds at the second path."""

    def repo(self, tmp: Path) -> Path:
        repo = tmp / "repo"
        (repo / "crates" / "gramdrive-ffi" / "src").mkdir(parents=True)
        (repo / "crates" / "gramdrive-ffi" / "src" / "lib.rs").write_text("// core")
        for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
            (repo / name).write_text(f"# {name}")
        return repo

    def test_stages_every_build_input(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.repo(Path(tmp))
            dest = Path(tmp) / "staged"
            packaging.stage_build_inputs(repo, dest)
            self.assertTrue((dest / "crates" / "gramdrive-ffi" / "src" / "lib.rs").is_file())
            for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
                self.assertTrue((dest / name).is_file())

    def test_does_not_stage_a_target_dir(self):
        # Staging target/ would carry the very state the check exists to rule
        # out into the build it is comparing against.
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.repo(Path(tmp))
            (repo / "crates" / "target" / "release").mkdir(parents=True)
            (repo / "crates" / "target" / "release" / "stale.rlib").write_bytes(b"stale")
            dest = Path(tmp) / "staged"
            packaging.stage_build_inputs(repo, dest)
            self.assertFalse((dest / "crates" / "target").exists())

    def test_a_missing_build_input_fails_loudly(self):
        # Silently staging less than the build reads would compare something
        # other than what ships.
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.repo(Path(tmp))
            (repo / "Cargo.lock").unlink()
            with self.assertRaises(packaging.StepFailed) as caught:
                packaging.stage_build_inputs(repo, Path(tmp) / "staged")
            self.assertIn("Cargo.lock", str(caught.exception))


class CheckReproducibleTest(unittest.TestCase):
    """The check must vary the axis the claim is about.

    The previous version built twice at the same path: that tests determinism,
    which is real, but it structurally cannot observe path-independence, which
    is what the manifest asserts.
    """

    def repo(self, tmp: Path) -> Path:
        repo = tmp / "repo"
        (repo / "crates").mkdir(parents=True)
        (repo / "crates" / "lib.rs").write_text("// core")
        for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
            (repo / name).write_text(f"# {name}")
        return repo

    def run_check(self, tmp: Path, *, contents):
        """Drive the check with a fake cargo that writes `contents` per build."""
        repo = self.repo(tmp)
        out = tmp / "out"
        out.mkdir()
        builds: list[Path] = []
        written = iter(contents)

        def scripted(argv, cwd, env=None):
            if "rustc" in " ".join(str(a) for a in argv):
                target = Path(env["CARGO_TARGET_DIR"])
                release = target / "aarch64-apple-darwin" / "release"
                release.mkdir(parents=True, exist_ok=True)
                (release / "libgramdrive_ffi.a").write_bytes(next(written))
                builds.append(Path(cwd))
            return 0, ""

        code = packaging.check_reproducible(
            repo, out_dir=out, runner=scripted, echo=lambda _: None, environ={"HOME": "/h"}
        )
        return code, builds

    def test_builds_at_two_different_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            code, builds = self.run_check(Path(tmp), contents=[b"same", b"same"])
            self.assertEqual(code, packaging.EXIT_OK)
            self.assertEqual(len(builds), 2)
            self.assertNotEqual(builds[0], builds[1], "the check must vary the path")

    def test_identical_bytes_at_two_paths_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            code, _ = self.run_check(Path(tmp), contents=[b"same", b"same"])
            self.assertEqual(code, packaging.EXIT_OK)

    def test_a_path_dependent_build_fails_the_check(self):
        # The case the old same-path check could not see.
        with tempfile.TemporaryDirectory() as tmp:
            code, _ = self.run_check(Path(tmp), contents=[b"here", b"different there"])
            self.assertEqual(code, packaging.EXIT_FAILED)

    def test_each_build_gets_its_own_clean_target_dir(self):
        with tempfile.TemporaryDirectory() as tmp:
            targets: list[str] = []

            repo = self.repo(Path(tmp))
            out = Path(tmp) / "out"
            out.mkdir()

            def scripted(argv, cwd, env=None):
                if "rustc" in " ".join(str(a) for a in argv):
                    targets.append(env["CARGO_TARGET_DIR"])
                    release = Path(env["CARGO_TARGET_DIR"]) / "aarch64-apple-darwin" / "release"
                    release.mkdir(parents=True, exist_ok=True)
                    (release / "libgramdrive_ffi.a").write_bytes(b"archive")
                return 0, ""

            packaging.check_reproducible(
                repo, out_dir=out, runner=scripted, echo=lambda _: None, environ={"HOME": "/h"}
            )
            self.assertEqual(len(set(targets)), 2, "each path needs its own target dir")

    def test_a_build_that_writes_nothing_is_not_a_pass(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = self.repo(Path(tmp))
            out = Path(tmp) / "out"
            out.mkdir()
            code = packaging.check_reproducible(
                repo,
                out_dir=out,
                runner=lambda argv, cwd, env=None: (0, ""),
                echo=lambda _: None,
                environ={"HOME": "/h"},
            )
            self.assertEqual(code, packaging.EXIT_FAILED)


class PipelineTest(unittest.TestCase):
    """The pipeline's control flow, with every subprocess faked."""

    def stage(self, tmp: Path, runner: FakeRunner) -> Path:
        """Lay out a fake repo whose builds 'succeed' by writing files."""
        repo = tmp / "repo"
        consumer = repo / ".scripts" / "packaging" / "swift-consumer"
        consumer.mkdir(parents=True)
        (consumer / "Package.swift").write_text("// consumer")
        return repo

    def bindgen_side_effect(self, out_dir: Path) -> None:
        out_dir.mkdir(parents=True, exist_ok=True)
        (out_dir / "GramDriveCore.swift").write_text("// bindings")
        (out_dir / "GramDriveCoreFFI.h").write_text("// header")
        (out_dir / "GramDriveCoreFFI.modulemap").write_text("module GramDriveCoreFFI {}")

    def run_pipeline(self, tmp: Path, out_dir: Path | None = None, **kwargs):
        """Drive package() with fakes standing in for cargo/xcodebuild/swift.

        host_machine defaults to arm64 here so the suite's verdicts do not
        depend on which CI host happens to run it: the repo suite runs both on
        the arm64 reference host and on the x86_64 self-hosted runner, and a
        default of platform.machine() would silently flip the pipeline into
        cross-link mode on the latter.
        """
        out_dir = out_dir or (tmp / "out")
        out_dir.mkdir(parents=True, exist_ok=True)
        runner = FakeRunner()
        repo = self.stage(tmp, runner)

        report = kwargs.pop("report", '{"contract_version": "0.1.0"}')
        kwargs.setdefault("host_machine", "arm64")

        def scripted(argv, cwd, env=None):
            argv = tuple(str(a) for a in argv)
            runner.calls.append(argv)
            runner.envs.append(env)
            joined = " ".join(argv)
            if "rustc" in joined and "--crate-type" in joined:
                # A real cargo writes into CARGO_TARGET_DIR under the triple it
                # was asked for; a fake that wrote anywhere else would hide a
                # pipeline looking in the wrong place.
                triple = argv[argv.index("--target") + 1]
                release = Path(env["CARGO_TARGET_DIR"]) / triple / "release"
                release.mkdir(parents=True, exist_ok=True)
                (release / "libgramdrive_ffi.a").write_bytes(b"archive")
                return 0, ""
            if argv[0] == "lipo" and "-create" in joined:
                output = Path(argv[argv.index("-output") + 1])
                output.parent.mkdir(parents=True, exist_ok=True)
                output.write_bytes(b"universal-archive")
                return 0, ""
            if "uniffi-bindgen" in joined and "generate" in joined:
                self.bindgen_side_effect(Path(argv[argv.index("--out-dir") + 1]))
                return 0, ""
            if "-create-xcframework" in joined:
                output = Path(argv[argv.index("-output") + 1])
                (output / "macos-arm64" / "Headers").mkdir(parents=True)
                (output / "Info.plist").write_text("<plist/>")
                (output / "macos-arm64" / "libgramdrive_ffi.a").write_bytes(b"archive")
                return 0, ""
            if "GramDriveVerify" in joined:
                return 0, f"Build complete!\n{report}\n"
            if "cargo metadata" in joined:
                packages = [
                    {"name": "uniffi", "version": "0.32.0"},
                    {"name": "gramdrive-ffi", "version": "0.1.0"},
                ]
                return 0, json.dumps({"packages": packages})
            if joined.startswith("git describe"):
                return 0, "v0.1.0\n"
            if joined.startswith("git rev-parse"):
                return 0, "abc123\n"
            if joined.startswith("git status"):
                return 0, ""
            return 0, ""

        manifest = packaging.package(
            repo,
            out_dir=out_dir,
            runner=scripted,
            echo=lambda _: None,
            environ={"HOME": "/home/u", "PATH": "/bin"},
            **kwargs,
        )
        return manifest, runner, out_dir

    def test_produces_a_consumable_artifact_and_manifest(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, out_dir = self.run_pipeline(Path(tmp))
            artifact = out_dir / "GramDriveCore"
            self.assertTrue((artifact / "Package.swift").is_file())
            self.assertTrue((artifact / "README.md").is_file())
            self.assertTrue((artifact / "gramdrive-core-manifest.json").is_file())
            self.assertTrue((artifact / "Sources" / "GramDriveCore" / "GramDriveCore.swift").is_file())
            self.assertTrue((artifact / "GramDriveCore.xcframework" / "Info.plist").is_file())
            self.assertEqual(manifest["contract_version"], "0.1.0")
            self.assertTrue((out_dir / "CHECKSUMS.sha256").is_file())
            self.assertTrue((out_dir / "GramDriveCore-0.1.0.zip").is_file())

    def test_artifact_directory_name_matches_swiftpm_package_identity(self):
        # SwiftPM derives a path dependency's identity from the directory name;
        # the consumer's `.package(path: "../GramDriveCore")` breaks if this
        # drifts, and the break is a resolution error far from the cause.
        with tempfile.TemporaryDirectory() as tmp:
            _, _, out_dir = self.run_pipeline(Path(tmp))
            self.assertTrue((out_dir / "GramDriveCore").is_dir())

    def test_build_runs_with_reproducibility_flags(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, runner, _ = self.run_pipeline(Path(tmp))
            index = next(
                i for i, argv in enumerate(runner.calls) if "rustc" in " ".join(argv)
            )
            env = runner.envs[index]
            self.assertIsNotNone(env, "the cargo build must run with a prepared env")
            self.assertIn("--remap-path-prefix", env[packaging.ENCODED_RUSTFLAGS])

    def test_shipped_library_is_built_in_packagings_own_target_dir(self):
        # Not the repo's. Measured (LOGBOOK 0552): reusing a target directory
        # that earlier builds wrote to changes the shipped bytes, so the repo's
        # target/ -- which the debug loop owns -- cannot be where this builds.
        with tempfile.TemporaryDirectory() as tmp:
            _, runner, out_dir = self.run_pipeline(Path(tmp))
            index = next(
                i for i, argv in enumerate(runner.calls) if "rustc" in " ".join(argv)
            )
            self.assertEqual(runner.envs[index]["CARGO_TARGET_DIR"], str(out_dir / "target"))

    def test_the_shipped_build_starts_from_a_clean_target_dir(self):
        # The wipe is the property; a leftover from an earlier run must not
        # survive into the build whose bytes we then publish.
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp) / "out"
            stale = out_dir / "target" / "aarch64-apple-darwin" / "release"
            stale.mkdir(parents=True)
            (stale / "leftover.rlib").write_bytes(b"from an earlier run")
            self.run_pipeline(Path(tmp), out_dir=out_dir)
            self.assertFalse((stale / "leftover.rlib").exists())

    def test_manifest_reports_the_build_that_ran(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, _ = self.run_pipeline(Path(tmp))
            reproducible = manifest["reproducible"]
            self.assertTrue(reproducible["path_independent"])
            self.assertTrue(reproducible["clean_target_dir"])
            self.assertIn("/gramdrive", reproducible["path_prefixes_remapped_to"])

    def test_the_shipped_manifest_carries_no_local_build_path(self):
        # The artifact-wide version of the rule: whatever else the manifest
        # gains, it ships to consumers, and the build machine's paths are the
        # one thing this pipeline works to keep out of what ships.
        with tempfile.TemporaryDirectory() as tmp:
            _, _, out_dir = self.run_pipeline(Path(tmp))
            shipped = (out_dir / "GramDriveCore" / "gramdrive-core-manifest.json").read_text()
            self.assertNotIn(tmp, shipped)
            self.assertNotIn("/home/u", shipped)

    def test_bindgen_stays_out_of_the_packaging_target_dir(self):
        # It is a host tool built with --features bindgen; letting it into the
        # packaging target dir is exactly the pollution the wipe exists to stop.
        with tempfile.TemporaryDirectory() as tmp:
            _, runner, _ = self.run_pipeline(Path(tmp))
            index = next(
                i for i, argv in enumerate(runner.calls) if "uniffi-bindgen" in " ".join(argv)
            )
            self.assertIsNone(runner.envs[index])

    def test_checksums_cover_the_artifact_and_the_zip(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, out_dir = self.run_pipeline(Path(tmp))
            rendered = (out_dir / "CHECKSUMS.sha256").read_text()
            self.assertIn("Package.swift", rendered)
            self.assertIn("GramDriveCore.xcframework/Info.plist", rendered)
            self.assertIn("../GramDriveCore-0.1.0.zip", rendered)
            self.assertIn("gramdrive-core-manifest.json", manifest["checksums"])

    def test_sizes_are_measured_and_recorded(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, _ = self.run_pipeline(Path(tmp))
            self.assertGreater(manifest["sizes"]["artifact_bytes"], 0)
            self.assertGreater(manifest["sizes"]["xcframework_bytes"], 0)
            self.assertGreater(manifest["sizes"]["zip_bytes"], 0)

    def test_skip_verify_marks_the_version_unverified(self):
        # The escape hatch must not silently produce a release-looking manifest.
        with tempfile.TemporaryDirectory() as tmp:
            manifest, runner, _ = self.run_pipeline(Path(tmp), skip_verify=True)
            self.assertEqual(manifest["contract_version"], "unverified")
            self.assertFalse(any("GramDriveVerify" in " ".join(c) for c in runner.calls))

    def test_verifier_failure_fails_the_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(packaging.StepFailed):
                self.run_pipeline(Path(tmp), report="no json here")

    def test_missing_library_after_a_successful_build_is_caught(self):
        # cargo exiting 0 while writing nothing means the crate-type override
        # stopped doing what we think it does. Silence there would ship an
        # artifact built from a stale library.
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp) / "out"
            out_dir.mkdir(parents=True)
            repo = Path(tmp) / "repo"
            repo.mkdir()
            with self.assertRaises(packaging.StepFailed) as caught:
                packaging.package(
                    repo,
                    out_dir=out_dir,
                    runner=lambda argv, cwd, env=None: (0, ""),
                    echo=lambda _: None,
                    environ={"HOME": "/h"},
                )
            self.assertIn("produced no", str(caught.exception))

    def test_manifest_records_native_run_verify_mode(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, _ = self.run_pipeline(Path(tmp))
            self.assertEqual(manifest["verify_mode"], "native-run")
            self.assertIsNone(manifest["host_test_slice"])

    def test_cross_link_verify_on_a_host_that_cannot_run_the_slice(self):
        # The x86_64 runner staging the arm64-only artifact: the consumer must
        # be cross-BUILT for the shipped arch (a real resolve + link proof) and
        # must NOT be run, and the manifest must say what actually happened
        # rather than presenting a contract version nothing probed.
        with tempfile.TemporaryDirectory() as tmp:
            manifest, runner, out_dir = self.run_pipeline(Path(tmp), host_machine="x86_64")
            build = runner.argv_containing("swift build")
            self.assertIn("--arch", build)
            self.assertEqual(build[build.index("--arch") + 1], "arm64")
            self.assertFalse(any("GramDriveVerify" in " ".join(c) for c in runner.calls))
            self.assertEqual(manifest["verify_mode"], "cross-link-only")
            self.assertEqual(manifest["contract_version"], "unverified")
            self.assertIsNone(manifest["host_test_slice"])
            self.assertTrue((out_dir / "GramDriveCore-unverified.zip").is_file())

    def test_host_test_slice_builds_a_twin_and_runs_the_verifier(self):
        # --host-test-slice on the x86_64 runner: both triples build from the
        # same clean target dir, lipo folds them into one archive, xcodebuild
        # gets exactly one -library, and the verifier executes natively -- so
        # the contract version is real, and the staging is marked test-only.
        with tempfile.TemporaryDirectory() as tmp:
            manifest, runner, out_dir = self.run_pipeline(
                Path(tmp), host_machine="x86_64", host_test_slice=True
            )
            targets = [
                argv[argv.index("--target") + 1]
                for argv in runner.calls
                if "--crate-type" in " ".join(argv)
            ]
            self.assertEqual(
                sorted(targets), ["aarch64-apple-darwin", "x86_64-apple-darwin"]
            )
            lipo = runner.argv_containing("lipo -create")
            self.assertIn("-output", lipo)
            xcf = runner.argv_containing("-create-xcframework")
            self.assertEqual(sum(1 for a in xcf if a == "-library"), 1)
            self.assertIn("universal", xcf[xcf.index("-library") + 1])
            self.assertTrue(any("GramDriveVerify" in " ".join(c) for c in runner.calls))
            self.assertEqual(manifest["verify_mode"], "native-run")
            self.assertEqual(manifest["contract_version"], "0.1.0")
            self.assertEqual(
                manifest["host_test_slice"]["triple"], "x86_64-apple-darwin"
            )
            readme = (out_dir / "GramDriveCore" / "README.md").read_text()
            self.assertIn("CI test staging", readme)
            # The shipped slice list must not gain the twin: SLICES is the
            # product decision and this staging is not it.
            self.assertEqual(
                [entry["triple"] for entry in manifest["slices"]],
                ["aarch64-apple-darwin"],
            )

    def test_host_test_slice_is_a_noop_on_the_shipping_host(self):
        # An arm64 host already runs the shipped slice; asking for the twin
        # must not lipo or change the staged shape.
        with tempfile.TemporaryDirectory() as tmp:
            manifest, runner, _ = self.run_pipeline(
                Path(tmp), host_machine="arm64", host_test_slice=True
            )
            self.assertFalse(any(argv[0] == "lipo" for argv in runner.calls))
            self.assertIsNone(manifest["host_test_slice"])
            self.assertEqual(manifest["verify_mode"], "native-run")

    def test_renamed_bindgen_output_is_caught(self):
        # The build succeeds and bindgen exits 0 having written nothing under
        # the names uniffi.toml pins -- so this reaches the bindgen check rather
        # than tripping the build check above.
        def builds_but_generates_nothing(argv, cwd, env=None):
            if "rustc" in " ".join(str(a) for a in argv):
                release = Path(env["CARGO_TARGET_DIR"]) / "aarch64-apple-darwin" / "release"
                release.mkdir(parents=True, exist_ok=True)
                (release / "libgramdrive_ffi.a").write_bytes(b"archive")
            return 0, ""

        with tempfile.TemporaryDirectory() as tmp:
            out_dir = Path(tmp) / "out"
            out_dir.mkdir(parents=True)
            repo = self.stage(Path(tmp), FakeRunner())
            with self.assertRaises(packaging.StepFailed) as caught:
                packaging.package(
                    repo,
                    out_dir=out_dir,
                    runner=builds_but_generates_nothing,
                    echo=lambda _: None,
                    environ={"HOME": "/h"},
                )
            self.assertIn("uniffi-bindgen did not produce", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
