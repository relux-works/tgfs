#!/usr/bin/env python3
"""Tests for .scripts/apple-app/build_app_bundle.py.

Run: python3 -m unittest discover -s .scripts/tests -t .scripts/tests
     (or via the gate: run_automated.py --suite repo --run-id local-repo)

No test shells out to swift, codesign, spctl, hdiutil or notarytool: a fake
runner stands in for every subprocess, so the suite is fast and runs on a
machine without Xcode, a signing identity, or network. What that buys is
coverage of the properties a reader of the artifact cannot check for themselves
and the real pipeline cannot be asked to stage on demand:

  * the entitlements are exactly what ships — app-groups on all three, App
    Sandbox only on the extension, and NO get-task-allow (SwiftPM's debug
    default, which would fail notarization);
  * the bundle layout matches what SMAppService and fileproviderd resolve;
  * signing is inside-out (nested code before the app that seals it);
  * the manifest carries no key material and gates notarization honestly.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import plistlib
import sys
import tempfile
import unittest
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
SCRIPT_PATH = REPO_ROOT / ".scripts" / "apple-app" / "build_app_bundle.py"


def load_module():
    """Import build_app_bundle.py by path (`.scripts` is not a package)."""
    spec = importlib.util.spec_from_file_location("build_app_bundle", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


app = load_module()


def read_plist(path: Path) -> dict:
    with path.open("rb") as handle:
        return plistlib.load(handle)


def stage_openssl_attribution(core: Path, manifest: dict) -> None:
    license_path = core / app.OPENSSL_LICENSE_PATH
    license_path.parent.mkdir(parents=True, exist_ok=True)
    license_path.write_text("Apache License 2.0\n")
    manifest["third_party"] = {
        "openssl": {
            "name": "OpenSSL",
            "version": "3.6.3",
            "source": {
                "url": "https://example.invalid/openssl-3.6.3.tar.gz",
                "sha256": app.OPENSSL_SOURCE_SHA256,
            },
            "build_options": ["no-shared", "no-module"],
            "linkage": "static",
            "embedded_in": "lib/libtdjson.dylib",
            "license": {
                "id": "Apache-2.0",
                "file": app.OPENSSL_LICENSE_PATH.as_posix(),
                "sha256": app.sha256_file(license_path),
            },
        }
    }
    manifest.setdefault("tdjson", {})["runtime"] = {
        "dependency_policy": "system-only-static-openssl",
        "openssl_linkage": "static",
        "dependencies": ["/usr/lib/libSystem.B.dylib"],
        "forbidden_builder_paths_verified": True,
        "trust_store": {
            "policy": "macos-system-pem",
            "cert_file": app.OPENSSL_CERT_FILE,
            "environment_overrides_scrubbed": True,
            "certificate_objects": 158,
            "verified": True,
        },
    }


class EntitlementsTest(unittest.TestCase):
    """The load-bearing signing decision: what each binary is entitled to."""

    def test_app_has_group_only_no_sandbox_no_get_task_allow(self):
        ent = app.app_entitlements()
        self.assertEqual(
            ent["com.apple.security.application-groups"], ["262RZ595FP.com.reluxworks.gramdrive"]
        )
        self.assertNotIn("com.apple.security.app-sandbox", ent)
        self.assertNotIn("com.apple.security.get-task-allow", ent)

    def test_agent_matches_the_app_container_unsandboxed(self):
        ent = app.agent_entitlements()
        self.assertEqual(
            ent["com.apple.security.application-groups"], ["262RZ595FP.com.reluxworks.gramdrive"]
        )
        self.assertNotIn("com.apple.security.app-sandbox", ent)
        self.assertNotIn("com.apple.security.get-task-allow", ent)

    def test_fileprovider_is_sandboxed_with_the_shared_group(self):
        # Extensions run in the App Sandbox; the App Group is how it reaches
        # durable state and the agent's hydration socket.
        ent = app.fileprovider_entitlements()
        self.assertIs(ent["com.apple.security.app-sandbox"], True)
        self.assertEqual(
            ent["com.apple.security.application-groups"], ["262RZ595FP.com.reluxworks.gramdrive"]
        )
        self.assertNotIn("com.apple.security.get-task-allow", ent)

    def test_the_team_prefixed_group_is_the_form_v1_ships(self):
        # Not group.com.reluxworks.gramdrive: that needs a provisioning profile
        # and does not work under Developer ID (platform-requirements.md).
        self.assertEqual(app.APP_GROUP, "262RZ595FP.com.reluxworks.gramdrive")


class InfoPlistTest(unittest.TestCase):
    def test_app_info_plist_identity_and_floor(self):
        plist = app.app_info_plist("1.2.3", "42", app.update_configuration("test"))
        self.assertEqual(plist["CFBundleIdentifier"], "com.reluxworks.gramdrive")
        self.assertEqual(plist["CFBundleExecutable"], "GramDrive")
        self.assertEqual(plist["CFBundleShortVersionString"], "1.2.3")
        self.assertEqual(plist["CFBundleVersion"], "42")
        self.assertEqual(plist["LSMinimumSystemVersion"], "14.0")
        self.assertIs(plist["LSUIElement"], True)
        self.assertEqual(
            plist["SUFeedURL"],
            "https://github.com/relux-works/tgfs/releases/download/updates-test-v1/test.xml",
        )
        self.assertEqual(plist["SUPublicEDKey"], "T8IBLvve21ObUHz78CLXdF0eWN7QgJPHd1eKlcFhqmo=")
        self.assertIs(plist["SUVerifyUpdateBeforeExtraction"], True)
        self.assertIs(plist["SURequireSignedFeed"], True)
        self.assertEqual(plist["SUSignedFeedFailureExpirationInterval"], 0)

    def test_update_channels_have_different_immutable_trust_surfaces(self):
        test = app.update_configuration("test")
        stable = app.update_configuration("stable")
        self.assertNotEqual(test["feed_url"], stable["feed_url"])
        self.assertNotEqual(test["public_key"], stable["public_key"])
        self.assertEqual(
            stable["feed_url"], "https://relux-works.github.io/tgfs/updates/stable/v1/stable.xml")
        self.assertEqual(stable["public_key"], "FWkWDnXjzJFkgtipafAAtUJ42qcIuGBZ14Qvd0WpuDE=")

    def test_missing_or_invalid_reviewed_public_key_refuses_packaging(self):
        with self.assertRaises(app.StepFailed):
            app.update_configuration("test", {"test": {"feed_url": "https://example.test/feed.xml"}})
        with self.assertRaises(app.StepFailed):
            app.update_configuration(
                "test",
                {"test": {"feed_url": "https://example.test/feed.xml", "public_key": "not-base64"}},
            )

    def test_appex_declares_a_file_provider_extension(self):
        plist = app.appex_info_plist("1.2.3", "42")
        self.assertEqual(plist["CFBundleIdentifier"], "com.reluxworks.gramdrive.fileprovider")
        ext = plist["NSExtension"]
        self.assertEqual(ext["NSExtensionPointIdentifier"], "com.apple.fileprovider-nonui")
        # Must equal the Swift-mangled runtime name emitted into the binary.
        self.assertEqual(
            ext["NSExtensionPrincipalClass"],
            "GramDriveFileProvider.GramDriveFileProviderExtension",
        )
        self.assertEqual(
            ext["NSExtensionFileProviderDocumentGroup"], "262RZ595FP.com.reluxworks.gramdrive"
        )
        self.assertIs(ext["NSExtensionFileProviderSupportsEnumeration"], True)

    def test_agent_launchd_plist_points_at_the_embedded_binary(self):
        plist = app.agent_launchd_plist()
        self.assertEqual(plist["Label"], "com.reluxworks.gramdrive.agent")
        self.assertEqual(plist["BundleProgram"], "Contents/MacOS/gramdrive-agent")
        self.assertEqual(plist["AssociatedBundleIdentifiers"], ["com.reluxworks.gramdrive"])
        self.assertIs(plist["RunAtLoad"], True)
        self.assertEqual(
            plist["KeepAlive"],
            {"SuccessfulExit": False},
            "a planned updater _exit must not relaunch the old build",
        )
        # The label must equal the plist basename SMAppService resolves.
        self.assertEqual(plist["Label"], app.AGENT_LAUNCHD_LABEL)
        agent = next(binary for binary in app.BINARIES if binary.key == "agent")
        self.assertEqual(
            plist["BundleProgram"],
            agent.install_path,
            "SMAppService and direct-spawn discovery must resolve the same packaged binary",
        )


class FileProviderEntryPointTest(unittest.TestCase):
    """Pin the boundary that previously recursed before any callback."""

    def test_swiftpm_links_nsextensionmain_as_the_macho_entry(self):
        manifest = (REPO_ROOT / "apple" / "GramDriveSupport" / "Package.swift").read_text()
        self.assertIn('"-Xlinker", "-e", "-Xlinker", "_NSExtensionMain"', manifest)

    def test_swift_main_never_calls_nsextensionmain(self):
        source = (
            REPO_ROOT
            / "apple"
            / "GramDriveSupport"
            / "Sources"
            / "GramDriveFileProviderExtensionApp"
            / "main.swift"
        ).read_text()
        self.assertNotIn("gramDriveNSExtensionMain()", source)
        self.assertNotIn("exit(", source)
        self.assertIn("GramDriveFileProviderExtension.self", source)


class SigningOrderTest(unittest.TestCase):
    def test_nested_code_is_signed_before_the_app(self):
        # codesign refuses to seal a bundle whose nested code is unsigned, so the
        # app (is_app_bundle) must be last and the appex/agent before it.
        keys = [b.key for b in app.BINARIES]
        self.assertLess(keys.index("fileprovider"), keys.index("app"))
        self.assertLess(keys.index("agent"), keys.index("app"))
        self.assertTrue(app.BINARIES[-1].is_app_bundle)

    def test_every_binary_carries_a_gramdrive_bundle_id(self):
        for spec in app.BINARIES:
            self.assertTrue(spec.bundle_id.startswith("com.reluxworks.gramdrive"))


class CodesignArgvTest(unittest.TestCase):
    def test_hardened_runtime_timestamp_and_entitlements(self):
        argv = app.codesign_argv(
            Path("/x/GramDrive.app"),
            identity="Developer ID Application: X",
            entitlements=Path("/e/app.entitlements"),
            timestamp=True,
        )
        self.assertIn("--force", argv)
        self.assertEqual(argv[argv.index("--options") + 1], "runtime")
        self.assertIn("--timestamp", argv)
        self.assertEqual(argv[argv.index("--entitlements") + 1], "/e/app.entitlements")
        self.assertEqual(argv[argv.index("--sign") + 1], "Developer ID Application: X")
        self.assertEqual(argv[-1], "/x/GramDrive.app")

    def test_identifier_pins_the_bundle_id_for_a_loose_helper(self):
        # The agent is a loose Mach-O with no Info.plist, so codesign would
        # default its identifier to the file name without this.
        argv = app.codesign_argv(
            Path("/x/Contents/MacOS/gramdrive-agent"),
            identity="X",
            entitlements=Path("/e/agent.entitlements"),
            timestamp=True,
            identifier="com.reluxworks.gramdrive.agent",
        )
        self.assertEqual(argv[argv.index("--identifier") + 1], "com.reluxworks.gramdrive.agent")

    def test_dmg_signs_without_entitlements(self):
        argv = app.codesign_argv(
            Path("/x/G.dmg"), identity="X", entitlements=None, timestamp=True
        )
        self.assertNotIn("--entitlements", argv)

    def test_timestamp_can_be_disabled_for_a_dry_pass(self):
        argv = app.codesign_argv(Path("/x"), identity="X", entitlements=None, timestamp=False)
        self.assertIn("--timestamp=none", argv)
        self.assertNotIn("--timestamp", argv)

    def test_verify_is_deep_and_strict(self):
        argv = app.verify_argv(Path("/x/GramDrive.app"))
        self.assertIn("--deep", argv)
        self.assertIn("--strict", argv)


class ParseTest(unittest.TestCase):
    def test_parses_entitlements_among_a_header(self):
        payload = plistlib.dumps({"com.apple.security.app-sandbox": True}).decode()
        out = f"/x/GramDrive.app:\n[Dict]\n{payload}"
        self.assertEqual(app.parse_entitlements(out), {"com.apple.security.app-sandbox": True})

    def test_no_entitlements_is_an_empty_dict_not_an_error(self):
        self.assertEqual(app.parse_entitlements("/x: code object is not signed\n"), {})

    def test_malformed_entitlements_is_an_empty_dict(self):
        self.assertEqual(app.parse_entitlements("<?xml version='1.0'?><plist>oops</plist>"), {})

    def test_parses_cdhash(self):
        out = "Executable=/x\nIdentifier=com.reluxworks.gramdrive\nCDHash=abc123def\n"
        self.assertEqual(app.parse_cdhash(out), "abc123def")

    def test_no_cdhash_is_none(self):
        self.assertIsNone(app.parse_cdhash("Executable=/x\n"))

    def test_parses_notary_submission_last_json_object(self):
        out = '{"id": "old", "status": "In Progress"}\n{"id": "new", "status": "Accepted"}'
        record = app.parse_notary_submission(out)
        self.assertEqual(record["id"], "new")
        self.assertEqual(record["status"], "Accepted")

    def test_notary_submission_missing_is_empty(self):
        self.assertEqual(app.parse_notary_submission("no json here"), {})


class MarketingVersionTest(unittest.TestCase):
    def test_reads_the_reviewed_three_component_source(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / app.SUPPORT_PACKAGE
            source.mkdir(parents=True)
            (source / "Version.json").write_text(json.dumps({"marketing_version": "1.2.3"}))
            self.assertEqual(app.marketing_version(root), "1.2.3")

    def test_invalid_source_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / app.SUPPORT_PACKAGE
            source.mkdir(parents=True)
            (source / "Version.json").write_text(json.dumps({"marketing_version": "1.2"}))
            with self.assertRaises(app.StepFailed):
                app.marketing_version(root)


class AssertEntitlementsTest(unittest.TestCase):
    def test_match_passes(self):
        app.assert_entitlements("app", {"a": [1]}, {"a": [1], "extra": True})

    def test_missing_expected_key_fails(self):
        with self.assertRaises(app.StepFailed):
            app.assert_entitlements("app", {"a": [1]}, {})

    def test_wrong_value_fails(self):
        with self.assertRaises(app.StepFailed):
            app.assert_entitlements("app", {"a": [1]}, {"a": [2]})

    def test_get_task_allow_leak_fails(self):
        # The exact notarization-killer this check exists for.
        with self.assertRaises(app.StepFailed) as caught:
            app.assert_entitlements(
                "app", {}, {"com.apple.security.get-task-allow": True}
            )
        self.assertIn("get-task-allow", str(caught.exception))


class ResolveIdentityTest(unittest.TestCase):
    def test_flag_wins_then_env_then_default(self):
        self.assertEqual(app.resolve_identity("Flag", {app.IDENTITY_ENV: "Env"}), "Flag")
        self.assertEqual(app.resolve_identity(None, {app.IDENTITY_ENV: "Env"}), "Env")
        self.assertEqual(app.resolve_identity(None, {}), app.DEFAULT_IDENTITY)


class PipelineTest(unittest.TestCase):
    """The whole pipeline's control flow, with every subprocess faked."""

    def scripted(
        self,
        extra=None,
        leak_get_task_allow=False,
        nested_arch="arm64",
        nested_team=app.TEAM_ID,
    ):
        """A runner that fakes swift/codesign/spctl/hdiutil/git/notarytool.

        Build side effects are real files (the pipeline copies them), so a fake
        that wrote nothing would hide a pipeline looking in the wrong place.
        `leak_get_task_allow` simulates SwiftPM's debug default surviving into
        the signature: the dump then carries the correct entitlements *plus*
        get-task-allow, so the run reaches — and must trip — that guard rather
        than an earlier missing-entitlement one.
        """
        extra = extra or {}
        calls: list[tuple[str, ...]] = []

        def entitlements_xml_for(target: str) -> str:
            if "PlugIns" in target:
                data = app.fileprovider_entitlements()
            elif "gramdrive-agent" in target:
                data = app.agent_entitlements()
            else:
                data = app.app_entitlements()
            if leak_get_task_allow:
                data = {**data, "com.apple.security.get-task-allow": True}
            # Prepend a header line the way real codesign does.
            return f"{target}:\n" + plistlib.dumps(data).decode()

        def run(argv, cwd, env=None):
            argv = tuple(str(a) for a in argv)
            calls.append(argv)
            joined = " ".join(argv)
            for needle, result in extra.items():
                if needle in joined:
                    return result
            if joined.startswith("swift build") and "--show-bin-path" in joined:
                bin_dir = Path(cwd) / ".build" / "release"
                bin_dir.mkdir(parents=True, exist_ok=True)
                for product in ("gramdrive-companion", "gramdrive-agent", "gramdrive-fileprovider"):
                    binary = bin_dir / product
                    binary.write_bytes(b"\xcf\xfa\xed\xfe macho")
                    binary.chmod(0o755)
                sparkle = bin_dir / "Sparkle.framework" / "Versions" / "A"
                (sparkle / "XPCServices" / "Downloader.xpc").mkdir(parents=True)
                (sparkle / "XPCServices" / "Installer.xpc").mkdir(parents=True)
                (sparkle / "Sparkle").write_bytes(b"\xcf\xfa\xed\xfe framework")
                (sparkle / "Autoupdate").write_bytes(b"\xcf\xfa\xed\xfe helper")
                (sparkle / "Updater.app").mkdir()
                nested_executables = (
                    sparkle / "Updater.app" / "Contents" / "MacOS" / "Updater",
                    sparkle / "XPCServices" / "Downloader.xpc" / "Contents" / "MacOS" / "Downloader",
                    sparkle / "XPCServices" / "Installer.xpc" / "Contents" / "MacOS" / "Installer",
                )
                for binary in nested_executables:
                    binary.parent.mkdir(parents=True)
                    binary.write_bytes(b"\xcf\xfa\xed\xfe nested")
                (sparkle.parent / "Current").symlink_to("A")
                return 0, str(bin_dir) + "\n"
            if joined.startswith("swift build"):
                return 0, ""
            if joined.startswith("lipo -archs"):
                if "Sparkle.framework" in joined:
                    return 0, nested_arch + "\n"
                return 0, "arm64\n"
            if "-d --entitlements" in joined:
                return 0, entitlements_xml_for(argv[-1])
            if "-d --verbose=4" in joined:
                team = nested_team if "Sparkle.framework" in joined else app.TEAM_ID
                return 0, (
                    f"Executable={argv[-1]}\n"
                    "CDHash=deadbeef\n"
                    f"Authority=Developer ID Application: Test ({app.TEAM_ID})\n"
                    + (f"TeamIdentifier={team}\n" if team is not None else "")
                )
            if joined.startswith("codesign"):
                return 0, ""
            if joined.startswith("spctl"):
                return 0, "accepted\n"
            if joined.startswith("ditto"):
                # Zipping the .app into its notarization container.
                Path(argv[-1]).write_bytes(b"app-zip-bytes")
                return 0, ""
            if joined.startswith("hdiutil"):
                dmg = Path(argv[argv.index("create") + 0])  # placeholder
                out = Path(argv[-1])
                out.write_bytes(b"dmg-bytes")
                return 0, ""
            if joined.startswith("xcrun notarytool"):
                return 0, '{"id": "sub-123", "status": "Accepted"}'
            if joined.startswith("xcrun stapler"):
                return 0, "The staple and validate action worked!"
            if joined.startswith("git describe"):
                return 0, "v0.1.0-2-gabc\n"
            if joined.startswith("git rev-parse"):
                return 0, "abc123\n"
            if joined.startswith("git rev-list"):
                return 0, "137\n"
            if joined.startswith("git status"):
                return 0, ""
            if joined.startswith("swift --version"):
                return 0, "Apple Swift version 6.3\n"
            if joined.startswith("xcodebuild -version"):
                return 0, "Xcode 26.5\n"
            if joined.startswith("rustc --version"):
                return 0, "rustc 1.91.0\n"
            return 0, ""

        return run, calls

    def stage(self, tmp: Path, *, linked: bool = True) -> tuple[Path, Path]:
        """A repo with the SwiftPM package and a staged core package present."""
        repo = tmp / "repo"
        (repo / app.SUPPORT_PACKAGE).mkdir(parents=True)
        (repo / app.SUPPORT_PACKAGE / "Version.json").write_text(
            json.dumps({"marketing_version": "0.5.0"}))
        core = repo / app.DEFAULT_CORE_PACKAGE
        core.mkdir(parents=True)
        (core / "Package.swift").write_text("// core")
        manifest = {"contract_version": "0.5.0"}
        if linked:
            (core / "lib").mkdir()
            (core / "lib" / "libtdjson.dylib").write_bytes(b"\xcf\xfa\xed\xfe dylib")
            manifest["tdjson"] = {
                "linked": True,
                "library_sha256": app.sha256_file(
                    core / "lib" / "libtdjson.dylib"
                ),
            }
            stage_openssl_attribution(core, manifest)
        (core / "gramdrive-core-manifest.json").write_text(json.dumps(manifest))
        return repo, core

    def run_pipeline(
        self,
        tmp: Path,
        *,
        extra=None,
        leak_get_task_allow=False,
        nested_arch="arm64",
        nested_team=app.TEAM_ID,
        linked=True,
        **kwargs,
    ):
        repo, core = self.stage(tmp, linked=linked)
        out = tmp / "out"
        out.mkdir()
        run, calls = self.scripted(
            extra,
            leak_get_task_allow=leak_get_task_allow,
            nested_arch=nested_arch,
            nested_team=nested_team,
        )
        manifest = app.package(
            repo,
            out_dir=out,
            identity="Developer ID Application: Test (262RZ595FP)",
            core_package=core,
            runner=run,
            echo=lambda _: None,
            environ={"HOME": "/h", "PATH": "/bin"},
            **kwargs,
        )
        return manifest, calls, out

    def test_swift_builds_target_the_shipped_arch(self):
        # POL-5/DEC-017 via TASK-260719-1dwaj8: the x86_64 CI host must
        # cross-build the same arm64 executables an arm64 host builds natively,
        # so the arch is explicit on every product build and on the bin-path
        # query that locates their output.
        with tempfile.TemporaryDirectory() as tmp:
            _, calls, _ = self.run_pipeline(Path(tmp))
            builds = [c for c in calls if c[:2] == ("swift", "build")]
            self.assertTrue(builds)
            for argv in builds:
                self.assertIn("--arch", argv)
                self.assertEqual(argv[argv.index("--arch") + 1], "arm64")

    def test_a_wrong_arch_binary_fails_the_build(self):
        # A host that quietly fell back to its own arch stages binaries the
        # shipped platform cannot run; `lipo -archs` reads the built bytes and
        # anything but exactly arm64 must fail loudly.
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(app.StepFailed) as caught:
                self.run_pipeline(Path(tmp), extra={"lipo -archs": (0, "x86_64\n")})
            self.assertIn("arm64 only", str(caught.exception))

    def test_manifest_records_the_enforced_binary_arch(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, _ = self.run_pipeline(Path(tmp))
            self.assertEqual(manifest["binary_arch"]["required"], "arm64")
            self.assertIn("lipo -archs", manifest["binary_arch"]["verified_by"])

    def test_signed_package_refuses_a_core_without_tdjson_by_default(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(app.StepFailed) as caught:
                self.run_pipeline(Path(tmp), linked=False)
            self.assertIn("signed app packaging requires a tdjson-linked", str(caught.exception))

    def test_unsigned_diagnostic_assembly_allows_an_unlinked_core(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, _ = self.run_pipeline(Path(tmp), linked=False, unsigned=True)
            self.assertFalse(manifest["tdjson"]["linked"])

    def test_make_signed_and_notarized_recipes_use_the_default_linkage_guard(self):
        makefile = (REPO_ROOT / "Makefile").read_text()
        self.assertIn(
            "package-app:\n\tpython3 .scripts/apple-app/build_app_bundle.py\n",
            makefile,
        )
        self.assertIn(
            "package-app-notarize:\n"
            "\tpython3 .scripts/apple-app/build_app_bundle.py --notarize\n",
            makefile,
        )
        self.assertNotIn("--require-tdjson", makefile)

    def test_assembles_the_expected_bundle_layout(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, _, out = self.run_pipeline(Path(tmp))
            appdir = out / "GramDrive.app"
            self.assertTrue((appdir / "Contents" / "MacOS" / "GramDrive").is_file())
            agent = appdir / "Contents" / "MacOS" / "gramdrive-agent"
            self.assertTrue(agent.is_file())
            self.assertTrue(agent.stat().st_mode & 0o111, "the bundled agent must stay executable")
            self.assertTrue(
                (
                    appdir
                    / "Contents"
                    / "PlugIns"
                    / "GramDriveFileProvider.appex"
                    / "Contents"
                    / "MacOS"
                    / "GramDriveFileProvider"
                ).is_file()
            )

    def test_embeds_sparkle_helpers_and_signs_them_before_the_app(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, calls, out = self.run_pipeline(Path(tmp))
            appdir = out / "GramDrive.app"
            framework = appdir / "Contents" / "Frameworks" / "Sparkle.framework"
            self.assertTrue((framework / "Versions" / "Current").is_symlink())
            self.assertTrue((framework / "Versions" / "Current" / "Autoupdate").is_file())
            self.assertTrue((framework / "Versions" / "Current" / "Updater.app").is_dir())
            self.assertTrue(
                (framework / "Versions" / "Current" / "XPCServices" / "Downloader.xpc").is_dir())
            self.assertTrue(
                (framework / "Versions" / "Current" / "XPCServices" / "Installer.xpc").is_dir())
            order = [" ".join(call) for call in calls]
            sparkle_helpers = [
                framework / "Versions" / "Current" / "XPCServices" / "Installer.xpc",
                framework / "Versions" / "Current" / "XPCServices" / "Downloader.xpc",
                framework / "Versions" / "Current" / "Autoupdate",
                framework / "Versions" / "Current" / "Updater.app",
            ]
            helper_indices = [
                next(i for i, call in enumerate(order) if call.endswith(str(helper)))
                for helper in sparkle_helpers
            ]
            sparkle_index = next(
                i for i, call in enumerate(order) if call.endswith("Sparkle.framework"))
            app_index = next(
                i for i, call in enumerate(order)
                if call.endswith("GramDrive.app"))
            self.assertEqual(helper_indices, sorted(helper_indices))
            self.assertLess(helper_indices[-1], sparkle_index)
            self.assertLess(sparkle_index, app_index)
            for helper, helper_index in zip(sparkle_helpers, helper_indices):
                self.assertIn("--preserve-metadata=entitlements,requirements,flags", order[helper_index])
                self.assertTrue(order[helper_index].startswith("codesign --force "))
            self.assertTrue((appdir / "Contents" / "Info.plist").is_file())
            self.assertTrue((appdir / "Contents" / "PkgInfo").is_file())
            self.assertTrue(
                (
                    appdir
                    / "Contents"
                    / "Library"
                    / "LaunchAgents"
                    / "com.reluxworks.gramdrive.agent.plist"
                ).is_file()
            )

    def test_adds_the_shared_frameworks_rpath_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, calls, _ = self.run_pipeline(Path(tmp))
            rpath_calls = [
                call for call in calls
                if call[:2] == ("install_name_tool", "-add_rpath")
                and "@executable_path/../Frameworks" in call
            ]
            self.assertEqual(len(rpath_calls), 1)

    def test_the_bundle_plists_are_what_the_system_reads(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, _, out = self.run_pipeline(Path(tmp))
            appdir = out / "GramDrive.app"
            info = read_plist(appdir / "Contents" / "Info.plist")
            self.assertEqual(info["CFBundleIdentifier"], "com.reluxworks.gramdrive")
            appex = read_plist(
                appdir / "Contents" / "PlugIns" / "GramDriveFileProvider.appex" / "Contents" / "Info.plist"
            )
            self.assertEqual(
                appex["NSExtension"]["NSExtensionPointIdentifier"], "com.apple.fileprovider-nonui"
            )

    def test_packaged_variants_embed_only_their_exact_reviewed_trust_anchor(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            _, _, test_out = self.run_pipeline(root / "test")
            _, _, stable_out = self.run_pipeline(root / "stable", update_channel="stable")
            test_info = read_plist(test_out / "GramDrive.app" / "Contents" / "Info.plist")
            stable_info = read_plist(stable_out / "GramDrive.app" / "Contents" / "Info.plist")
            self.assertEqual(
                (test_info["SUFeedURL"], test_info["SUPublicEDKey"]),
                (
                    "https://github.com/relux-works/tgfs/releases/download/updates-test-v1/test.xml",
                    "T8IBLvve21ObUHz78CLXdF0eWN7QgJPHd1eKlcFhqmo=",
                ),
            )
            self.assertEqual(
                (stable_info["SUFeedURL"], stable_info["SUPublicEDKey"]),
                (
                    "https://relux-works.github.io/tgfs/updates/stable/v1/stable.xml",
                    "FWkWDnXjzJFkgtipafAAtUJ42qcIuGBZ14Qvd0WpuDE=",
                ),
            )
            self.assertNotEqual(
                (test_info["SUFeedURL"], test_info["SUPublicEDKey"]),
                (stable_info["SUFeedURL"], stable_info["SUPublicEDKey"]),
            )

    def test_signs_inside_out(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, calls, _ = self.run_pipeline(Path(tmp))
            order = [
                c[-1]
                for c in calls
                if c[0] == "codesign" and "--force" in c and "-d" not in c
            ]
            appex_idx = next(i for i, t in enumerate(order) if "PlugIns" in t)
            agent_idx = next(i for i, t in enumerate(order) if t.endswith("gramdrive-agent"))
            app_idx = next(i for i, t in enumerate(order) if t.endswith("GramDrive.app"))
            self.assertLess(appex_idx, app_idx)
            self.assertLess(agent_idx, app_idx)

    def test_manifest_records_identity_but_no_key_material(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, _ = self.run_pipeline(Path(tmp))
            self.assertEqual(manifest["team_id"], "262RZ595FP")
            self.assertIn("Developer ID Application", manifest["signing_identity"])
            blob = json.dumps(manifest)
            # None of the secret-ish words a leak would carry.
            for forbidden in ("PRIVATE KEY", "BEGIN CERTIFICATE", "p12", "password", "AuthKey"):
                self.assertNotIn(forbidden, blob)

    def test_signed_app_ships_checksummed_static_openssl_attribution(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, out = self.run_pipeline(Path(tmp))
            openssl = manifest["third_party"]["openssl"]
            relative = app.APP_OPENSSL_LICENSE_PATH.as_posix()
            license_path = out / app.APP_BUNDLE_NAME / app.APP_OPENSSL_LICENSE_PATH
            self.assertEqual(openssl["version"], "3.6.3")
            self.assertEqual(openssl["license"]["id"], "Apache-2.0")
            self.assertEqual(openssl["license"]["file"], relative)
            self.assertEqual(openssl["license"]["sha256"], app.sha256_file(license_path))
            self.assertEqual(
                manifest["checksums"][f"{app.APP_BUNDLE_NAME}/{relative}"],
                openssl["license"]["sha256"],
            )

    def test_manifest_records_per_binary_entitlements_and_cdhash(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, _ = self.run_pipeline(Path(tmp))
            roles = {b["role"]: b for b in manifest["binaries"]}
            self.assertEqual(
                roles["fileprovider"]["bundle_id"], "com.reluxworks.gramdrive.fileprovider"
            )
            self.assertIs(
                roles["fileprovider"]["entitlements"]["com.apple.security.app-sandbox"], True
            )
            self.assertEqual(roles["app"]["cdhash"], "deadbeef")

    def test_default_run_does_not_notarize(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, calls, _ = self.run_pipeline(Path(tmp))
            self.assertFalse(manifest["notarization"]["submitted"])
            self.assertFalse(any("notarytool" in " ".join(c) for c in calls))

    def test_unsigned_assembles_the_bundle_but_signs_nothing(self):
        # The assembly gate: build + lay out GramDrive.app and its plists, then
        # stop. No Developer ID, no codesign/spctl/hdiutil/notarytool, no dmg —
        # the check an ordinary CI runner can make.
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            repo, core = self.stage(tmp)
            out = tmp / "out"
            out.mkdir()
            run, calls = self.scripted()
            manifest = app.package(
                repo,
                out_dir=out,
                identity="unsigned",
                core_package=core,
                unsigned=True,
                runner=run,
                echo=lambda _: None,
                environ={"HOME": "/h", "PATH": "/bin"},
            )
            joined = [" ".join(c) for c in calls]

            # It really built and assembled the nested extension bundle.
            appex = out / "GramDrive.app" / "Contents" / "PlugIns" / "GramDriveFileProvider.appex"
            self.assertTrue((appex / "Contents" / "MacOS" / "GramDriveFileProvider").is_file())
            self.assertEqual(
                read_plist(appex / "Contents" / "Info.plist")["NSExtension"][
                    "NSExtensionPointIdentifier"
                ],
                "com.apple.fileprovider-nonui",
            )

            # ...but ran no signing, assessment, dmg, or notarization tool.
            self.assertFalse(any(j.startswith("codesign") for j in joined))
            self.assertFalse(any(j.startswith("spctl") for j in joined))
            self.assertFalse(any(j.startswith("hdiutil") for j in joined))
            self.assertFalse(any("notarytool" in j for j in joined))

            # The manifest is honest about being an unsigned assembly.
            self.assertFalse(manifest["signed"])
            self.assertEqual(manifest["signing_identity"], "unsigned")
            self.assertFalse(manifest["notarization"]["submitted"])
            self.assertTrue(all(b["cdhash"] is None for b in manifest["binaries"]))

            # No dmg, so nothing dmg-shaped is recorded; the .app is still
            # checksummed so the assembly has real provenance (NFR-052).
            self.assertNotIn("dmg_bytes", manifest["sizes"])
            self.assertFalse(any(k.endswith(".dmg") for k in manifest["checksums"]))
            self.assertTrue(any(k.startswith("GramDrive.app/") for k in manifest["checksums"]))

    def test_unsigned_and_notarize_cannot_combine(self):
        # Assembling without a signature and notarizing a signature are
        # contradictory; the CLI refuses the combination up front.
        with self.assertRaises(SystemExit):
            app.main(["--unsigned", "--notarize"])

    def test_notarize_submits_staples_and_records_the_submission(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, calls, _ = self.run_pipeline(Path(tmp), notarize=True)
            self.assertTrue(manifest["notarization"]["submitted"])
            self.assertEqual(manifest["notarization"]["id"], "sub-123")
            self.assertEqual(manifest["notarization"]["status"], "Accepted")
            self.assertTrue(any("notarytool submit" in " ".join(c) for c in calls))
            self.assertTrue(any(c[:2] == ("xcrun", "stapler") for c in calls))
            # Both the app and the dmg are notarized+stapled, recorded per target.
            self.assertEqual(manifest["notarization"]["app"]["target"], "app")
            self.assertEqual(manifest["notarization"]["dmg"]["target"], "dmg")
            self.assertEqual(
                manifest["signature_verification"],
                {"app": "passed", "dmg": "passed", "nested": "passed"},
            )
            self.assertEqual(
                manifest["staple_verification"],
                {"app": "validated", "dmg": "validated"},
            )
            self.assertEqual(manifest["gatekeeper"]["app"], "accepted")
            self.assertEqual(manifest["gatekeeper"]["dmg"], "accepted")
            shipped = manifest["shipped_code_verification"]
            self.assertTrue(shipped["complete"])
            self.assertEqual(shipped["count"], len(shipped["objects"]))
            self.assertGreaterEqual(shipped["count"], 9)
            self.assertTrue(all("arm64" in item["architectures"] for item in shipped["objects"]))
            self.assertTrue(all(item["team_id"] == app.TEAM_ID for item in shipped["objects"]))
            joined = [" ".join(c) for c in calls]
            self.assertTrue(any(j.startswith("codesign --verify --strict") and j.endswith(".dmg") for j in joined))
            self.assertTrue(any("stapler validate" in j and j.endswith("GramDrive.app") for j in joined))
            self.assertTrue(any("stapler validate" in j and j.endswith(".dmg") for j in joined))
            self.assertTrue(any("spctl --assess --type open" in j and j.endswith(".dmg") for j in joined))

    def test_wrong_nested_architecture_fails_post_embedding_readback(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(app.StepFailed, "lacks required arm64 architecture"):
                self.run_pipeline(Path(tmp), notarize=True, nested_arch="x86_64")

    def test_wrong_or_missing_nested_team_id_fails_post_embedding_readback(self):
        for team in ("BADTEAM123", None):
            with self.subTest(team=team), tempfile.TemporaryDirectory() as tmp:
                with self.assertRaisesRegex(app.StepFailed, "TeamIdentifier"):
                    self.run_pipeline(Path(tmp), notarize=True, nested_team=team)

    def test_post_staple_dmg_gatekeeper_rejection_fails_the_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(app.StepFailed, "Gatekeeper rejected the notarized DMG"):
                self.run_pipeline(
                    Path(tmp),
                    notarize=True,
                    extra={"spctl --assess --type open": (3, "rejected")},
                )

    def test_the_app_is_stapled_before_the_dmg_is_built(self):
        # The offline-ticket fix (packaging review 2115): the app must be zipped,
        # notarized and stapled BEFORE hdiutil builds the dmg, so the copy inside
        # the dmg carries a ticket a user can verify offline.
        with tempfile.TemporaryDirectory() as tmp:
            _, calls, _ = self.run_pipeline(Path(tmp), notarize=True)
            joined = [" ".join(c) for c in calls]
            staple_app_idx = next(
                i for i, j in enumerate(joined)
                if j.startswith("xcrun stapler") and j.rstrip().endswith("GramDrive.app")
            )
            hdiutil_idx = next(i for i, j in enumerate(joined) if j.startswith("hdiutil"))
            self.assertLess(staple_app_idx, hdiutil_idx)
            # The app went through a ditto zip container, not submitted bare.
            self.assertTrue(any(j.startswith("ditto") for j in joined))

    def test_notary_keychain_is_passed_to_notarytool_when_set(self):
        # CI stores the gramdrive-notary profile in a throwaway keychain and
        # passes it through so nothing touches the login keychain.
        with tempfile.TemporaryDirectory() as tmp:
            _, calls, _ = self.run_pipeline(
                Path(tmp), notarize=True, notary_keychain=Path("/tmp/ci.keychain-db")
            )
            submits = [c for c in calls if "notarytool" in c and "submit" in c]
            self.assertTrue(submits)
            for c in submits:
                self.assertIn("--keychain", c)
                self.assertIn("/tmp/ci.keychain-db", c)

    def test_notary_keychain_defaults_to_the_login_keychain(self):
        with tempfile.TemporaryDirectory() as tmp:
            _, calls, _ = self.run_pipeline(Path(tmp), notarize=True)
            submits = [c for c in calls if "notarytool" in c and "submit" in c]
            self.assertTrue(submits)
            self.assertFalse(any("--keychain" in c for c in submits))

    def test_notarization_rejection_fails_the_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(app.StepFailed):
                self.run_pipeline(
                    Path(tmp),
                    notarize=True,
                    extra={"notarytool submit": (0, '{"id": "x", "status": "Invalid"}')},
                )

    def test_a_leaked_get_task_allow_fails_verification(self):
        # The signature carries get-task-allow (SwiftPM debug default leaking):
        # the entitlement dump must catch it rather than ship an un-notarizable app.
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(app.StepFailed) as caught:
                self.run_pipeline(Path(tmp), leak_get_task_allow=True)
            self.assertIn("get-task-allow", str(caught.exception))

    def test_missing_core_package_fails_loudly(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            (repo / app.SUPPORT_PACKAGE).mkdir(parents=True)
            out = Path(tmp) / "out"
            out.mkdir()
            run, _ = self.scripted()
            with self.assertRaises(app.StepFailed) as caught:
                app.package(
                    repo,
                    out_dir=out,
                    identity="X",
                    core_package=repo / "missing",
                    runner=run,
                    echo=lambda _: None,
                    environ={"HOME": "/h"},
                )
            self.assertIn("staged core package not found", str(caught.exception))

    def test_checksums_cover_the_dmg_and_the_app(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, out = self.run_pipeline(Path(tmp))
            rendered = (out / "CHECKSUMS.sha256").read_text()
            self.assertIn("GramDrive-0.5.0.dmg", rendered)
            self.assertTrue(any(name.startswith("GramDrive.app/") for name in manifest["checksums"]))

    def test_manifest_binds_pre_sign_tdlib_to_final_signed_bytes(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, _ = self.run_pipeline(Path(tmp), notarize=True)
            transition = manifest["tdjson"]["signing_transition"]
            self.assertTrue(transition["required"])
            self.assertEqual(
                transition["source"]["sha256"],
                transition["pre_sign"]["sha256"],
            )
            self.assertEqual(
                transition["post_sign"]["sha256"],
                manifest["checksums"][
                    "GramDrive.app/Contents/Frameworks/libtdjson.dylib"
                ],
            )
            self.assertEqual(
                transition["signature"],
                {
                    "verified": True,
                    "team_id": app.TEAM_ID,
                    "authority": "Developer ID Application: Test (262RZ595FP)",
                    "architecture": "arm64",
                },
            )

    def test_pre_sign_tdlib_tamper_is_rejected_before_codesign(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo, core = self.stage(Path(tmp))
            out = Path(tmp) / "out"
            out.mkdir()
            run, calls = self.scripted()
            packager = app.AppPackager(
                repo,
                out,
                identity="Developer ID Application: Test (262RZ595FP)",
                core_package=core,
                runner=run,
                echo=lambda _: None,
                environ={},
            )
            app_bundle = self._assembled_runtime_app_for_transition(Path(tmp), core)
            embedded = app_bundle / "Contents" / "Frameworks" / "libtdjson.dylib"
            embedded.write_bytes(b"tampered before signing")
            with self.assertRaisesRegex(app.StepFailed, "immediately before signing"):
                packager.begin_tdjson_signing_transition(
                    app_bundle, ["libtdjson.dylib"]
                )
            self.assertFalse(any(call and call[0] == "codesign" for call in calls))

    def _assembled_runtime_app_for_transition(self, tmp: Path, core: Path) -> Path:
        app_bundle = tmp / "transition.app"
        embedded = app_bundle / "Contents" / "Frameworks" / "libtdjson.dylib"
        embedded.parent.mkdir(parents=True)
        embedded.write_bytes((core / "lib" / "libtdjson.dylib").read_bytes())
        return app_bundle

    def test_reproducibility_claim_is_honest(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest, _, _ = self.run_pipeline(Path(tmp))
            # A signed artifact is attributable, not byte-identical.
            self.assertFalse(manifest["reproducible"]["byte_identical"])
            self.assertTrue(manifest["reproducible"]["attributable"])


class RuntimeEmbeddingTest(unittest.TestCase):
    """The tdjson-linked packaging path — core_tdjson_linked,
    embed_runtime_libraries, its portability assertion, and the Frameworks
    signing loop — which the default (hermetic-core) PipelineTest never reaches.
    """

    def _core(self, base: Path, *, linked: bool) -> Path:
        base.mkdir(parents=True, exist_ok=True)
        core = base / "core"
        (core / "lib").mkdir(parents=True)
        (core / "lib" / "libtdjson.dylib").write_bytes(b"\xcf\xfa\xed\xfe dylib")
        manifest = {"contract_version": "0.5.0"}
        if linked:
            manifest["tdjson"] = {"linked": True}
            stage_openssl_attribution(core, manifest)
        (core / "gramdrive-core-manifest.json").write_text(json.dumps(manifest))
        return core

    def _packager(self, base: Path, run):
        repo = base / "repo"
        repo.mkdir(parents=True, exist_ok=True)
        out = base / "out"
        out.mkdir(parents=True, exist_ok=True)
        return app.AppPackager(
            repo,
            out,
            identity="Developer ID Application: T (262RZ595FP)",
            core_package=base / "core",
            runner=run,
            echo=lambda _: None,
            environ={},
        )

    def _app(self, base: Path) -> Path:
        appdir = base / "GramDrive.app"
        (appdir / "Contents" / "MacOS").mkdir(parents=True)
        (appdir / "Contents" / "PlugIns" / app.APPEX_BUNDLE_NAME / "Contents" / "MacOS").mkdir(
            parents=True
        )
        return appdir

    def otool_runner(self, deps_for):
        """A runner that answers `otool -L` from `deps_for(target)` and no-ops
        install_name_tool, so an embed can run without Mach-O tooling."""

        def run(argv, cwd, env=None):
            argv = tuple(str(a) for a in argv)
            if argv[:2] == ("otool", "-L"):
                target = argv[2]
                lines = [f"{target}:"]
                for dep in deps_for(target):
                    lines.append(f"\t{dep} (compatibility version 1.0.0, current version 1.0.0)")
                return 0, "\n".join(lines) + "\n"
            return 0, ""

        return run

    def test_core_tdjson_linked_reads_the_manifest_flag(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            self._core(tmp / "linked", linked=True)
            self._core(tmp / "hermetic", linked=False)
            run = self.otool_runner(lambda _t: [])
            self.assertTrue(self._packager(tmp / "linked", run).core_tdjson_linked())
            self.assertFalse(self._packager(tmp / "hermetic", run).core_tdjson_linked())

    def test_hermetic_core_skips_embedding(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            self._core(tmp, linked=False)
            pk = self._packager(tmp, self.otool_runner(lambda _t: []))
            appdir = self._app(tmp)
            self.assertEqual(pk.embed_runtime_libraries(appdir), [])
            self.assertFalse((appdir / "Contents" / "Frameworks").exists())

    def test_live_core_rejects_tampered_openssl_license_before_signing(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            core = self._core(tmp, linked=True)
            (core / app.OPENSSL_LICENSE_PATH).write_text("tampered\n")
            pk = self._packager(tmp, self.otool_runner(lambda _target: []))
            with self.assertRaisesRegex(app.StepFailed, "OpenSSL attribution"):
                pk.embed_third_party_attribution(self._app(tmp))

    def test_embed_rewrites_to_rpath_and_passes_the_assertion(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            self._core(tmp, linked=True)

            def clean_deps(target):
                if target.endswith("libtdjson.dylib"):
                    return ["@rpath/libtdjson.dylib", "/usr/lib/libSystem.B.dylib"]
                return ["@rpath/libtdjson.dylib", "/usr/lib/libSystem.B.dylib"]

            pk = self._packager(tmp, self.otool_runner(clean_deps))
            appdir = self._app(tmp)
            embedded = pk.embed_runtime_libraries(appdir)
            self.assertEqual(embedded, ["libtdjson.dylib"])
            self.assertTrue((appdir / "Contents" / "Frameworks" / "libtdjson.dylib").is_file())

    def test_a_surviving_absolute_reference_fails_the_build(self):
        # The fixup's `-change` silently no-ops when its target no longer
        # matches; reading the shipped bytes back is what catches an absolute
        # staged path that the fixup left behind.
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            core = self._core(tmp, linked=True)
            staged = str(core / "lib" / "libtdjson.dylib")

            def dirty_deps(target):
                if target.endswith("libtdjson.dylib"):
                    return ["/usr/lib/libSystem.B.dylib"]
                if target.endswith("gramdrive-agent"):
                    return [staged, "/usr/lib/libSystem.B.dylib"]
                return ["@rpath/libtdjson.dylib", "/usr/lib/libSystem.B.dylib"]

            pk = self._packager(tmp, self.otool_runner(dirty_deps))
            appdir = self._app(tmp)
            with self.assertRaises(app.StepFailed) as caught:
                pk.embed_runtime_libraries(appdir)
            self.assertIn("gramdrive-agent", str(caught.exception))

    def test_a_homebrew_reference_also_fails_the_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            self._core(tmp, linked=True)

            def brew_dep(target):
                if target.endswith("libtdjson.dylib"):
                    return ["/usr/lib/libSystem.B.dylib"]
                if target.endswith("GramDriveFileProvider"):
                    return ["/opt/homebrew/opt/openssl@3/lib/libssl.dylib"]
                return ["@rpath/libtdjson.dylib"]

            pk = self._packager(tmp, self.otool_runner(brew_dep))
            appdir = self._app(tmp)
            with self.assertRaises(app.StepFailed) as caught:
                pk.embed_runtime_libraries(appdir)
            self.assertIn("homebrew", str(caught.exception).lower())

    def test_compiled_homebrew_default_fails_with_clean_load_commands(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            core = self._core(tmp, linked=True)
            (core / "lib" / "libtdjson.dylib").write_bytes(
                b"Mach-O\0/opt/homebrew/etc/openssl@3/cert.pem\0"
            )
            pk = self._packager(
                tmp,
                self.otool_runner(lambda _target: ["/usr/lib/libSystem.B.dylib"]),
            )
            with self.assertRaisesRegex(app.StepFailed, "builder-local"):
                pk.embed_runtime_libraries(self._app(tmp))

    def test_staged_tdjson_rejects_dynamic_openssl_before_embedding(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            self._core(tmp, linked=True)

            def openssl_dep(target):
                if target.endswith("libtdjson.dylib"):
                    return ["@rpath/libssl.3.dylib", "/usr/lib/libSystem.B.dylib"]
                return ["@rpath/libtdjson.dylib"]

            pk = self._packager(tmp, self.otool_runner(openssl_dep))
            with self.assertRaisesRegex(app.StepFailed, "OpenSSL must be static"):
                pk.embed_runtime_libraries(self._app(tmp))

    def test_staged_tdjson_rejects_any_builder_absolute_dependency(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            self._core(tmp, linked=True)

            def builder_dep(target):
                if target.endswith("libtdjson.dylib"):
                    return ["/Users/builder/local/libcustom.dylib"]
                return ["@rpath/libtdjson.dylib"]

            pk = self._packager(tmp, self.otool_runner(builder_dep))
            with self.assertRaisesRegex(app.StepFailed, "non-portable"):
                pk.embed_runtime_libraries(self._app(tmp))

    def test_frameworks_dylibs_are_signed_before_the_binaries(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            self._core(tmp, linked=True)
            calls: list[tuple[str, ...]] = []

            def run(argv, cwd, env=None):
                argv = tuple(str(a) for a in argv)
                calls.append(argv)
                return 0, ""

            pk = self._packager(tmp, run)
            appdir = self._app(tmp)
            frameworks = appdir / "Contents" / "Frameworks"
            frameworks.mkdir(parents=True)
            (frameworks / "libtdjson.dylib").write_bytes(b"\xcf\xfa\xed\xfe")
            entitlements = pk.write_entitlement_files()
            pk.sign(appdir, entitlements, timestamp=False)
            codesigns = [
                c[-1] for c in calls if c and c[0] == "codesign" and "--force" in c and "-d" not in c
            ]
            dylib_idx = next(i for i, t in enumerate(codesigns) if t.endswith("libtdjson.dylib"))
            agent_idx = next(i for i, t in enumerate(codesigns) if t.endswith("gramdrive-agent"))
            app_idx = next(i for i, t in enumerate(codesigns) if t.endswith("GramDrive.app"))
            self.assertLess(dylib_idx, agent_idx)
            self.assertLess(dylib_idx, app_idx)


class PlatformGuardTest(unittest.TestCase):
    def test_non_macos_cannot_start(self):
        # POL-5: the app artifact is macOS-only; a non-Apple host must exit 2
        # with the reason, not produce a partial artifact.
        original = app.sys.platform
        try:
            app.sys.platform = "linux"
            with contextlib.redirect_stderr(io.StringIO()):
                exit_code = app.main(["--repo-root", str(REPO_ROOT)])
            self.assertEqual(exit_code, app.EXIT_CANNOT_START)
        finally:
            app.sys.platform = original


if __name__ == "__main__":
    unittest.main()
